# 0002 — GraphReact is a backend runtime; no bundled frontend

**Status:** DECIDED — recorded 2026-05-03. Addendum 2026-05-03 (same day) on backend-NPM access.
**Related:** Strategic plan at `~/.claude/plans/toasty-crafting-lantern.md`. Companion ecosystem section + M16 (Observer) + M18 (WASM target) + **M12.4 (Node-API compat for JSC bodies)** all touch this commitment.

## The architectural commitment

**GraphReact is a server-side runtime. It lives at the backend tier of an N-tier application architecture. It does NOT ship a frontend, GUI, web UI, or any browser-rendered surface.** The runtime's surfaces are exclusively *protocols* (HTTP JSON, Prometheus, OpenTelemetry, structured logs, debug-protocol endpoints) and *ops CLIs* (`grx`, `grx observer`, `grx ps`, `grx top`, etc.). Frontend tier — if a user wants one — is the user's responsibility to build, OR an external companion library's responsibility to provide.

Same architectural slot as Postgres, Redis, BEAM. Same shape of distribution: a server process + a CLI + standard ops surfaces. Frontend is someone else's tier.

## Why this is the right framing

1. **GraphReact's value proposition is backend logic** — message routing, state, supervision, distribution, hot-reload, observer telemetry. None of that is rendered. Bundling a UI dilutes the value proposition.
2. **N-tier separation is industry-standard.** Postgres + pgAdmin (separate project), Redis + RedisInsight (separate project), Kubernetes + k9s/Lens (separate projects). The pattern is mature.
3. **Per-application customization.** Different GraphReact users (compliance auditing vs ML pipelines vs IoT meshes vs game state) want very different visualization affordances. A bundled UI commits us to a one-size-fits-all design that won't fit anyone well.
4. **Frontend-stack agnosticism.** Bundling React makes us hostile to Vue/Svelte/htmx/native-app users. Standards-based protocol surfaces work for everyone.
5. **Release velocity.** Backend runtime cadence and frontend library cadence don't match. Coupling them slows both.
6. **Distribution simplicity.** A backend binary stays a backend binary. The moment we ship a UI, we're shipping JS bundles, supporting browser quirks, debugging UA-specific rendering, etc. Out of scope.

## What this means in practice

### Runtime ships (backend-tier surfaces)

- **CLI** — `grx` and its subcommands (`run`, `parse`, `observer`, `ps`, `top`, `tree`, `trace`, `repl`, `init`, `version`, `fmt`, `check`, `completions`, `reload`, `connect`).
- **Management/introspection API (M16)** — HTTP JSON endpoints for `/supervisor/tree`, `/nodes`, `/nodes/{id}`, `/nodes/{id}/messages/recent`. Optional WebSocket/SSE for live tailing. Authenticated via `--observer-token` + TLS for remote bind. **Stable + documented + `/v1/` path prefix from day one** because companion tools depend on it.
- **Prometheus exporter (M16)** — `/metrics` endpoint exposing scheduler/supervisor/hook counters. Pluggable into the user's existing Grafana/Datadog/etc.
- **OpenTelemetry traces (M16)** — message flow as spans; node lifecycle as events. Standard observability pipeline.
- **Structured JSON logs (M16)** — single-line-per-event for Loki/Elastic ingestion.
- **Per-body debug protocols (M16 — opt-in)** — `--debug=jsc:9229` (CDP), `--debug=cpython:5678` (debugpy), `--debug=jvm:5005` (JDWP). Engine-native; users attach their existing IDE.
- **`grx parse --json`** — stable JSON-AST output of a parsed `.grx`. Companion tools depend on it for static topology rendering.
- **Server-WASM target (M18)** — for edge-compute / embedded-host / sandbox deployments. NOT for browser frontends; browser is incidentally a CI-convenient WASM runner.

### Runtime does NOT ship

- ❌ Bundled web UI (Preact, React, Vue, htmx, anything).
- ❌ Bundled supervisor-tree GUI / message-flow visualizer.
- ❌ Browser-frontend WASM target (browsers may be incidentally a WASM runner; not a use case).
- ❌ Chrome DevTools as the runtime-observability entry point. (CDP is fine for per-body source debugging, opt-in. Not for runtime telemetry.)
- ❌ Any frontend-tier code in the GraphReact-runtime workspace.

### Encouraged companion ecosystem (third-party / user-built)

GraphReact's standard protocol surfaces enable a healthy companion ecosystem. Catalogued in the plan's "Companion ecosystem" section. The canonical envisioned shape:

