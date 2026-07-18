//! **M16-Backend P6 verification gate.** End-to-end smoke test of
//! the `grx observer` data plumbing — without spinning up a real
//! terminal. Stages a fixture management API (P4), drives the P6
//! `fetch_round` helper against it, applies the result to
//! `ObserverState`, and renders the Overview pane via
//! [`ratatui::backend::TestBackend`] to assert the data shows
//! correctly.
//!
//! This is the headless equivalent of the manual acceptance gate
//! ("SSH to host, run `grx observer --target …`, render live UI").
//! The reconnect path is covered by a second test that swaps a new
//! daemon with a different `process_start_id` under the polling
//! task and asserts the `[target restarted]` divider appears.

use std::sync::Arc;
use std::time::SystemTime;

use graphreact_core::telemetry::{NodeIntrospection, SupervisorIntrospection};
use graphreact_observer::{
    spawn_management_api, ApiServerHandle, BindConfig, MetricsInner, SnapshotRequest,
};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use graphreact_cli::observer_client::{Target, TlsMode};
use graphreact_cli::observer_tui::client::fetch_round;
use graphreact_cli::observer_tui::panes;
use graphreact_cli::observer_tui::state::{ObserverState, Pane, TraceEntry};

async fn spawn_fixture(process_start_id: u64) -> (ApiServerHandle, JoinHandle<()>) {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let mut server = spawn_management_api(bind, metrics, process_start_id)
        .await
        .expect("spawn");
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain_task = tokio::spawn(async move {
        while let Some(req) = snapshot_rx.recv().await {
            match req {
                SnapshotRequest::Nodes(tx) => {
                    let _ = tx.send(vec![
                        NodeIntrospection {
                            id: "logger".into(),
                            role: "sink".into(),
                            depth: 0,
                            path: vec!["logger".into()],
                            supervisor: Some("sup:0".into()),
                        },
                        NodeIntrospection {
                            id: "sensor-1".into(),
                            role: "sensor".into(),
                            depth: 1,
                            path: vec!["sensor-1".into()],
                            supervisor: Some("sup:0".into()),
                        },
                        NodeIntrospection {
                            id: "sensor-2".into(),
                            role: "sensor".into(),
                            depth: 1,
                            path: vec!["sensor-2".into()],
                            supervisor: Some("sup:0".into()),
                        },
                    ]);
                }
                SnapshotRequest::NodeDetail(_, tx) => {
                    let _ = tx.send(None);
                }
                SnapshotRequest::SupervisorTree(tx) => {
                    let _ = tx.send(vec![SupervisorIntrospection {
                        key: "sup:0".into(),
                        strategy: "OneForOne".into(),
                        max_restarts: 3,
                        period_secs: 5,
                        restart_count: 0,
                        children: vec![
                            "logger".into(),
                            "sensor-1".into(),
                            "sensor-2".into(),
                        ],
                    }]);
                }
                // **M-Ingress.** TUI smoke test does not exercise inject /
                // call / bulk; dedicated coverage is in
                // observer/tests/{inject,call,bulk}_e2e.rs.
                SnapshotRequest::InjectMessage { .. }
                | SnapshotRequest::InjectMessageWithReply { .. }
                | SnapshotRequest::InjectMessageBulkLine { .. }
                | SnapshotRequest::DelegatedRestart { .. } => {}
            }
        }
    });
    (server, drain_task)
}

