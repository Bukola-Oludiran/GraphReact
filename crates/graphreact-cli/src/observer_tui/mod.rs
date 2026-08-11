//! **M16-Backend Phase 6.** `grx observer` ratatui terminal UI.
//!
//! Architecture:
//! 1. Polling task ([`polling::run_poller`]) fetches `/v1/identity` +
//!    `/v1/nodes` + `/v1/supervisor/tree` (and the per-node ring when
//!    a Trace target is set) every 100ms, pushing the assembled
//!    [`state::ObserverState`] through a [`tokio::sync::watch`].
//! 2. Render+input task drives ratatui synchronously between
//!    crossterm `EventStream` events; reads the latest state on every
//!    frame.
//! 3. [`TerminalGuard`] owns raw-mode + alternate-screen entry and
//!    restores on `Drop` so a panic doesn't leave the user's terminal
//!    corrupted.
//!
//! Pass-1 ships polling-only (no WebSocket; that's P7) and renders
//! data correctly via the pane state machine. Pass-2 polishes the
//! layout (column widths, color highlighting, pane-specific
//! filtering) as a documented follow-up.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::watch;
use tokio::time::interval;

use crate::cli::TlsObserverTarget;
use crate::commands::CommandError;
use crate::observer_client::{self, Target, TlsMode};

pub mod client;
pub mod input;
pub mod panes;
pub mod polling;
pub mod reconnect;
pub mod state;
pub mod ws_client;

use input::{handle_key, InputAction};
use polling::{run_poller, PollerCtx};
use state::{ObserverState, Pane};
use ws_client::{run_ws_task, WsCtx};

/// Public entry point — invoked by `grx observer`'s dispatch arm in
/// `bin/grx.rs`. Owns the tokio task graph for one session; returns
/// when the user quits with `q` / Ctrl-C / Esc-from-Overview.
pub async fn run(target: TlsObserverTarget) -> Result<(), CommandError> {
    let parsed = Target::parse(&target.target)
        .map_err(|e| CommandError::ClusterArg(format!("--target: {e}")))?;
    let token =
        observer_client::resolve_token(target.token_env.as_deref(), target.token_file.as_deref())
            .map_err(|e| CommandError::ClusterArg(format!("token: {e}")))?;
    let tls_mode = if target.insecure_no_tls {
        TlsMode::InsecureSkipVerify
    } else {
        TlsMode::Native
    };

    let target_label = format_target_label(&parsed, tls_mode);
    let initial_state = ObserverState::new(target_label.clone());

    let (state_tx, state_rx) = watch::channel(initial_state);
    let (focused_tx, focused_rx) = watch::channel::<Option<String>>(None);
    let (shutdown_tx, shutdown_rx) = watch::channel::<bool>(false);

    let ctx = Arc::new(PollerCtx {
        target: parsed.clone(),
        token: token.clone(),
        tls: tls_mode,
    });
    let poller_handle = tokio::spawn(run_poller(
        ctx,
        state_tx.clone(),
        focused_rx,
        shutdown_rx.clone(),
    ));

    // M16-Backend P7 — WebSocket task subscribes to the daemon's
    // /v1/observer/events live event stream and feeds
    // MessageDelivered events into ObserverState.recent_messages +
    // the trace tape. Coexists with polling: polling keeps
    // nodes/supervisors/identity fresh on a 10 Hz cadence; WS
    // delivers per-message granularity that polling can't reach.
    // On WS disconnect, the polling task continues to drive the
    // UI; the reconnect schedule re-enters WS with exponential
    // backoff.
    let ws_ctx = Arc::new(WsCtx {
        target: parsed,
        token,
        tls: tls_mode,
    });
    let ws_handle = tokio::spawn(run_ws_task(ws_ctx, state_tx.clone(), shutdown_rx.clone()));

    let render_result = render_loop(state_tx, state_rx, focused_tx, shutdown_tx).await;
    let _ = poller_handle.await;
    let _ = ws_handle.await;
    render_result
}

async fn render_loop(
    state_tx: watch::Sender<ObserverState>,
    mut state_rx: watch::Receiver<ObserverState>,
    focused_tx: watch::Sender<Option<String>>,
    shutdown_tx: watch::Sender<bool>,
) -> Result<(), CommandError> {
    let _guard =
        TerminalGuard::enter().map_err(|e| CommandError::Io(io::Error::other(e.to_string())))?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|e| CommandError::Io(io::Error::other(e.to_string())))?;

    let mut events = EventStream::new();
    // ratatui re-render cadence — the polling task drives state
    // updates at 10 Hz; this tick keeps the status-bar age counter
    // ticking visibly between polls.
    let mut render_tick = interval(Duration::from_millis(250));
    render_tick.tick().await; // skip immediate

    let mut last_focused: Option<String> = None;
    loop {
        // Forward focused-trace-node changes to the polling task.
        let current_focused = match &state_rx.borrow().pane {
            Pane::Trace { node_id } => node_id.clone(),
            _ => None,
        };
        if current_focused != last_focused {
            let _ = focused_tx.send(current_focused.clone());
            last_focused = current_focused;
        }

        terminal
            .draw(|f| panes::render(f, &state_rx.borrow()))
            .map_err(|e| CommandError::Io(io::Error::other(e.to_string())))?;

        tokio::select! {
            biased;
            event_opt = events.next() => {
                match event_opt {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        let mut action = InputAction::Continue;
                        state_tx.send_modify(|s| { action = handle_key(s, key); });
                        if matches!(action, InputAction::Quit) {
                            let _ = shutdown_tx.send(true);
                            return Ok(());
                        }
                    }
                    Some(Ok(_)) => {} // mouse / resize / paste — ignored in pass-1
                    Some(Err(e)) => {
                        let _ = shutdown_tx.send(true);
                        return Err(CommandError::Io(io::Error::other(e.to_string())));
                    }
                    None => {
                        // event stream exhausted (terminal closed)
                        let _ = shutdown_tx.send(true);
                        return Ok(());
                    }
                }
            }
            _ = state_rx.changed() => {}
            _ = render_tick.tick() => {}
        }
    }
}

fn format_target_label(target: &Target, tls: TlsMode) -> String {
    let scheme = if target.tls { "https" } else { "http" };
    let suffix = match tls {
        TlsMode::Native => "",
        TlsMode::InsecureSkipVerify => " (no-verify)",
    };
    format!("{}://{}:{}{}", scheme, target.host, target.port, suffix)
}

/// RAII guard around raw-mode + alternate-screen enter/exit.
/// Restores the terminal on Drop even under panic — without this,
/// crashes inside the render loop leave the user's shell in raw
/// mode with the alternate screen still active.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

// Suppress unused import warning when the type is referenced only
// inside the (non-test) main body. Some environments compile both
// the lib + the test target together; keep the alias explicit.
#[allow(dead_code)]
type _StdoutBackend = CrosstermBackend<Stdout>;
