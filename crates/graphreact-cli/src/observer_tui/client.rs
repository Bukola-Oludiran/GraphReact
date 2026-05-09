//! Per-poll-round fetch over the management API. Wraps the P5
//! [`crate::observer_client`] helper with concurrent
//! identity / nodes / supervisor fetches via `futures::join!`, plus
//! an opt-in per-node `messages/recent` fetch when the user is
//! tailing one node.

use std::collections::HashMap;

use crate::observer_client::{self, ObserverClientError, Target, TlsMode};

use super::state::{Identity, MessageRow, NodeRow, SupervisorRow};

/// One poll round's worth of decoded payloads.
#[derive(Debug, Clone)]
pub struct PollRound {
    pub identity: Identity,
    pub nodes: Vec<NodeRow>,
    pub supervisors: Vec<SupervisorRow>,
    pub recent_messages: HashMap<String, Vec<MessageRow>>,
}

/// Issue one round of GETs to the management API. The `focused_node`
/// arg drives the optional `messages/recent` fetch — pass `None` when
/// no Trace pane is active to skip the round trip.
///
/// Errors map to a single `ObserverClientError`; the polling task's
/// reconnect loop interprets them all the same way.
pub async fn fetch_round(
    target: &Target,
    token: Option<&str>,
    tls: TlsMode,
    focused_node: Option<&str>,
) -> Result<PollRound, ObserverClientError> {
    let id_fut = observer_client::get_with_tls(target, "/v1/identity", token, tls);
    let nodes_fut = observer_client::get_with_tls(target, "/v1/nodes", token, tls);
    let sup_fut = observer_client::get_with_tls(target, "/v1/supervisor/tree", token, tls);

    let (id_res, nodes_res, sup_res) = futures::join!(id_fut, nodes_fut, sup_fut);

    let identity: Identity =
        serde_json::from_str(&id_res?).map_err(|e| decode_err("identity", &e))?;
    let nodes: Vec<NodeRow> =
        serde_json::from_str(&nodes_res?).map_err(|e| decode_err("nodes", &e))?;
    let supervisors: Vec<SupervisorRow> =
        serde_json::from_str(&sup_res?).map_err(|e| decode_err("supervisor/tree", &e))?;

    let mut recent_messages = HashMap::new();
    if let Some(id) = focused_node {
        let path = format!("/v1/nodes/{id}/messages/recent?limit=64");
        match observer_client::get_with_tls(target, &path, token, tls).await {
            Ok(body) => {
                let rows: Vec<MessageRow> =
                    serde_json::from_str(&body).map_err(|e| decode_err("messages/recent", &e))?;
                recent_messages.insert(id.to_string(), rows);
            }
            // 404 = the focused node disappeared between polls.
            // Don't fail the whole round; just skip the per-node
            // fetch this time. The user can refocus or wait for
            // it to reappear.
            Err(ObserverClientError::Status { status: 404, .. }) => {}
            Err(other) => return Err(other),
        }
    }

    Ok(PollRound {
        identity,
        nodes,
        supervisors,
        recent_messages,
    })
}

fn decode_err(route: &str, e: &serde_json::Error) -> ObserverClientError {
    ObserverClientError::InvalidUrl(format!("decode `{route}`: {e}"))
}
