# M16-Backend — `grx observer` TUI keybinds + recipes

**Status:** SHIPPED 2026-05-08 (P11 closeout doc; TUI shipped P6).

`grx observer` is the htop/k9s-class terminal UI for live introspection of a running `grx cluster daemon`. ratatui 0.28 + crossterm 0.28; works over SSH; no GUI/browser dependency. P6 shipped pass-1 (data-shows-correctly + reconnect-on-restart); pass-2 polish (column widths, color highlighting, pane-specific filtering) is documented follow-up per the P6 2-iteration budget.

## Launch

```sh
grx observer --target https://daemon.example.com:9090 \
  --token-env GRX_OBSERVER_TOKEN
```

| Flag | Description |
|---|---|
| `--target <URL>` | Daemon's management-API URL. `https://` (default) uses `rustls-native-certs` system trust store; `http://` plain. |
| `--token-env <VAR>` | Read bearer token from environment variable. Mutually exclusive with `--token-file`. |
| `--token-file <PATH>` | Read bearer token from file. Refuses to load if the file is group-/world-readable on Unix (mode-0600 enforcement). |
| `--insecure-no-tls` | **DEV ONLY.** Disables certificate verification. MUST NOT appear in production deploys; only for self-signed-cert local development. The flag is on `grx observer` only; never on the daemon path or other introspection commands. |
| `--ws` (default off) | Enable WebSocket subscription to the live event stream (P7 substrate). Default polls REST every 100ms (10 Hz); WS streams per-message granularity that polling can't reach. CLI WS task COEXISTS with polling — both stream simultaneously. |

## Pane state machine

| Key | Pane | Notes |
|---|---|---|
| `Tab` | Cycle Overview → Tree → Nodes → Trace → Overview | NodeDetail is a drill-down from Nodes (not in the Tab cycle). |
| `Esc` | Return from NodeDetail / Trace to Overview | Stable home pane. |

### Overview

Stats summary: cluster identity, node count, supervisor count, message rate (per second over last 1m), active subscribers, recent reload outcome.

### Tree

ASCII supervisor tree with restart-counts. Mirrors `grx tree` output. Each supervisor shows its strategy + intensity + current children.

### Nodes

Flat list of every registered node sorted by message-delivery rate (descending). Each row: `node_id`, `role`, `depth`, `messages_recv` (count over last 1m), `last_render` (timestamp).

| Key | Action |
|---|---|
| `↑` / `↓` (or `j` / `k`) | Move cursor. |
| `Enter` | Drill into NodeDetail for the selected node. |
| `t` | Switch to Trace pane focused on the selected node. |

### NodeDetail

Per-node deep dive: full path, supervisor mapping, recent message ring (last 64), render count, last error.

| Key | Action |
|---|---|
| `Esc` | Return to Nodes. |
| `t` | Switch to Trace pane for this node. |

### Trace

Per-node message stream. Polls `/v1/nodes/:id/messages/recent` at 10 Hz (REST) AND/OR streams via WebSocket (`?node=<id>` filter). NDJSON-style line-mode follower. Suitable for piping via `grx trace <node-id>` (the non-TUI variant).

| Key | Action |
|---|---|
| `c` | Clear the trace tape. |
| `Esc` | Return to Nodes. |

## Reconnect behavior

On WS drop or REST poll failure, exponential backoff schedule:
- Floor: 250ms
- Ceiling: 30s
- Multiplier: 2
- Reset: floor on every successful poll.

On reconnect, `process_start_id` is checked via `/v1/identity`. If it changed (daemon restarted), the trace tape is cleared + a `[target restarted]` divider is pushed. Same semantic as the WS server's resync-on-lag close-frame (which the client also handles by inserting the divider).

## Common recipes

### "What's the busiest node right now?"

`Nodes` pane (Tab once from Overview). Sorted by message-delivery rate descending. Top row.

### "Why did supervisor `sup:3` restart 5 times in the last minute?"

`Tree` pane → find `sup:3` → `Enter` (drill into its child list) → `Enter` again on the child showing high restart count → see the recent message ring + last error reason.

### "Tail messages flowing into `worker-42` like `tail -f`"

`grx trace worker-42` (non-TUI; pipes NDJSON to stdout, suitable for `jq` filtering).

OR: in the TUI, `Nodes` → cursor to `worker-42` → press `t` → see the live tail in the Trace pane.

### "Watch for the next hot reload"

`Overview` pane shows `recent reload` (status + timestamp). For the live-stream variant, run with `--ws` and watch for `kind: hot_reload` events.

### "Headless ops scripts"

The TUI is interactive only; for scripted ops use:
- `grx ps` — flat node list (`--json` for jq pipelines).
- `grx tree` — ASCII supervisor tree.
- `grx top` — htop-shaped live refresh on stdout (no full TUI; uses ANSI clear-screen).
- `grx trace <node-id>` — NDJSON message tail.

These all share `observer_client.rs` (~225 LOC hand-rolled HTTP/1.1 over `tokio::net::TcpStream` + manual bearer auth header — no `reqwest` dep). TLS via `tokio_rustls::TlsConnector` + `rustls-native-certs`.

## Pass-2 polish (documented follow-up)

P6 2-iteration budget bounded the initial ship to data-shows-correctly + reconnect-on-restart. Documented follow-ups for P11+ / M-Prod:

- Column widths auto-fit + truncation with ellipsis.
- Color highlighting (green = healthy, yellow = degraded, red = recently-failed).
- Pane-specific filtering (e.g. Nodes pane: filter by role, depth range).
- Search-by-id (e.g. `/` opens a fuzzy-finder over node IDs).
- Help overlay (`?`).
- Per-pane refresh-rate override (default 10 Hz; some panes could be slower).

These are TUI ergonomics, not substrate. Ship as user feedback comes in.

## Cross-references

- [`m16-architecture.md`](m16-architecture.md) — overall data flow.
- [`m16-management-api.md`](m16-management-api.md) — endpoint catalog (the TUI's data source).
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase6.md` — P6 closeout (TUI substrate landed; render layer + watch channel + reconnect + TLS uplift).
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase7.md` — P7 closeout (WS subscriber path + slow-subscriber resync).
