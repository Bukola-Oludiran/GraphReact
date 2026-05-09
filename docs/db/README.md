# GraphReactDB (Reactive Graph Database) — documentation

> **Pitch**: Live graph state, observable from clients, with typed reducer-style mutations. The runtime IS the API. SpacetimeDB-shaped over graph-reactive semantics instead of relational.
>
> **Audience**: application developers building realtime / collaborative / multiplayer apps. Frontend devs who want subscriptions instead of REST endpoints.
>
> **Comparable to**: SpacetimeDB, Firebase Realtime, Supabase Realtime.

## Status — early roadmap, not yet shipping

GraphReactDB is the **second of two GraphReact products** ([decision 0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md)). The Backend product (GraphReact, [`../backend/`](../backend/README.md)) ships first; GraphReactDB starts materialising after the substrate stabilises. As of 2026-05-03:

- Substrate work that benefits the DB product is shipping today (M11 parser, M12 JSC, M13 CPython, M12.4 Node-compat).
- DB-product-specific milestones (M16-DB, M19, M20, M21, M24) are sketched but not yet active.
- Most of this doc tree is **forward-looking** until the product track materialises.

## What this product will be

GraphReactDB takes the same `.grx` system you'd run on the Backend product and exposes it as a **reactive graph database** that clients consume directly:

- **Subscriptions over polling.** Clients subscribe to typed queries (`{nodes: {role: 'sensor'}, state: ['readings']}`) via WebSocket; server pushes deltas as state changes.
- **Typed reducers.** Clients call typed functions (message-sends to nodes) over the wire; server validates against permission manifests; state mutates; subscriptions push deltas to all subscribers.
- **Authoritative server.** Clients can't bypass validation; the runtime is the source of truth.
- **Persistence.** Event-sourced graph mutation log + replay-to-state (M19); restart-safe.
- **Tier-aware queries.** Cypher-shaped query language (M21) with `MATCH ... USING TIER 2 BY 'message-type'` — clients can ask "what's the policy-derived reachability set for this message type?" not just "what edges exist".
- **Schema-introspection-driven typed clients.** `GET /v1/db/schema` returns the running runtime's `.grx` JSON-AST + per-node manifests; client SDKs use that to generate typed bindings.

## Conceptual analog: SpacetimeDB

GraphReactDB is conceptually SpacetimeDB-shaped, but over graph-reactive state instead of relational rows:

| SpacetimeDB | GraphReactDB |
|---|---|
| A *module* (compiled WASM) declaring tables + reducers | A `.grx` system declaring nodes + edges + bodies + per-node `useState` |
| *Tables* (relational rows) | *Nodes* (graph vertices with hook state + permission manifest) |
| *Reducers* (typed RPC functions, server-side mutations) | *Messages* (typed events processed by `useMessage` handlers, mutating state via setters) |
| *Subscriptions* (SQL-like queries) | *Live state subscriptions* (M21 Cypher-shaped queries; clients subscribe to subgraphs + state slots + message types) |
| Authoritative server validates reducer calls | Authoritative scheduler validates incoming messages against permission manifests (and optionally edge-accept predicates per [followon-F three-tier model](https://example.com)) |
| `@clockworklabs/spacetimedb-sdk` is the canonical client | `@graphreact/client-react` (working name) is the canonical React client SDK |

## Sister product: GraphReact (Backend)

GraphReactDB shares its underlying Rust substrate with GraphReact (the Backend product, [`../backend/`](../backend/README.md)). What differs is the product wrapper:

- **Backend** uses the `grx` CLI binary and exposes ops surfaces (Prometheus, OTel, JSON logs, management API, terminal observer).
- **DB** uses a `grx-db-server` daemon binary (anticipated; not yet built) and exposes client surfaces (WebSocket subscription gateway, typed reducer endpoints, schema introspection).

The same `.grx` system can run on either; what changes is which product wrapper hosts it.

## Living docs in this tree

(None yet — DB product track has not started shipping. As M16-DB / M19 / M21 / M24 progress, sub-phase docs land here.)

## Key milestones for the DB product

(See the [strategic plan](../../../../../.claude/plans/toasty-crafting-lantern.md) for full milestone definitions; tagged `[DB]` or `[Shared substrate]`.)

| Milestone | What it delivers | Status |
|---|---|---|
| M16-DB | Client SDK gateway — WebSocket subscription manager + typed reducer endpoints + schema introspection | Sketched 2026-05-03; not active |
| M19 | Persistence — event-sourced graph mutation log + replay-to-state + durable monitor refs | Sketched as first-class (was future) 2026-05-03 |
| M20 | Sharded `CommunicationGraph` with client-visibility consistency model | Sketched as first-class (was future) 2026-05-03 |
| M21 | Cypher-shaped, tier-aware query language for client subscriptions | Sketched as first-class (was future) 2026-05-03 |
| M24 | Wire-format spec + canonical React client SDK reference pattern + typed-binding codegen strategy | Sketched 2026-05-03 |
| M25+ | Additional client SDK languages (Rust, Swift, C# / Unity) — companion ecosystem | Sketched |

## Decisions touching this product

- [Decision 0002 — backend-only architecture](../decisions/0002-graphreact-is-backend-only.md). GraphReactDB is backend-tier; clients consume the runtime via the SDK protocol.
- [Decision 0003 — product bifurcation](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md). GraphReact (Backend) and GraphReactDB are two products with shared substrate; the bifurcation is the foundational decision for this product.

## Quickstart (placeholder — when the product ships)

```sh
# Install (future)
brew install graphreact-db

# Run a .grx system as a DB
grx-db-server my-app.grx --bind 127.0.0.1:8088

# In a React app, install + connect
npm install @graphreact/client-react
```

```jsx
import { useGraphReactQuery, useGraphReactSend } from '@graphreact/client-react';

function Dashboard() {
    const sensors = useGraphReactQuery({ role: 'sensor' });
    const send = useGraphReactSend();
    return (
        <>
            {sensors.map(s => (
                <Sensor key={s.id} data={s} onCalibrate={() => send(s.id, { type: 'calibrate' })} />
            ))}
        </>
    );
}
```
