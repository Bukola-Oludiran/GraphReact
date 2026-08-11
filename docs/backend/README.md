# GraphReact (Backend Runtime) — documentation

> **Pitch**: Fault-tolerant, distributed, message-driven backend systems with React-shaped dev-ex. BEAM-class server runtime.
>
> **Audience**: ops engineers, backend devs building distributed systems (self-healing infrastructure, compliance routing, distributed actor systems, workflow engines).
>
> **Comparable to**: Akka, Erlang/OTP, Orleans.

> **New developers start here:** see [`../onboarding.md`](../onboarding.md) for orientation + reading order, or [`../architecture.md`](../architecture.md) for the crate map.

## What this product is

GraphReact (the Backend product) is a server-side runtime that executes `.grx` systems. A `.grx` file declares nodes (with bodies in JavaScript, Python, or Go), edges, supervision policies, and message-routing channels. The runtime parses the file, mounts the components, schedules message dispatch, and handles supervision crash-recovery — all server-side.

For the boundary between this backend runtime and a complete managed agent
platform, see [GraphReact Rust as an Agentic Cloud
substrate](../agentic-cloud.md). That document treats GraphReact as the
execution plane and identifies persistence, isolation, agent services, and the
multi-tenant control plane as distinct layers.

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
| [`m22-mixed-runtime.md`](m22-mixed-runtime.md) | M22 milestone narrative — mixed-runtime daemon (JS + Python + Go in one `.grx`); phases 1-4 shipped 2026-06-17 |
| [`m23-lsp.md`](m23-lsp.md) | M23 milestone narrative — `.grx` Language Server outer-shell MVP (initialize / diagnostics / hover / goto-definition / completion); shipped 2026-06-17 |
| [`go-module-dependencies.md`](go-module-dependencies.md) | Deterministic public/private Go modules, offline builds, cache fingerprints, and per-artifact SBOMs |

## Key milestones for the Backend product

(See the [later-milestone strategic plan](../../../../../.claude/plans/graphreact-rust-later-milestones.md) for M17–M25+ definitions; tagged `[Backend]` / `[Shared substrate]` / `[DB]` per [ADR-0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md). Pre-M17 milestones live in the per-milestone narrative docs in this directory.)

| Milestone | Product-tag | What it delivers |
|---|---|---|
| M11 | [Shared] | `.grx` parser |
| M12 / M12.1-polish / M12.3a / M12.3b / M12.3a-followon | [Backend] | Standalone `grx` CLI + JSC engine bridge |
| M12.4 | [Backend] | Node-API compat for JSC bodies — first-class backend NPM ecosystem access |
| M13 | [Backend] | CPython engine bridge — Python authors first-class; PyPI works |
| M14 | [Backend] | Distribution layer — multi-host clustering, distributed supervision |
| M15 | [Backend] | Hot reload — compatible-change reload preserves state + queue |
| M16-Backend | [Backend] | Ops surface — Prometheus + OTel + JSON logs + management API + `grx observer` terminal CLI |
| M-Ingress (M16.5) | [Backend] | External HTTP ingress substrate — POST `/v1/nodes/:id/{messages,call}` + bulk NDJSON; default-deny via manifest |
| **M17** | [Backend] | **Go runtime engine bridge — third in-process language host (replaces JVM per Java→Go pivot 2026-06-15)** |
| **M22** | [Backend] | **Mixed-runtime systems — JS + Python + Go in same `.grx` (phases 1-4 SHIPPED 2026-06-17, see [`m22-mixed-runtime.md`](m22-mixed-runtime.md); phase 5+ cross-engine messaging open)** |
| **M23** | [Shared] | **LSP server for `.grx` — outer-shell MVP shipped 2026-06-17 (see [`m23-lsp.md`](m23-lsp.md)); semantic tokens / VS Code extension publishing / in-body completions deferred to M23.next** |
| **Cleanup-Pass** | [Backend / Shared] | **Pre-M-Prod deferred-cleanup slice (13 named residuals)** |
| **M-Prod** | [Production-hardening] | **Production-readiness milestone — 9 sub-tracks (auth / TLS / secrets / lifecycle (incl. M14.11) / observability / rate limiting / supply-chain / packaging / chaos+fuzz). STARTED 2026-06-17. Group 1 (2026-06-17): #3 secrets Phase A (`grx secrets` + OS keyring, see [`m-prod-secrets.md`](m-prod-secrets.md)); #5 observability (`/v1/readyz` + `OBSERVER_SHUTDOWN_DEADLINE` + residual mpsc sweep + cluster-membership probe + published [`SLO.md`](../SLO.md), see [`m-prod-observability.md`](m-prod-observability.md)); #9 chaos+fuzz (`fuzz_management_api_bodies` libFuzzer + CI cell, see [`m-prod-chaos-fuzz.md`](m-prod-chaos-fuzz.md)). Group 2 (2026-06-17): #2 Phase A (atomic-swap TLS rotation substrate, see [`m-prod-tls-rotation.md`](m-prod-tls-rotation.md)); #7 supply chain (CycloneDX 1.4 SBOM + `GRX_SIGNATURE_URL` in `grx version` + reproducible-build CI cell + Trivy SARIF, see [`m-prod-supply-chain.md`](m-prod-supply-chain.md) + [`m-prod-runbook.md`](m-prod-runbook.md)). Group 3 (2026-06-17): #1 Phase A (hash-chained `AuditSink` + `FileAuditSink` mode-0o600 + `grx audit verify` CLI + ADR [`0006`](../decisions/0006-audit-chain-substrate.md), see [`m-prod-auth.md`](m-prod-auth.md)); #4 Phase α (SIGTERM drain primitive + ingress 503 `draining` + `DaemonHandle::start_drain` + `--drain-deadline-secs`, see [`m-prod-lifecycle.md`](m-prod-lifecycle.md)). Group 4 (2026-06-18): #6 Phase A (per-IP `tower_governor` on `/v1/*` protected routes + 429 envelope + `grx_rate_limit_blocked_total{route}` + audit event + precedence-locked Retry-After clamp, see [`m-prod-rate-limit.md`](m-prod-rate-limit.md)); #8 Phase A (distroless multi-arch Dockerfile + tag-triggered `release.yml` + `charts/grx/` helm + homebrew formula scaffold, see [`m-prod-packaging.md`](m-prod-packaging.md)). All 9 sub-tracks now have safe-slice substrate landed; Phase B/C/D (multi-token RBAC, ACME cert rotation, M14.11 delegated-restart, OTel BatchSpan bench, Sigstore signing, ghcr.io push, distributed rate limit, k8s operator) deferred per the M-Prod close-out doc.** |
| **M19** | [DB] | **`graphreact-persistence` — own segment log + snapshot + replay** |
| **M20** | [DB] | **`graphreact-db-distributed` — sharded `CommunicationGraph` with Raft per shard** |
| **M21** | [DB] | **`graphreact-query` — SpacetimeDB-style reducer-with-subscription (supersedes the prior "Cypher-shaped" wording)** |
| **M24** | [DB] | **DB client SDK protocol — WS + binary BSATN; canonical React SDK; typed-binding codegen** |
| **M25 / M25.a / M25.b** | [DB] | **Additional client SDKs — Rust → Swift → C#/Unity (per locked Q11)** |
| M18 | [Backend] | Server-WASM target — orthogonal, ships on host-platform demand (Cloudflare Workers / Fastly / Wasmtime / Wasmer / WAMR) |

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
