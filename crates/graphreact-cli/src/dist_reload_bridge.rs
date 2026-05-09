//! **M15 Phase 10.** Bridge between the `Send + Sync`
//! dist-side `RemoteReloadSink` callback and the `!Send`
//! runtime-thread (which owns the `Rc<RefCell<Engine>>` plus
//! the `&mut Scheduler`). The dist sink runs inside the
//! per-connection async task; the runtime thread runs a
//! current-thread tokio runtime hosted on a dedicated OS
//! thread (mirrors the existing `grx run --watch` plus
//! `grx cluster daemon` flows).
//!
//! ## Shape
//!
//! On `Cluster::start`, the higher layer:
//!
//! 1. Calls [`make_reload_bridge`] to get a paired
//!    `(callback, ReloadCommandReceiver)`.
//! 2. Hands the callback to
//!    [`graphreact_dist::Cluster::install_remote_reload_sink`].
//! 3. Holds the receiver on the runtime thread.
//!
//! Each inbound `Packet::ReloadModule` fires the callback,
//! which clones the payload + sends a [`ReloadCommand`] onto
//! the channel. The runtime thread polls the receiver each
//! tick + applies via the local hot-reload classifier +
//! orchestrator.
//!
//! ## Why not call the orchestrator directly from the sink
//!
//! The orchestrator + `EngineAdapter` impls hold
//! `Rc<RefCell<Engine>>` (per-isolate state preservation) so
//! they aren't `Send`. The dist sink's callback signature is
//! `Fn(...) + Send + Sync` because the inbound dispatch
//! happens on tokio's worker pool. A channel hop is the
//! standard cross-thread bridge for this shape.

use std::sync::Arc;

use graphreact_dist::RemoteReloadCallback;
use tokio::sync::mpsc;

/// One inbound cluster reload, ready for the runtime thread
/// to decode + apply.
///
/// Fields mirror `ReloadModulePacket` minus the `origin`
/// (which is captured separately as the authenticated peer
/// name). The runtime thread postcard-decodes `ir_payload`
/// into a `GrxIr` + runs the classifier + orchestrator.
#[derive(Debug, Clone)]
pub struct ReloadCommand {
    /// Authenticated peer name (e.g. `alice`, not
    /// `alice@127.0.0.1`). Rewritten authoritatively by the
    /// dist receive path.
    pub origin_peer: String,
    /// Origin's canonical path. Logged for ops correlation;
    /// the runtime thread does NOT re-read from local disk.
    pub file_path: String,
    /// SHA-256 over `ir_payload`. Already verified at the
    /// dist receive path (mismatch logs a warning + applies
    /// anyway — origin-authoritative v1 semantics).
    pub ir_hash: [u8; 32],
    /// `serde_json::to_vec(&GrxIr)` — the canonical inner
    /// encoding for cluster-wide reload (matches
    /// `Message.payload`'s outer-postcard / inner-JSON split;
    /// postcard itself can't serialise `AttrMap`'s
    /// `serde_json::Value` field). Decoded by the runtime
    /// thread.
    pub ir_payload: Vec<u8>,
    /// Origin's monotonic per-origin reload counter.
    pub version: u64,
}

/// Receiver half of the bridge. Held by the runtime thread.
pub struct ReloadCommandReceiver {
    rx: mpsc::UnboundedReceiver<ReloadCommand>,
}

impl ReloadCommandReceiver {
    /// Drain any commands queued so far without blocking.
    /// Returns the drained commands in receive order. Use
    /// this from the runtime thread's tick loop.
    pub fn drain(&mut self) -> Vec<ReloadCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    /// Await the next command, blocking the runtime thread
    /// until one arrives. Returns `None` if every sender has
    /// been dropped (i.e. the cluster has shut down).
    pub async fn recv(&mut self) -> Option<ReloadCommand> {
        self.rx.recv().await
    }
}

/// Build a paired callback + receiver. The callback is
/// installed via
/// [`graphreact_dist::Cluster::install_remote_reload_sink`];
/// the receiver lives on the runtime thread.
#[must_use]
pub fn make_reload_bridge() -> (RemoteReloadCallback, ReloadCommandReceiver) {
    let (tx, rx) = mpsc::unbounded_channel::<ReloadCommand>();
    let tx = Arc::new(tx);
    let callback: RemoteReloadCallback = Arc::new(
        move |origin_peer: &str,
              file_path: &str,
              ir_payload: &[u8],
              ir_hash: &[u8; 32],
              version: u64| {
            let cmd = ReloadCommand {
                origin_peer: origin_peer.to_string(),
                file_path: file_path.to_string(),
                ir_hash: *ir_hash,
                ir_payload: ir_payload.to_vec(),
                version,
            };
            match tx.send(cmd) {
                Ok(()) => true,
                Err(_) => {
                    tracing::warn!(
                        target: "graphreact_cli::dist_reload_bridge",
                        origin = origin_peer,
                        version,
                        "reload command channel closed; dropping inbound reload"
                    );
                    false
                }
            }
        },
    );
    (callback, ReloadCommandReceiver { rx })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bridge_round_trips_reload_command() {
        let (callback, mut receiver) = make_reload_bridge();
        let hash = [9u8; 32];
        let payload = vec![1, 2, 3];
        let fired = callback("alice", "/srv/sys.grx", &payload, &hash, 7);
        assert!(fired, "send should succeed while receiver is alive");
        let drained = receiver.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].origin_peer, "alice");
        assert_eq!(drained[0].file_path, "/srv/sys.grx");
        assert_eq!(drained[0].ir_hash, hash);
        assert_eq!(drained[0].ir_payload, payload);
        assert_eq!(drained[0].version, 7);
    }

    #[tokio::test]
    async fn bridge_callback_returns_false_after_receiver_dropped() {
        let (callback, receiver) = make_reload_bridge();
        drop(receiver);
        let fired = callback("alice", "/srv/sys.grx", &[1, 2, 3], &[0u8; 32], 1);
        assert!(!fired, "send should fail once the receiver is dropped");
    }

    #[tokio::test]
    async fn bridge_drain_returns_commands_in_send_order() {
        let (callback, mut receiver) = make_reload_bridge();
        for v in 1u8..=4 {
            assert!(callback(
                "alice",
                "/srv/sys.grx",
                &[v],
                &[0u8; 32],
                u64::from(v),
            ));
        }
        let drained = receiver.drain();
        assert_eq!(drained.len(), 4);
        assert_eq!(
            drained.iter().map(|c| c.version).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
    }
}
