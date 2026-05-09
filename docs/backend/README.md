# GraphReact (Backend Runtime) — documentation

> **Pitch**: Fault-tolerant, distributed, message-driven backend systems with React-shaped dev-ex. BEAM-class server runtime.
>
> **Audience**: ops engineers, backend devs building distributed systems (self-healing infrastructure, compliance routing, distributed actor systems, workflow engines).
>
> **Comparable to**: Akka, Erlang/OTP, Orleans.

## What this product is

GraphReact (the Backend product) is a server-side runtime that executes `.grx` systems. A `.grx` file declares nodes (with bodies in JS / Python / future JVM), edges, supervision policies, and message-routing channels. The runtime parses the file, mounts the components, schedules message dispatch, and handles supervision crash-recovery — all server-side.

You interact with GraphReact via:

- **The `grx` CLI** — `grx run`, `grx ps`, `grx top`, `grx tree`, `grx trace`, `grx connect`, `grx reload`, `grx repl`, `grx parse --json`, etc.
- **Standard ops protocols (M16-Backend)** — Prometheus `/metrics`, OpenTelemetry traces, structured JSON logs, and a versioned management/introspection HTTP JSON API.
- **Per-body debug protocols (opt-in)** — CDP for JS bodies, debugpy for Python bodies, JDWP for JVM bodies; attach your existing IDE.

You do NOT interact with GraphReact via a bundled UI — there isn't one. GraphReact is backend-only ([decision 0002](../decisions/0002-graphreact-is-backend-only.md)). If you want a custom dashboard, you build it as a separate frontend tier that consumes the management API.

## Sister product: GraphReactDB

GraphReact bifurcates from a sibling product called **GraphReactDB** ([decision 0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md)). GraphReactDB targets a different audience (application devs building realtime/collaborative apps) with a different surface shape (client SDK protocol + subscriptions + typed reducers, SpacetimeDB-style). Both products share the same Rust substrate; what differs is the product wrapper layer and the protocol surfaces. GraphReactDB docs live at [`../db/`](../db/README.md).

GraphReact (Backend) ships first. GraphReactDB starts materializing after the Backend product reaches stable shape.

## Living docs in this tree

| Doc | Topic |
|---|---|
| [`m14-distribution.md`](m14-distribution.md) | M14 milestone narrative — distribution layer (Erlang-cluster-equivalent); shipped 2026-05-05 |
| [`m13-phases.md`](m13-phases.md) | M13 living document — CPython engine bridge for backend Python authoring; phases 1+2 / 2.5 / 3a / 3b shipped, 3c–3h roadmap |
| [`m12.3b-webkit-vendoring.md`](m12.3b-webkit-vendoring.md) | M12.3b living document — vendored WebKit for cross-platform JSC linkage |
| [`m6.6-engine-parity.md`](m6.6-engine-parity.md) | M6.6 living document — TS feature parity audit; per-engine sign-off matrix |

## Key milestones for the Backend product

(See the [strategic plan](../../../../../.claude/plans/toasty-crafting-lantern.md) for full milestone definitions; tagged `[Backend]` or `[Shared substrate]`.)

| Milestone | What it delivers |
|---|---|
| M11 | `.grx` parser (Shared) |
| M12 / M12.1-polish / M12.3a / M12.3b / M12.3a-followon | Standalone `grx` CLI + JSC engine bridge |
| M12.4 | Node-API compat for JSC bodies — first-class backend NPM ecosystem access |
| M13 | CPython engine bridge — Python authors first-class; PyPI works |
| M14 | Distribution layer — multi-host clustering, distributed supervision |
| M15 | Hot reload — compatible-change reload preserves state + queue |
| M16-Backend | Ops surface — Prometheus + OTel + JSON logs + management API + `grx observer` terminal CLI |
| M17 | JVM engine bridge (primary M17 deliverable) |
| M18 | Server-WASM target — Cloudflare Workers / Wasmtime / Wasmer / sandbox plugins |
| M22 | Mixed-runtime systems — JS + Python + JVM in same `.grx` |

## Quickstart (placeholder — fills in as product stabilises)

```sh
# Install
brew install graphreact     # (future)

# Initialise a project
grx init my-system && cd my-system

# Run the system
grx run my-system.grx

# Observe
grx observer
```

## Decisions touching this product

- [Decision 0001 — V8 soft-demote](../decisions/0001-v8-engine-question.md). JSC suffices for backend NPM (especially with M12.4); V8 not first-party.
- [Decision 0002 — backend-only architecture](../decisions/0002-graphreact-is-backend-only.md). GraphReact is backend-tier; ops surfaces are protocols + CLIs, not UIs.
- [Decision 0003 — product bifurcation](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md). GraphReact (Backend) and GraphReactDB are two products with shared substrate.