**A third-party React library** — e.g. `@graphreact/visualizer` — that:
- Parses a `.grx` file (via the runtime's stable `grx parse --json` schema).
- Renders the topology as a graph (cytoscape.js / vis-network / react-flow / etc.).
- Optionally connects to a running `grx` instance via the management API + WebSocket; overlays live state.
- Composable: drop `<GraphReactVisualizer grxFile={...} runtime={url} />` into a user's React app.

The `.grx` file is the structured source-of-truth for system topology. The runtime is one rendering of it (executes business logic). A React library would be another rendering of it (visualizes structure + live state). Both consume the same spec; neither is privileged.

This is NOT a GraphReact-team deliverable. If it materializes (third-party, user-built, or eventually maintained-with-attribution), great — we link to it from docs.

## Implications for the plan

### M16 (Observer) — rewritten 2026-05-03

Prior framing leaned on "CDP-via-JSC + Chrome DevTools" as the primary observability surface. That conflated runtime telemetry (Axis A) with per-body source debugging (Axis B). The 2026-05-03 rewrite splits them:
- Axis A is server-native protocols (terminal CLI + Prometheus + OTel + JSON logs + management API).
- Axis B is per-body debug protocols (CDP/debugpy/JDWP), opt-in.

The "soft-deferred bundled Preact UI" deliverable is **dropped entirely** — not deferred. GraphReact does not ship UIs.

### M18 (WASM target) — reframed 2026-05-03

Prior framing read "WASM-via-Boa-or-QuickJS, headless Chromium smoke test, bundle ≤2 MB gzipped" as if browser-frontend was the use case. The 2026-05-03 reframe makes server-WASM deployment the explicit use case (Cloudflare Workers, Fastly, Wasmtime, Wasmer, WAMR, sandbox/plugin scenarios). Browser is incidentally a CI-convenient WASM runner, not a target.

### Companion ecosystem section — added 2026-05-03

A new section in the plan, separate from the milestone numbering, records the canonical envisioned companion ecosystem shape (React library, terminal dashboards, IDE plugins, mobile ops apps, domain-specific code generators). Explicitly NOT a GraphReact-team deliverable list — the section's purpose is to make the architectural intent durable so future contributors don't re-bundle these into the runtime.

### Stable management API requirement (new contract)

If we're committed to companion tools, the management API must be stable. From the M16 implementation onward:
- Endpoints carry a `/v1/` prefix.
- JSON shapes are documented + versioned.
- Prometheus metric names + labels are documented + versioned.
- `grx parse --json` JSON-AST schema is documented + versioned.
- WebSocket/SSE event schema is documented + versioned.
- Breaking changes go through deprecation cycle (warn for 2 minor versions, break in next major).

## Reversibility / when to revisit

Architectural commitments should be reversible if reality changes. Triggers that would warrant re-opening:

- **A user pays for a bundled UI.** Even then, more likely we'd point them at building a companion library and pay them to maintain it. Bundling-into-runtime is the bar this commitment defends; an external library is fine.
- **A specific runtime telemetry need that genuinely can't be expressed via the management API + Prometheus + OTel + structured logs.** Hard to imagine; would surface organically. Even then, the answer is probably "extend the management API", not "ship a UI".
- **The companion ecosystem fails to materialize for >2 years and a vendor blocks adoption** specifically because no GraphReact-shipped UI exists. Possible but distant; would need real signal.

If any trigger fires, re-open this doc + propose a flip with the specific motivation.

## What is NOT being decided here

- **Per-body debugger surfaces** are valid (engine-native CDP/debugpy/JDWP). They're just not the runtime-observability surface.
- **The ops CLI** (`grx observer`, `grx top`, etc.) is valid — that's a CLI, not a UI in the frontend sense. Same shape as `redis-cli monitor` or `kubectl top`.
- **The server-WASM target (M18)** is valid — it's a backend deployment target, not a browser frontend.
- **Companion-ecosystem encouragement** is valid — it's the intended shape, just not a GraphReact-team deliverable.

## Addendum 2026-05-03: backend-NPM access is a long-term capability goal (M12.4)

The "backend-only" framing in this decision document was prompted by user clarification that "JS-ecosystem reach" — the load-bearing premise of the M12.3 Bun reference — meant **backend NPM packages** specifically (Express, Prisma, ioredis, pg, AWS SDK, BullMQ, etc.), NOT browser-side libraries.

That framing surfaces a real asymmetry in the current plan as designed:

- **CPython bodies** get PyPI access for free at M13-phase3a+, because CPython's import machinery is part of every sub-interpreter. A Python author can `import requests` / `import sqlalchemy` / `import boto3` and it works.
- **JSC bodies** at M12.3a/b are pure ECMAScript with no `require()`, no `import` from `node_modules`, no `process` / `Buffer` / `fs` / `http` / `crypto`. The M12 plan's Bun fork-and-strip matrix explicitly excluded Bun's Node-compat layer ("VirtualMachine.zig REST IS BUN-SPECIFIC — SKIP").

The asymmetry conflicts with the architectural intent. **Backend-only means the runtime is backend-tier; it does NOT mean JS bodies should be weaker than Python bodies.** First-class backend ecosystem access is part of the architectural commitment, not separate from it.

**M12.4 (Node-API compatibility for JSC bodies)** is committed — locked in by user 2026-05-03 (option (C) of three options on the table — pure-ES status quo / targeted shims / full Bun-fork). The plan's M12.4 entry captures the sub-staging (5 phases) and acceptance gates. M12.4 reverses the M12.3 fork-and-strip exclusion of Bun's Node-compat layer, taking it under MIT.

Important framing: M12.4 does NOT relax the "backend only" commitment in this decision document. M12.4 expands what backend JS bodies can do; it doesn't ship a frontend, doesn't add browser support, doesn't invalidate the N-tier separation. Backend-NPM access is a backend capability — same way PyPI is a backend capability for Python bodies.

**This addendum makes the architectural commitment richer, not narrower.** "GraphReact is backend-only" ⊃ "first-class backend ecosystem access for every supported language."

See plan file's M12.4 section for sub-staging + acceptance gates + risk mitigation.

## Cross-references

- Plan file `~/.claude/plans/toasty-crafting-lantern.md`: M16 rewrite, M18 reframe, companion-ecosystem section, **M12.4 (Node-API compat for JSC bodies)** all reference this decision.
- Decision 0001 (V8 soft-demote) is independent but architecturally aligned — both reflect "the runtime's value is in the runtime, not in the surfaces around it; commit to focused scope, encourage ecosystem." M12.4 reinforces 0001: with Node-compat at JSC, the V8 soft-demote becomes more grounded (JSC truly suffices for backend-NPM, the original V8 motivation).
- Memory file `project_graphreact_backend_only_decision.md` (created alongside this doc) carries the framing forward to future sessions.
- Memory file `project_graphreact_m12_4_node_compat.md` (created with this addendum) captures the M12.4 commitment as a recall hook for future sessions.
