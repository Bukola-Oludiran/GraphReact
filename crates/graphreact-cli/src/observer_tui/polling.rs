//! Polling task for `grx observer`. Fetches a poll round every
//! `POLL_INTERVAL` (10 Hz when connected); on error, falls into the
//! reconnect loop with exponential backoff up to 30s. Identity-change
//! detection happens inside [`super::state::ObserverState::apply_poll`].

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::watch;

use crate::observer_client::{ObserverClientError, Target, TlsMode};

use super::client::fetch_round;
use super::reconnect::Backoff;
use super::state::ObserverState;

/// 10 Hz poll cadence on the steady-state path. The plan locks this
/// in; tightening it costs scheduler-side scrape latency without
/// buying display fidelity (ratatui renders at ~16 Hz so any extra
/// poll round just lands behind the next frame anyway).
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Inputs the polling task carries — fixed for the duration of a
/// `grx observer` session.
pub struct PollerCtx {
    pub target: Target,
    pub token: Option<String>,
    pub tls: TlsMode,
}

/// Run the polling task forever. Returns `()` on graceful shutdown
/// (the watch receivers all dropping signals end-of-life). Mutations
/// land on `state_tx` which fans out to the render loop.
///
/// `focused_rx` carries the currently-focused trace node — when the
/// user picks one in the Trace pane, the polling task starts pulling
/// `/v1/nodes/:id/messages/recent` for that id on every round.
pub async fn run_poller(
    ctx: Arc<PollerCtx>,
    state_tx: watch::Sender<ObserverState>,
    mut focused_rx: watch::Receiver<Option<String>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Backoff::new();
    loop {
        if *shutdown.borrow() {
            return;
        }

        let focused = focused_rx.borrow_and_update().clone();
        let result = fetch_round(
            &ctx.target,
            ctx.token.as_deref(),
            ctx.tls,
            focused.as_deref(),
        )
        .await;

        match result {
            Ok(round) => {
                let now_ms = unix_ms();
                state_tx.send_modify(|s| {
                    s.apply_poll(
                        round.identity,
                        round.nodes,
                        round.supervisors,
                        round.recent_messages,
                        now_ms,
                    );
                });
                backoff.reset();
                tokio::select! {
                    () = tokio::time::sleep(POLL_INTERVAL) => {}
                    _ = shutdown.changed() => return,
                    _ = focused_rx.changed() => {}
                }
            }
            Err(err) => {
                let reason = render_error(&err);
                let retry_ms = backoff.current_ms();
                state_tx.send_modify(|s| s.record_disconnect(reason, retry_ms));
                let delay = backoff.current();
                backoff.bump();
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    _ = shutdown.changed() => return,
                }
            }
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn render_error(err: &ObserverClientError) -> String {
    match err {
        ObserverClientError::Connect { addr, detail } => {
            format!("connect {addr}: {detail}")
        }
        ObserverClientError::Status { status, .. } => format!("HTTP {status}"),
        ObserverClientError::Timeout => "timeout".to_string(),
        ObserverClientError::Tls(s) => format!("tls: {s}"),
        ObserverClientError::TokenLoad(s) => format!("token: {s}"),
        ObserverClientError::InvalidUrl(s) => format!("url: {s}"),
        other => other.to_string(),
    }
}