fn dump_buffer(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn fetch_round_populates_state_and_overview_renders() {
    let (server, _drain) = spawn_fixture(0xCAFE_BABE).await;
    let target_str = format!("{}", server.local_addr);
    let target = Target::parse(&target_str).expect("parse");

    let round = fetch_round(&target, None, TlsMode::Native, None)
        .await
        .expect("fetch ok");
    assert_eq!(round.identity.process_start_id, 0xCAFE_BABE);
    assert_eq!(round.nodes.len(), 3);
    assert_eq!(round.supervisors.len(), 1);

    let mut state = ObserverState::new(target_str.clone());
    state.apply_poll(
        round.identity,
        round.nodes,
        round.supervisors,
        round.recent_messages,
        unix_ms(),
    );

    // Render Overview.
    let backend = TestBackend::new(96, 24);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| panes::render(f, &state)).unwrap();
    let dump = dump_buffer(&term);
    assert!(dump.contains("nodes:        3"), "buffer: {dump}");
    assert!(dump.contains("supervisors:  1"), "buffer: {dump}");
    assert!(dump.contains("connected"), "buffer: {dump}");

    // Render Tree pane to confirm the supervisor + children show.
    state.pane = Pane::Tree;
    term.draw(|f| panes::render(f, &state)).unwrap();
    let dump = dump_buffer(&term);
    assert!(dump.contains("sup:0 [OneForOne]"), "buffer: {dump}");
    assert!(dump.contains("sensor-1"), "buffer: {dump}");

    server.shutdown().await;
}

#[tokio::test]
async fn process_start_id_change_pushes_divider_through_polling() {
    // Initial daemon at pid=11.
    let (server_a, _drain_a) = spawn_fixture(11).await;
    let target_str = format!("{}", server_a.local_addr);
    let target = Target::parse(&target_str).expect("parse");

    let round_a = fetch_round(&target, None, TlsMode::Native, None)
        .await
        .expect("fetch a");
    let mut state = ObserverState::new(target_str.clone());
    state.apply_poll(
        round_a.identity,
        round_a.nodes,
        round_a.supervisors,
        round_a.recent_messages,
        unix_ms(),
    );
    assert!(state.trace_log.is_empty(), "first poll has no divider");

    server_a.shutdown().await;

    // Spawn a fresh daemon at the same loopback host but a NEW
    // pid — operators see this when the daemon restarts under
    // them. The polling task fetches against the new daemon, the
    // identity-change trips the divider.
    let (server_b, _drain_b) = spawn_fixture(22).await;
    let target_b_str = format!("{}", server_b.local_addr);
    let target_b = Target::parse(&target_b_str).expect("parse");
    let round_b = fetch_round(&target_b, None, TlsMode::Native, None)
        .await
        .expect("fetch b");
    state.apply_poll(
        round_b.identity,
        round_b.nodes,
        round_b.supervisors,
        round_b.recent_messages,
        unix_ms(),
    );

    let last = state.trace_log.back().expect("divider");
    assert!(
        matches!(last, TraceEntry::TargetRestarted { .. }),
        "expected divider, got {last:?}"
    );

    // Render Trace pane to confirm the divider is visible.
    state.pane = Pane::Trace { node_id: None };
    let backend = TestBackend::new(96, 24);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| panes::render(f, &state)).unwrap();
    let dump = dump_buffer(&term);
    assert!(dump.contains("target restarted"), "buffer: {dump}");

    server_b.shutdown().await;
}

#[tokio::test]
async fn reconnect_keeps_last_known_state_visible() {
    let (server, _drain) = spawn_fixture(99).await;
    let target_str = format!("{}", server.local_addr);
    let target = Target::parse(&target_str).expect("parse");
    let round = fetch_round(&target, None, TlsMode::Native, None)
        .await
        .expect("fetch ok");

    let mut state = ObserverState::new(target_str.clone());
    state.apply_poll(
        round.identity,
        round.nodes,
        round.supervisors,
        round.recent_messages,
        unix_ms(),
    );
    server.shutdown().await;

    // Simulate connection loss — the polling task would record a
    // disconnect and the render loop continues to display the
    // last-known nodes / supervisors plus the Disconnected status.
    state.record_disconnect("connect refused".into(), 1_000);

    let backend = TestBackend::new(96, 24);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| panes::render(f, &state)).unwrap();
    let dump = dump_buffer(&term);
    assert!(dump.contains("disconnected"), "buffer: {dump}");
    assert!(dump.contains("connect refused"), "buffer: {dump}");
    assert!(dump.contains("nodes:        3"), "last-known still shown: {dump}");
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
