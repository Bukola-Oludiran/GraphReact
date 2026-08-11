//! Key → state-transition mapping. Pure-function shape so tests can
//! drive transitions without crossterm in the loop.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::state::{ObserverState, Pane};

/// Outcome of one key press. The main loop uses `Quit` to break out
/// of the render-input-poll select; everything else continues.
#[derive(Debug, PartialEq, Eq)]
pub enum InputAction {
    Continue,
    Quit,
}

/// Apply one [`KeyEvent`] to `state`, returning the dispatch outcome.
/// Pass-1 keybinds:
/// - `q` / Ctrl-C: quit.
/// - `Tab`: cycle pane forward.
/// - `j`, Down: move Nodes-pane cursor down.
/// - `k`, Up: move Nodes-pane cursor up.
/// - `Enter`: drill from Nodes → NodeDetail.
/// - `Esc`: pop NodeDetail / drop Trace target.
/// - `t`: start tailing the focused node from NodeDetail.
pub fn handle_key(state: &mut ObserverState, key: KeyEvent) -> InputAction {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return InputAction::Quit;
    }
    match key.code {
        KeyCode::Char('q') => InputAction::Quit,
        KeyCode::Tab => {
            state.switch_pane();
            InputAction::Continue
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if matches!(state.pane, Pane::Nodes) {
                state.move_nodes_cursor(1);
            }
            InputAction::Continue
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if matches!(state.pane, Pane::Nodes) {
                state.move_nodes_cursor(-1);
            }
            InputAction::Continue
        }
        KeyCode::Enter => {
            if matches!(state.pane, Pane::Nodes) {
                state.drill_into_selected();
            }
            InputAction::Continue
        }
        KeyCode::Esc => {
            match &state.pane {
                Pane::NodeDetail { .. } => state.pop_to_nodes(),
                Pane::Trace { node_id: Some(_) } => state.stop_trace(),
                _ => {}
            }
            InputAction::Continue
        }
        KeyCode::Char('t') => {
            state.start_trace_for_focused();
            InputAction::Continue
        }
        _ => InputAction::Continue,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    use super::super::state::{Identity, NodeRow, ObserverState, Pane};
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn seeded_state() -> ObserverState {
        let mut s = ObserverState::new("x".into());
        s.apply_poll(
            Identity {
                process_start_id: 1,
                version: "v".into(),
                api_version: 1,
            },
            vec![
                NodeRow {
                    id: "a".into(),
                    role: "x".into(),
                    depth: 0,
                    supervisor: None,
                },
                NodeRow {
                    id: "b".into(),
                    role: "x".into(),
                    depth: 0,
                    supervisor: None,
                },
            ],
            vec![],
            HashMap::new(),
            1_000,
        );
        s.pane = Pane::Nodes;
        s
    }

    #[test]
    fn q_quits() {
        let mut s = seeded_state();
        assert_eq!(
            handle_key(&mut s, key(KeyCode::Char('q'))),
            InputAction::Quit
        );
    }

    #[test]
    fn ctrl_c_quits() {
        let mut s = seeded_state();
        assert_eq!(handle_key(&mut s, ctrl_c()), InputAction::Quit);
    }

    #[test]
    fn tab_cycles_pane() {
        let mut s = seeded_state();
        s.pane = Pane::Overview;
        handle_key(&mut s, key(KeyCode::Tab));
        assert_eq!(s.pane, Pane::Tree);
    }

    #[test]
    fn j_moves_nodes_cursor_down() {
        let mut s = seeded_state();
        assert_eq!(s.nodes_cursor, 0);
        handle_key(&mut s, key(KeyCode::Char('j')));
        assert_eq!(s.nodes_cursor, 1);
    }

    #[test]
    fn k_moves_nodes_cursor_up() {
        let mut s = seeded_state();
        s.nodes_cursor = 1;
        handle_key(&mut s, key(KeyCode::Char('k')));
        assert_eq!(s.nodes_cursor, 0);
    }

    #[test]
    fn enter_drills_from_nodes() {
        let mut s = seeded_state();
        s.move_nodes_cursor(1);
        handle_key(&mut s, key(KeyCode::Enter));
        assert_eq!(
            s.pane,
            Pane::NodeDetail {
                node_id: "b".into()
            }
        );
    }

    #[test]
    fn esc_returns_from_node_detail() {
        let mut s = seeded_state();
        s.pane = Pane::NodeDetail {
            node_id: "a".into(),
        };
        handle_key(&mut s, key(KeyCode::Esc));
        assert_eq!(s.pane, Pane::Nodes);
    }

    #[test]
    fn t_starts_trace_from_node_detail() {
        let mut s = seeded_state();
        s.pane = Pane::NodeDetail {
            node_id: "a".into(),
        };
        handle_key(&mut s, key(KeyCode::Char('t')));
        assert_eq!(
            s.pane,
            Pane::Trace {
                node_id: Some("a".into())
            }
        );
    }

    #[test]
    fn esc_drops_trace_target() {
        let mut s = seeded_state();
        s.pane = Pane::Trace {
            node_id: Some("a".into()),
        };
        handle_key(&mut s, key(KeyCode::Esc));
        assert_eq!(s.pane, Pane::Trace { node_id: None });
    }
}
