//! **M16-Backend Phase 5.** `grx ps / tree / top / trace` introspection
//! subcommands. Each issues HTTP requests against a running daemon's
//! management API (P4) via the [`crate::observer_client`] helper +
//! renders the response to stdout.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::ObserverTarget;
use crate::commands::CommandError;
use crate::observer_client::{self, ObserverClientError, Target};

/// Echo of `graphreact_core::telemetry::NodeIntrospection` for
/// client-side deserialization. We can't depend on the core crate's
/// type for a CLI client because it's behind a workspace path;
/// duplicating the small struct here keeps the CLI binary's
/// closure self-contained + matches the schema-as-contract spirit
/// of the management API's `/v1/` versioning.
#[derive(Debug, Deserialize, Serialize)]
struct NodeRow {
    id: String,
    role: String,
    depth: u32,
    #[serde(default)]
    supervisor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SupervisorRow {
    key: String,
    strategy: String,
    children: Vec<String>,
    #[serde(default)]
    restart_count: usize,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct MessageRow {
    ts_ms: u64,
    from: String,
    to: String,
    #[serde(rename = "type")]
    message_type: String,
    ttl: u32,
    accepted: bool,
}

/// `grx ps` — flat node list.
pub async fn run_ps(target: ObserverTarget, json: bool) -> Result<(), CommandError> {
    let (tgt, token) = parse_target(&target)?;
    let body = observer_client::get(&tgt, "/v1/nodes", token.as_deref())
        .await
        .map_err(map_err)?;
    let rows: Vec<NodeRow> = serde_json::from_str(&body).map_err(map_serde)?;
    if json {
        for row in &rows {
            println!("{}", serde_json::to_string(row).unwrap_or_default());
        }
    } else {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{:<24} {:<16} {:<6} SUPERVISOR", "ID", "ROLE", "DEPTH");
        for row in &rows {
            let _ = writeln!(
                out,
                "{:<24} {:<16} {:<6} {}",
                row.id,
                row.role,
                row.depth,
                row.supervisor.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

/// `grx tree` — supervisor + child mapping (one-shot).
pub async fn run_tree(target: ObserverTarget, json: bool) -> Result<(), CommandError> {
    let (tgt, token) = parse_target(&target)?;
    let body = observer_client::get(&tgt, "/v1/supervisor/tree", token.as_deref())
        .await
        .map_err(map_err)?;
    if json {
        // Pass-through: emit the daemon's JSON exactly. Operators
        // who want jq filtering get the full SupervisorIntrospection
        // shape (key/strategy/max_restarts/period_secs/restart_count/children).
        println!("{body}");
        return Ok(());
    }
    let rows: Vec<SupervisorRow> = serde_json::from_str(&body).map_err(map_serde)?;
    if rows.is_empty() {
        println!("(no supervisors registered)");
        return Ok(());
    }
    for sup in &rows {
        println!(
            "{} [{}] (restarts {}; children: {})",
            sup.key,
            sup.strategy,
            sup.restart_count,
            sup.children.len()
        );
        if let Some((last, rest)) = sup.children.split_last() {
            for child in rest {
                println!("├── {child}");
            }
            println!("└── {last}");
        }
    }
    Ok(())
}

/// `grx top` — htop-shaped live refresh. Polling cadence
/// `interval` seconds (clamped 1..30).
pub async fn run_top(target: ObserverTarget, interval: u64) -> Result<(), CommandError> {
    let (tgt, token) = parse_target(&target)?;
    let interval = interval.clamp(1, 30);
    let mut tick = tokio::time::interval(Duration::from_secs(interval));
    tick.tick().await; // skip immediate
    loop {
        // ANSI: clear-screen + cursor-home. Operators on terminals
        // that don't support these (rare) see scrolling output —
        // acceptable for P5; P6 ratatui handles capability detection.
        print!("\x1B[2J\x1B[H");
        let identity = observer_client::get(&tgt, "/v1/identity", token.as_deref())
            .await
            .map_err(map_err)?;
        let nodes_body = observer_client::get(&tgt, "/v1/nodes", token.as_deref())
            .await
            .map_err(map_err)?;
        let metrics_body = observer_client::get(&tgt, "/v1/metrics", token.as_deref())
            .await
            .map_err(map_err)?;
        let nodes: Vec<NodeRow> = serde_json::from_str(&nodes_body).map_err(map_serde)?;

        println!(
            "grx observer — target {} — interval {}s",
            tgt.host_port_display(),
            interval
        );
        println!("identity: {}", identity.trim());
        println!();
        println!("nodes ({}):", nodes.len());
        for (shown, row) in nodes.iter().enumerate() {
            if shown >= 24 {
                println!("(... +{} more nodes)", nodes.len() - 24);
                break;
            }
            println!(
                "  {:<20} role={:<12} depth={:<3} sup={}",
                row.id,
                row.role,
                row.depth,
                row.supervisor.as_deref().unwrap_or("-")
            );
        }
        println!();
        // Surface a few queue-depth gauges from the prom metrics
        // body via crude line-grep — saves a JSON round-trip.
        for line in metrics_body.lines() {
            if line.starts_with("grx_queue_depth_") {
                println!("  {line}");
            }
        }

        tokio::select! {
            _ = tick.tick() => {}
            _ = tokio::signal::ctrl_c() => {
                println!();
                return Ok(());
            }
        }
    }
}

/// `grx trace <node-id>` — NDJSON line-mode follower.
pub async fn run_trace(
    target: ObserverTarget,
    node_id: String,
    limit: usize,
    poll_ms: u64,
) -> Result<(), CommandError> {
    let (tgt, token) = parse_target(&target)?;
    let limit = limit.clamp(1, 256);
    let poll = Duration::from_millis(poll_ms.clamp(50, 60_000));
    let path = format!("/v1/nodes/{node_id}/messages/recent?limit={limit}");
    let mut last_ts_ms: u64 = 0;
    let mut tick = tokio::time::interval(poll);
    let mut out = std::io::stdout().lock();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        let body = match observer_client::get(&tgt, &path, token.as_deref()).await {
            Ok(b) => b,
            Err(ObserverClientError::Status { status: 404, .. }) => {
                eprintln!("trace: node `{node_id}` not found (404)");
                return Err(CommandError::ClusterArg(format!(
                    "trace target `{node_id}` not found"
                )));
            }
            Err(e) => return Err(map_err(e)),
        };
        let rows: Vec<MessageRow> = serde_json::from_str(&body).map_err(map_serde)?;
        for row in &rows {
            if row.ts_ms <= last_ts_ms {
                continue; // already seen
            }
            // Emit NDJSON — one row per line.
            let line = serde_json::to_string(row).unwrap_or_default();
            let _ = writeln!(out, "{line}");
        }
        if let Some(latest) = rows.last() {
            last_ts_ms = last_ts_ms.max(latest.ts_ms);
        }
        let _ = out.flush();
    }
}

// ----- helpers -----

fn parse_target(t: &ObserverTarget) -> Result<(Target, Option<String>), CommandError> {
    let target = Target::parse(&t.target).map_err(map_err)?;
    let token_env = t.token_env.as_deref();
    let token_file: Option<&Path> = t.token_file.as_deref();
    let token = observer_client::resolve_token(token_env, token_file).map_err(map_err)?;
    Ok((target, token))
}

fn map_err(e: ObserverClientError) -> CommandError {
    CommandError::ClusterArg(e.to_string())
}

fn map_serde(e: serde_json::Error) -> CommandError {
    CommandError::ClusterArg(format!("response parse failed: {e}"))
}

impl Target {
    fn host_port_display(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{}://{}:{}", scheme, self.host, self.port)
    }
}
