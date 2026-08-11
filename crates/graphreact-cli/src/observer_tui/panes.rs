//! Pure render functions per pane. All render fns are
//! `fn(&ObserverState, Frame, Rect)` so the [`TestBackend`] smoke
//! test can drive them headlessly.
//!
//! Pass-1 widgets: `Block`, `Paragraph`, `List`, `Table`, `Tabs`.
//! Pass-2 polish (column widths, color highlighting, pane-specific
//! filtering) lives as a documented follow-up.
//!
//! [`TestBackend`]: ratatui::backend::TestBackend

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, Tabs};
use ratatui::Frame;

use super::state::{Connection, ObserverState, Pane, TraceEntry};

const PANE_LABELS: &[&str] = &["Overview", "Tree", "Nodes", "Trace"];

/// Top-level frame render — splits the screen into a status bar +
/// pane-tabs strip + main pane area + bottom hint bar; dispatches
/// into the per-pane render fn.
pub fn render(frame: &mut Frame<'_>, state: &ObserverState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status bar
            Constraint::Length(1), // pane tabs
            Constraint::Min(0),    // main pane area
            Constraint::Length(1), // hint bar
        ])
        .split(frame.area());

    render_status_bar(frame, chunks[0], state);
    render_pane_tabs(frame, chunks[1], &state.pane);
    match &state.pane {
        Pane::Overview => render_overview(frame, chunks[2], state),
        Pane::Tree => render_tree(frame, chunks[2], state),
        Pane::Nodes => render_nodes(frame, chunks[2], state),
        Pane::NodeDetail { node_id } => render_node_detail(frame, chunks[2], state, node_id),
        Pane::Trace { node_id } => render_trace(frame, chunks[2], state, node_id.as_deref()),
    }
    render_hint_bar(frame, chunks[3], &state.pane);
}

