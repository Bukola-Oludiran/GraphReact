# 0003 — GraphReact bifurcates into two products: GraphReact (Backend) + GraphReactDB

**Status:** DECIDED — recorded 2026-05-03; partially amended 2026-06-15 (see Amendments).
**Related:** Strategic plan at `~/.claude/plans/graphreact-rust-later-milestones.md` (replaces the lost `toasty-crafting-lantern.md`; this ADR's original Strategic-plan reference therefore now points at the reconstruction). Decision 0001 (V8 soft-demote) and decision 0002 (backend-only architecture) both stay valid; this decision sits on top. A follow-up ADR-0005 will record the Java→Go pivot for the M17 third-runtime slot (see Amendments).

## Amendments

- **2026-06-15 — Java→Go pivot for M22 mixed-runtime composition.** The line below that reads "Mixed-runtime systems (M22) | JS + Python + JVM in the same `.grx` system" was decided as **JS + Python + Go** in the 2026-06-15 replanning session (Q1 paired with M17 ships the Go engine; M22 relaxes for mixed-runtime). The "JVM" wording below remains as the historical record of the original 2026-05-03 decision; treat the later-milestone plan as authoritative for current scope. ADR-0005 will record this pivot in full.
- **2026-06-15 — M21 reducer-with-subscription supersedes Cypher-shaped query language.** The line below that reads "graphreact-query (M21) | Cypher-shaped query language compiled to Memo dependency forest" was decided as **SpacetimeDB-style reducer-with-subscription** in the 2026-06-15 replanning session (Q4). The "Cypher-shaped" wording below remains as historical record; treat the later-milestone plan as authoritative for current scope.
- **2026-06-15 — Plan-file reference.** The cited `toasty-crafting-lantern.md` is lost on disk and unrecoverable; the equivalent reconstruction lives at `~/.claude/plans/graphreact-rust-later-milestones.md`. Line-number citations to the lost original (e.g. line 138 below) reference text that no longer exists; the line numbers will not resolve.

## The decision

GraphReact ships as **two products with a shared substrate**:

1. **GraphReact** — Backend Runtime. BEAM-class server runtime in the Akka / Erlang-OTP / Orleans tradition. Ships server-side `.grx` execution + ops protocols + distribution + hot reload + observability.
2. **GraphReactDB** — Reactive Graph Database. SpacetimeDB-shaped product where clients consume the running runtime directly as live state + typed action surface. Ships client SDK protocol + persistence + query language + subscription manager + canonical React client SDK on top of the substrate.

Both products consume the same Rust crates (substrate); they differ at the *product wrapper* layer.

**Sequencing**: GraphReact (Backend) ships first. Most of the existing M12-M18 work is Backend-shaped. GraphReactDB starts materializing once persistence (M19) + query language (M21) + new client-SDK-protocol milestone land — that's a future track that builds on the same substrate.

**Naming (working, until branding decides otherwise)**: "GraphReact" for the Backend product, "GraphReactDB" for the DB product.

## Why two products

The two-product split clarifies pitch, audience, and milestone scoping:

- **GraphReact's pitch**: "Fault-tolerant, distributed, message-driven backend systems with React-shaped dev-ex." Audience: ops engineers, backend devs (Thymus self-healing K8s, ComplyFlow compliance, distributed actor systems, workflow engines). Comparable to Akka, Erlang/OTP, Orleans.
- **GraphReactDB's pitch**: "Live graph state, observable from clients, with typed reducer-style mutations. The runtime IS the API." Audience: application developers building realtime/collaborative/multiplayer apps. Frontend devs who want subscriptions instead of REST endpoints. Comparable to SpacetimeDB, Firebase Realtime, Supabase Realtime.

Conflating them muddles both. Splitting them lets each product evolve, ship, document, and market on its own arc while the substrate work serves both.

## What's shared (substrate)

| Crate | Purpose |
|---|---|
| `graphreact-core` | graph + scheduler + hooks + supervision + reactivity |
| `graphreact-grx-parser` | `.grx` AST + IR + multi-file resolver |
| `graphreact-engine-zig` + `graphreact-engine-jsc` + `graphreact-engine-cpython` + future engines (M17 JVM) | engine bridges; engine-trait stays engine-agnostic |
| `graphreact-algorithms` + `graphreact-graspologic` + `graphreact-linalg` | NetworkX-parity + graspologic ports + linalg primitives |
| M12.4 (Node-API compat for JSC) | backend-NPM access in JS bodies — works the same in either product |
| `.grx` JSON-AST schema (M11/M12.1 `grx parse --json`) | **stable + versioned** because both products and companion tools depend on it |

The substrate is engine-agnostic and product-agnostic. Adding a feature to the substrate benefits both products by construction.

## What's product-specific

### GraphReact (Backend)

| Crate / Milestone | Purpose |
|---|---|
| `graphreact-cli` (the `grx` binary) | runs systems, exposes ops protocols. Backend's primary entry point. |
| `graphreact-dist` (M14) | multi-host clustering, distributed supervision, distributed pg, net-split handling |
| `graphreact-hotreload` (M15) | compatible-change reload preserves state + queue; safe-point detection; rollback |
| `graphreact-observer` (**M16-Backend**) | Prometheus exporter + OpenTelemetry traces + structured JSON logs + management/introspection HTTP API + terminal `grx observer` + per-body debug protocols (CDP/debugpy/JDWP, opt-in) |
| Server-WASM target (M18) | Cloudflare Workers / Wasmtime / Wasmer / sandbox plugin scenarios |
| Mixed-runtime systems (M22) | JS + Python + JVM in the same `.grx` system |

### GraphReactDB

| Crate / Milestone | Purpose |
|---|---|
| `graphreact-db-server` (NEW; provisional name) | DB product daemon. Wraps the substrate with: WebSocket subscription gateway, typed reducer (message-send) endpoints, schema-introspection endpoints, client-facing query routing. |
| `graphreact-persistence` (M19) | event-sourced graph mutation log; replay-to-state; durable monitor refs across restarts. **Lifted from "future sketches" to first-class** because persistence is core to the DB pitch (a database without durability isn't a database). |
| `graphreact-query` (M21) | Cypher-shaped query language compiled to Memo dependency forest; **tier-aware** (`MATCH … USING TIER 2`) per the three-tier possibility model. **Lifted from "future sketches" to first-class** because subscription queries need a stable client-facing surface. |
| `graphreact-db-distributed` (M20) | sharded `CommunicationGraph`; query routing; replication. **Different consistency model from M14** — clients see authoritative state, not just message routing. |
| **M16-DB** (split from M16) | Client SDK gateway. WebSocket subscription manager; typed message-send endpoints with per-message-type permission-manifest validation; schema-introspection endpoints (`GET /v1/schema` returns running runtime's `.grx` JSON-AST + per-node manifests for client codegen). |
| New milestone for **client SDK protocol** | wire format spec + canonical React client SDK pattern + typed binding generation strategy. Comparable in scope to M16. Provisional name: M24 (placeholder). |
| `@graphreact/client-react` (companion ecosystem) | Canonical React client SDK. Third-party or maintained-with-attribution; lives outside the runtime workspace per [decision 0002](0002-graphreact-is-backend-only.md). |

## M16 splits explicitly

The bifurcation has a concrete consequence at M16 — what was one observer milestone becomes two, sharing infrastructure but serving different audiences:

| | M16-Backend | M16-DB |
|---|---|---|
| **Audience** | ops engineers running `grx` in production | application clients (React UIs, mobile apps, etc.) |
| **Surface shape** | terminal CLI + standard ops protocols | WebSocket subscription protocol + REST reducer endpoints |
| **Deliverables** | Prometheus exporter, OpenTelemetry traces, structured JSON logs, management API for introspection, `grx observer` terminal CLI, per-body debug protocols (CDP/debugpy/JDWP, opt-in) | Client SDK gateway: WebSocket subscription manager, typed reducer (message-send) endpoints with permission-manifest validation, schema-introspection endpoint, connection lifecycle + auth |
| **Auth** | `--observer-token` + TLS for remote bind | per-client auth token + TLS for remote bind; permission-manifest validation on every reducer call |
| **Network default** | `127.0.0.1` only | `127.0.0.1` only; production deployments enable explicit remote bind |
| **Versioning** | versioned via the management API (`/v1/`); breaking changes follow deprecation cycle | versioned via the client SDK protocol (`/v1/`); breaking changes follow deprecation cycle; SDK pinning supported |

Both share underlying plumbing (HTTP server, JSON serialization, WebSocket lifecycle, auth middleware, the `.grx` JSON-AST as the schema source). Different surfaces; same engine room.

## Documentation structure

Two doc trees with separate pitches and audiences:

- **`docs/backend/`** — GraphReact (Backend) docs. Onboarding for ops engineers + backend devs. Tutorials for distributed systems + supervision + hot reload + ops integration. Reference for `grx` CLI subcommands, M16-Backend protocols, deploy targets.
- **`docs/db/`** — GraphReactDB docs. Onboarding for application devs. Tutorials for client SDKs (React first; future Rust/Swift/Unity). Reference for client SDK protocol, subscription model, reducer API, schema introspection, query language.

**Shared docs** stay at `docs/` root:
- `docs/decisions/` — architectural decisions span products. (This file lives here.)
- `docs/SUBSTRATE.md` (future) — shared substrate documentation referencing both products.

Existing docs migrate:
- `docs/m12.3b-webkit-vendoring.md` → `docs/backend/m12.3b-webkit-vendoring.md`
- `docs/m13-phases.md` → `docs/backend/m13-phases.md` (CPython engine bridge — Backend product's polyglot story)
- `docs/m6.6-engine-parity.md` → `docs/backend/m6.6-engine-parity.md`

## Sequencing

GraphReact (Backend) ships first because:
1. **Most existing work is Backend-shaped.** M12.3a + 3b + 3a-followon + M13-phase3a/3b are all about running `.grx` systems with engine bodies — that's Backend product's runtime. The DB product builds on top of this and would be ungrounded without it.
2. **DB substrate is mostly nascent.** M19 (persistence) + M21 (query) + new M24 (client SDK protocol) all need to be built. Years of work.
3. **Backend's pitch is more concrete today.** "Run distributed actor systems in JS or Python with React-shaped dev-ex" is something we can demo. "Live graph state with React subscriptions" needs more substrate first.
4. **Risk reduction.** Shipping Backend first validates the substrate at production-shaped workloads before the DB product takes a dependency on it.

DB product work begins materially after M16-Backend ships. Until then, M19/M20/M21 stay tagged as "future DB-product milestones, not yet active."

Some shared-substrate work happens in parallel naturally (M11 parser improvements, M12.4 Node-compat, M17 engine bridges) — those serve both products and don't wait on either.

## Implications for the existing plan

- **Product-tag every milestone**: `[Shared]` / `[Backend]` / `[DB]`. Substrate (M0-M11) is `[Shared]`; M12-M18 + M22 are mostly `[Backend]`; M19-M21 + new M24 are `[DB]`; M23 (LSP) is `[Shared]`.
- **M19/M20/M21 promoted from future sketches to first-class DB-product milestones** with deliverable bodies + acceptance gates.
- **M16 splits into M16-Backend + M16-DB** as detailed above.
- **New M24 (DB client SDK protocol)** added with sketch-level deliverables.
- **Workspace structure list** updated to anticipate `graphreact-db-server/`, `graphreact-persistence/`, `graphreact-query/` crates (currently absent from the file tree; they materialize during the DB product's track).
- **Companion-ecosystem section** clarifies that the canonical React client library is *aligned with the DB product* specifically. (It can also visualize Backend systems via the Backend's M16-Backend management API, but its primary shape is DB-product-aligned.)
- **Polyglot rollout phases** stay valid for Backend; DB product's polyglot story is "any engine can author DB-shaped components" once the substrate (M14, M15, M16-Backend) lands.

## What's NOT being decided here

- **Renaming `graphreact-cli` to something more product-specific** (e.g., `graphreact-backend-cli`). Not necessary now; the existing crate name stays. If/when the DB product's daemon is added, it gets its own crate (`graphreact-db-server`); the existing `graphreact-cli` stays as the Backend's CLI without renaming.
- **Branding / website / marketing.** "GraphReact" + "GraphReactDB" are working names for code/docs/internal use. Final branding can shift later without breaking the architectural commitment.
- **Whether the DB product ever ships.** This decision says "GraphReactDB is a real product track on the roadmap." It does not commit a specific ship date or resource allocation. The Backend product is the priority; DB matures organically as the substrate solidifies.
- **Cross-product features.** A `.grx` system running on Backend can be observed via the Backend's M16-Backend management API. If the same system is later run under GraphReactDB, the same `.grx` works (substrate is shared); clients gain access to typed reducer + subscription surfaces via M16-DB. The two products are *deployment shapes* of the same substrate; specific systems can run under either.

## Reversibility

Architectural commitments should be reversible if reality changes. Triggers that would warrant re-merging the products:

- **The DB product's surface grows so much overlap with the Backend's M16 management API** that maintaining two distinct surfaces becomes pure overhead. Possible but distant — the audiences (ops vs. application clients) and protocols (Prometheus/OTel/CLI vs. WebSocket subscriptions/typed reducers) are different enough that the split should hold.
- **The DB product never gets traction** and stays perpetually-nascent for years. Then the architectural commitment to ship both becomes performative; consolidate as Backend with a DB-shaped extension surface.
- **Naming conflicts emerge** with another product called "GraphReactDB". Unlikely to invalidate the architectural commitment, but might force a rename.

If a trigger fires, re-open this doc + propose a flip.

## Cross-references

- Plan file `~/.claude/plans/toasty-crafting-lantern.md`: new Product structure section + product-tagged milestones + M16 split + M19/M20/M21 promoted + new M24 + workspace structure update + companion-ecosystem reframing.
- Decision 0001 (V8 soft-demote): unchanged. Decision concerns engine selection, not product structure.
- Decision 0002 (backend-only architecture): unchanged. Both products are backend-tier; the bifurcation is about which kind of backend product, not whether GraphReact ships frontends.
- M12.4 memory (`project_graphreact_m12_4_node_compat.md`): unchanged. Node-API compat is shared substrate, benefits both products.
- Companion memory `project_graphreact_bifurcation_decision.md` (created alongside this doc): carries the framing forward to future sessions.