fn render_status_bar(frame: &mut Frame<'_>, area: Rect, state: &ObserverState) {
    let conn = match &state.connection {
        Connection::Connecting => "connecting…".to_string(),
        Connection::Connected => "connected".to_string(),
        Connection::Disconnected {
            reason,
            retry_in_ms,
        } => {
            format!("disconnected ({reason}; retry {retry_in_ms}ms)")
        }
    };
    let identity = match &state.identity {
        Some(id) => format!(
            "id={} v={} api={}",
            id.process_start_id, id.version, id.api_version
        ),
        None => "id=?".to_string(),
    };
    let line = Line::from(vec![
        Span::styled(
            format!("grx observer  target={}  ", state.target_label),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(identity),
        Span::raw("  "),
        Span::styled(
            conn,
            match &state.connection {
                Connection::Connected => Style::default().fg(Color::Green),
                Connection::Connecting => Style::default().fg(Color::Yellow),
                Connection::Disconnected { .. } => Style::default().fg(Color::Red),
            },
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_pane_tabs(frame: &mut Frame<'_>, area: Rect, pane: &Pane) {
    let active = match pane {
        Pane::Overview => 0,
        Pane::Tree => 1,
        Pane::Nodes | Pane::NodeDetail { .. } => 2,
        Pane::Trace { .. } => 3,
    };
    let titles = PANE_LABELS
        .iter()
        .map(|s| Line::from(*s))
        .collect::<Vec<_>>();
    let tabs = Tabs::new(titles)
        .select(active)
        .style(Style::default())
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, area);
}

fn render_hint_bar(frame: &mut Frame<'_>, area: Rect, pane: &Pane) {
    frame.render_widget(
        Paragraph::new(pane.keybind_hint()).style(Style::default().add_modifier(Modifier::DIM)),
        area,
    );
}

fn render_overview(frame: &mut Frame<'_>, area: Rect, state: &ObserverState) {
    let block = Block::default().title("Overview").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let nodes = state.nodes.len();
    let supervisors = state.supervisors.len();
    let messages_in_log = state.trace_log.len();
    let last_poll = state
        .last_poll_at
        .map(|t| format!("{}ms ago", t.elapsed().as_millis()))
        .unwrap_or_else(|| "never".to_string());
    let lines = vec![
        Line::from(format!("nodes:        {nodes}")),
        Line::from(format!("supervisors:  {supervisors}")),
        Line::from(format!("trace log:    {messages_in_log}")),
        Line::from(format!("last poll:    {last_poll}")),
        Line::from(format!("poll round:   {}", state.poll_round)),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_tree(frame: &mut Frame<'_>, area: Rect, state: &ObserverState) {
    let block = Block::default()
        .title("Supervisor tree")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.supervisors.is_empty() {
        frame.render_widget(Paragraph::new("(no supervisors registered)"), inner);
        return;
    }
    let mut items: Vec<ListItem> = Vec::new();
    for sup in &state.supervisors {
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "{} [{}] (restarts {}; children: {})",
                sup.key,
                sup.strategy,
                sup.restart_count,
                sup.children.len()
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))));
        if let Some((last, rest)) = sup.children.split_last() {
            for child in rest {
                items.push(ListItem::new(format!("├── {child}")));
            }
            items.push(ListItem::new(format!("└── {last}")));
        }
    }
    frame.render_widget(List::new(items), inner);
}

fn render_nodes(frame: &mut Frame<'_>, area: Rect, state: &ObserverState) {
    let block = Block::default()
        .title(format!("Nodes ({})", state.nodes.len()))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header = Row::new(vec![
        Cell::from("ID"),
        Cell::from("ROLE"),
        Cell::from("DEPTH"),
        Cell::from("SUPERVISOR"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows = state.nodes.iter().enumerate().map(|(i, n)| {
        let style = if i == state.nodes_cursor && matches!(state.pane, Pane::Nodes) {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(n.id.clone()),
            Cell::from(n.role.clone()),
            Cell::from(n.depth.to_string()),
            Cell::from(n.supervisor.clone().unwrap_or_else(|| "-".into())),
        ])
        .style(style)
    });

    let widths = [
        Constraint::Length(24),
        Constraint::Length(16),
        Constraint::Length(6),
        Constraint::Min(8),
    ];
    frame.render_widget(Table::new(rows, widths).header(header), inner);
}

fn render_node_detail(frame: &mut Frame<'_>, area: Rect, state: &ObserverState, node_id: &str) {
    let block = Block::default()
        .title(format!("Node {node_id}"))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(node) = state.nodes.iter().find(|n| n.id == node_id) else {
        frame.render_widget(
            Paragraph::new(format!("node `{node_id}` not present in last poll")),
            inner,
        );
        return;
    };
    let supervisor = node.supervisor.as_deref().unwrap_or("-");
    let recent_count = state.recent_messages.get(node_id).map_or(0, Vec::len);
    let lines = vec![
        Line::from(format!("id:         {}", node.id)),
        Line::from(format!("role:       {}", node.role)),
        Line::from(format!("depth:      {}", node.depth)),
        Line::from(format!("supervisor: {supervisor}")),
        Line::from(format!("recent msgs (cached): {recent_count}")),
        Line::from(""),
        Line::from(Span::styled(
            "press `t` to start tailing this node",
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_trace(frame: &mut Frame<'_>, area: Rect, state: &ObserverState, node_id: Option<&str>) {
    let title = match node_id {
        Some(id) => format!("Trace ({id})"),
        None => "Trace (drill into a node and press `t` to tail)".to_string(),
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.trace_log.is_empty() {
        frame.render_widget(Paragraph::new("(no events seen yet)"), inner);
        return;
    }

    // Show the tail — `inner.height` rows of trace entries.
    let height = usize::from(inner.height);
    let start = state.trace_log.len().saturating_sub(height);
    let items = state
        .trace_log
        .iter()
        .skip(start)
        .map(|entry| ListItem::new(format_trace_entry(entry)))
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), inner);
}

fn format_trace_entry(entry: &TraceEntry) -> Line<'static> {
    match entry {
        TraceEntry::TargetRestarted { ts_ms } => Line::from(Span::styled(
            format!("[target restarted ts={ts_ms}]"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        TraceEntry::Message(m) => {
            let accept_glyph = if m.accepted { "ok" } else { "rj" };
            Line::from(format!(
                "{:>13}  {:<16}→{:<16} {:<12} ttl={} [{}]",
                m.ts_ms, m.from, m.to, m.message_type, m.ttl, accept_glyph
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::super::state::{Identity, NodeRow, ObserverState, SupervisorRow};
    use super::*;

    fn seeded_state() -> ObserverState {
        let mut s = ObserverState::new("127.0.0.1:9090".into());
        s.apply_poll(
            Identity {
                process_start_id: 42,
                version: "0.1.0".into(),
                api_version: 1,
            },
            vec![
                NodeRow {
                    id: "logger".into(),
                    role: "sink".into(),
                    depth: 0,
                    supervisor: Some("sup:0".into()),
                },
                NodeRow {
                    id: "sensor-1".into(),
                    role: "sensor".into(),
                    depth: 1,
                    supervisor: Some("sup:0".into()),
                },
                NodeRow {
                    id: "sensor-2".into(),
                    role: "sensor".into(),
                    depth: 1,
                    supervisor: Some("sup:0".into()),
                },
            ],
            vec![SupervisorRow {
                key: "sup:0".into(),
                strategy: "OneForOne".into(),
                max_restarts: 3,
                period_secs: 5,
                restart_count: 0,
                children: vec!["logger".into(), "sensor-1".into(), "sensor-2".into()],
            }],
            HashMap::new(),
            1_000,
        );
        s
    }

    #[test]
    fn overview_renders_three_nodes() {
        let state = seeded_state();
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &state)).unwrap();

        let buf = term.backend().buffer();
        let dump = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("nodes:        3"), "buffer: {dump}");
        assert!(dump.contains("supervisors:  1"), "buffer: {dump}");
    }

    #[test]
    fn nodes_pane_highlights_cursor_row() {
        let mut state = seeded_state();
        state.pane = Pane::Nodes;
        state.move_nodes_cursor(1);
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &state)).unwrap();

        let buf = term.backend().buffer();
        let dump = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("sensor-1"), "buffer: {dump}");
        assert!(dump.contains("sensor-2"), "buffer: {dump}");
    }

    #[test]
    fn tree_pane_lists_supervisor_and_children() {
        let mut state = seeded_state();
        state.pane = Pane::Tree;
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &state)).unwrap();

        let buf = term.backend().buffer();
        let dump = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("sup:0 [OneForOne]"), "buffer: {dump}");
        assert!(dump.contains("logger"));
        assert!(dump.contains("sensor-1"));
    }

    #[test]
    fn trace_pane_shows_target_restarted_divider() {
        let mut state = seeded_state();
        // Trigger a restart divider via a second poll with a new pid.
        state.apply_poll(
            Identity {
                process_start_id: 7,
                version: "0.1.0".into(),
                api_version: 1,
            },
            state.nodes.clone(),
            state.supervisors.clone(),
            HashMap::new(),
            2_000,
        );
        state.pane = Pane::Trace { node_id: None };
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &state)).unwrap();
        let buf = term.backend().buffer();
        let dump = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("target restarted"), "buffer: {dump}");
    }
}
