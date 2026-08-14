# GraphReact Rust as an Agentic Cloud substrate

**Status:** capability assessment and product boundary, 2026-08-11.

GraphReact Rust can serve as the execution and data plane of an Agentic Cloud.
It is not, by itself, the complete cloud product. The existing runtime already
provides many of the hard distributed-systems primitives an agent platform
needs: message-driven execution, supervision, multi-host distribution,
polyglot bodies, hot reload, ingress, observability, security controls, and
deployment packaging. The remaining work is concentrated in durable execution,
workload isolation, agent-specific services, and the multi-tenant control plane.

This document defines that boundary so future work can extend GraphReact without
turning `graphreact-core` into a collection of unrelated cloud services.

## Executive assessment

These percentages are architectural estimates, not production certification or
test-coverage claims.

| Scope | Estimated support | Meaning |
|---|---:|---|
| Agent runtime foundation | 70–80% | Most execution, messaging, recovery, distribution, and observation primitives exist. |
| Complete single-tenant Agentic Cloud | 40–50% | Durable jobs, tool/model services, approvals, and artifacts are still required. |
| Managed multi-tenant Agentic Cloud | 25–35% | Tenant isolation, quotas, billing, fleet scheduling, and a cloud control plane remain. |

The recommended product framing is:

> GraphReact is the reliable agent execution kernel. The Agentic Cloud supplies
> persistence, isolation, intelligence services, and organizational control
> around it.

## Mapping agent concepts onto GraphReact

| Agentic Cloud concept | GraphReact representation |
|---|---|
| Agent or durable task | A declared `.grx` node with a supervised runtime body |
| Delegation | A typed message routed to another node |
| Team or workflow topology | A `CommunicationGraph` subgraph |
| Tool request and result | Correlated request/reply messages through ingress or an internal edge |
| Capability declaration | Permission manifest plus explicit graph edges and external-ingress allowlists |
| Agent lifecycle | Mount, process, supervise, restart, drain, unmount |
| Agent group | Distributed process group |
| Runtime event | `TelemetryEvent` propagated to logs, metrics, traces, and event streams |
| Live rollout | Hot reload with compatibility classification and first-tick rollback |
| Remote worker | A location-transparent node hosted by a cluster peer |

This mapping should remain explicit. In particular, an agent is not a special
kind of scheduler hidden inside an LLM wrapper. It is a supervised GraphReact
node whose implementation may invoke models and tools.

## Capability matrix

### Strong substrate today

| Capability | Existing support | Remaining product work |
|---|---|---|
| Message-driven execution | `CommunicationGraph`, bounded queues, scheduler batches, hooks, typed node definitions, and correlated call/reply ingress | Agent-oriented SDK and standard task/result envelopes |
| Fault tolerance | OTP-inspired supervision, restart policies, backoff, restart-intensity limits, distributed monitors and links, partition policy | Durable recovery after total host or cluster loss |
| Distributed execution | Location-transparent addressing, TLS transport, discovery, process groups, partition detection, and distributed supervision | Fleet placement, autoscaling, capacity scheduling, and tenant-aware placement |
| Polyglot execution | JavaScript, Python, and Go bodies behind the engine abstraction | A stable agent SDK shared across languages and stronger workload boundaries |
| Observability | JSON logs, Prometheus, OpenTelemetry, management API, SSE/WS events, readiness, SLOs, and TUI | Agent-level cost, model, tool, evaluation, and outcome dimensions |
| Operational security | Static-token and JWT/OIDC authentication, RBAC, TLS rotation, secrets, rate limits, and hash-chained audit records | Fine-grained node/tool/resource authorization and tenant partitions |
| Deployment substrate | Distroless container, Helm, graceful drain, health/readiness, hot reload, rollback, SBOM, signing, and provenance lanes | Versioned agent releases, canaries, fleet rollout controller, and managed rollback |

### Partial support

| Capability | What exists | Critical gap |
|---|---|---|
| Durable execution | In-memory queues, idempotency support at ingress, supervision, drain, and hot-reload state preservation | No committed segment log, snapshot/replay runtime, durable timers, or restart-safe inbox/outbox |
| State and memory | Hook state and graph state during a live process | No durable state store, artifact store, checkpoint service, semantic memory, retention, or deletion policy |
| Secure tool access | Secrets, route-level RBAC, default-deny ingress manifests, and auditable calls | No tool registry/broker, MCP surface, scoped short-lived credential grants, egress policy, or per-call approval gate |
| Verification | Extensive Rust tests, fuzz substrate, audit-chain verification, SBOMs, signing, and provenance | No evaluator framework, result evidence bundle, model/tool replay record, or acceptance policy per task |
| Integrations | HTTP ingress, language ecosystems, Node bindings, WebSockets, and pluggable audit forwarders | No canonical connector lifecycle, OAuth delegation broker, webhook registry, or MCP compatibility layer |
| Governance | RBAC roles, JWT claim mapping, secrets, audit trails, and rate limiting | No organization/project/tenant resource model, tenant-scoped storage, quotas, data residency, or spend policy |

### Not yet supplied by GraphReact

- Human approval inboxes, pause/resume checkpoints, escalation policies, and
  timeout/default decisions.
- Model-provider abstraction, capability-based model routing, fallbacks,
  inference budgets, token accounting, and price/latency policies.
- Strong code isolation using containers, WebAssembly, or microVMs. Current
  language engines should be treated as trusted in-process workloads; the
  server-WASM target is still a stub.
- Multi-tenant scheduling, billing, organization administration, regional
  placement, and a managed fleet control plane.
- Durable artifacts and evidence attached to an agent result.

## Architecture boundary

The Agentic Cloud should be divided into four layers.

```text
┌──────────────────────────────────────────────────────────────────┐
│ Cloud control plane                                              │
│ tenants · projects · deployments · quotas · billing · policies   │
└───────────────────────────────┬──────────────────────────────────┘
                                │ desired state / status
┌───────────────────────────────▼──────────────────────────────────┐
│ Agent services                                                   │
│ model router · tool broker · approvals · memory · evaluations    │
└───────────────────────────────┬──────────────────────────────────┘
                                │ typed task/tool/model messages
┌───────────────────────────────▼──────────────────────────────────┐
│ GraphReact execution plane                                      │
│ graph scheduler · supervision · distribution · ingress · observe │
└───────────────────────────────┬──────────────────────────────────┘
                                │ workload execution contract
┌───────────────────────────────▼──────────────────────────────────┐
│ Isolated workers and persistence                                │
│ WAL/checkpoints · artifacts · containers/WASM/microVMs           │
└──────────────────────────────────────────────────────────────────┘
```

`graphreact-core` should continue to own deterministic graph execution,
messaging, lifecycle, and narrow extension traits. Provider SDKs, OAuth flows,
billing, user interfaces, and vendor-specific connectors belong above the core.

## Required additions

### 1. Durable runtime

This is the first dependency because every higher-level promise relies on it.

Required primitives:

- append-only runtime log with checksummed records and explicit format versions;
- snapshot plus replay for graph topology, hook state, inboxes, correlations,
  supervision state, and approval waits;
- transactional inbox/outbox semantics so a crash cannot lose or duplicate a
  tool request invisibly;
- durable timers, retry schedules, leases, cancellation, and deadlines;
- stable task, run, attempt, message, artifact, and correlation identifiers;
- pluggable local and cloud storage backends behind one conformance suite.

The planned `graphreact-persistence` work is the natural starting point, but an
Agentic Cloud requires it on the backend runtime path rather than only as a
GraphReactDB concern.

### 2. Isolated executor

Define an executor interface independently from the language-engine interface.
An engine evaluates a trusted body; an executor establishes a security and
resource boundary.

The executor contract should cover:

- image/module identity and immutable version;
- CPU, memory, wall-clock, process, filesystem, and output limits;
- network egress policy and DNS controls;
- ephemeral and durable mounts;
- short-lived secret injection without exposing the operator secret store;
- cancellation, heartbeat, logs, result artifacts, and termination reason;
- container, server-WASM, and microVM implementations with conformance tests.

### 3. Agent services

Add these as services consumed through typed GraphReact messages:

- model registry and router with provider adapters, fallbacks, budgets, and
  usage accounting;
- MCP-compatible tool registry and broker with schema validation, scoped grants,
  idempotency, timeouts, and audit receipts;
- human approval service whose wait state is durable and resumable;
- memory service separating task state, durable facts, retrieval indexes, and
  artifacts;
- evaluation service for policy checks, tests, confidence, and evidence bundles.

The scheduler should not import model-provider or connector SDKs. These services
remain ordinary supervised nodes or external services behind ingress contracts.

### 4. Cloud control plane

The managed product additionally needs:

- organization, project, environment, tenant, user, service-account, and role
  resources;
- desired-state deployments with immutable versions and rollback history;
- tenant-aware placement, quotas, concurrency limits, regional policy, and
  autoscaling;
- per-tenant encryption, storage partitioning, retention, deletion, and export;
- cost attribution and hard spending limits for models, tools, compute, and
  storage;
- staged rollout, canary analysis, admission policy, and fleet reconciliation.

Control-plane state should be strongly consistent and separate from the
high-throughput execution/event plane.

## Delivery order and acceptance gates

### Phase A — recoverable single-tenant agents

Deliver persistence, stable run identifiers, durable timers, cancellation, and
an artifact store.

Implementation tracker (updated 2026-08-14):

- Shipped: versioned persistence records, UUIDv7 identities, verified local
  append-only storage, snapshots, immutable deployment bundles, durable
  scheduler activations, state, timers, effects, cancellation, and artifacts.
- Shipped: named entrypoints plus protected run creation, status, cancellation,
  artifact-listing, and artifact-download routes.
- Shipped: the cluster `PersistenceEngine` has a Postgres control plane,
  S3-compatible immutable objects, lease acquisition/renewal/release, monotonic
  fencing, fenced head/snapshot updates, and deterministic conformance tests.
- Shipped: active/passive daemon orchestration waits without mounting on the
  standby, acquires with a 15-second lease and 2-second polling, renews every 5
  seconds before scheduler work, and closes readiness/admission plus halts on
  lease loss.
- Shipped: an opt-in real-service conformance suite runs against disposable
  Postgres and MinIO in CI. It covers immutable bundles, commits and replay,
  expired-writer orphan safety, fenced takeover, snapshots, idempotent artifact
  metadata, integrity verification, lease release, and deployment mismatch.
- Shipped: a three-cycle real-daemon failover gate suspends each active replica,
  requires its standby to become ready within 30 seconds, resumes the former
  owner, and proves renewal fencing halts it before further scheduler work.
- Shipped: JavaScript, Python, and Go expose equivalent named durable-state,
  durable-effect, durable-timer, immutable-artifact, and monotonic-cancellation
  activation APIs over one versioned JSON wire contract.
- Shipped: bounded durability metrics cover commit latency/failures, current
  log and snapshot bytes, recovery duration/replay count, effect retries and
  indeterminate outcomes, lease renewals/losses, and active-replica role.
- Remaining before Phase A closes: hash-chained durability audit events,
  decoder fuzzing, and the final crash/failover soak gates.

Acceptance: kill the daemon at every boundary of a tool-call workflow and prove
that restart produces exactly one externally visible outcome.

### Phase B — safe tools and models

Deliver the isolated executor, model router, MCP tool broker, scoped credentials,
budgets, and approval nodes.

Acceptance: every external side effect is attributable to a run, bounded by a
policy, auditable, and either idempotent or explicitly marked non-replayable.

### Phase C — multi-tenant control plane

Deliver tenant resources, storage partitions, quotas, deployment versions,
placement, rollouts, and billing attribution.

Acceptance: two hostile tenants can share a fleet without observing each
other's data, credentials, traffic, metrics, artifacts, or resource consumption
beyond documented aggregate behavior.

### Phase D — managed reliability

Deliver autoscaling, regional failover, canary analysis, backup/restore,
disaster-recovery exercises, and continuously enforced SLOs.

Acceptance: a regional execution-plane loss resumes durable runs in another
region within the published recovery objectives and preserves audit continuity.

## Trust invariant

The product-level invariant is:

> Every agent action is attributable, capability-bounded, durable at the point
> promised to the caller, reversible where the external system permits it, and
> independently verifiable from retained evidence.

GraphReact already supplies important parts of attribution, bounded routing,
recovery, and observability. The roadmap above closes the durability,
isolation, and verification portions without weakening the runtime's existing
substrate boundaries.

## Current-state caveat

This assessment describes the current repository worktree. The M-Prod close-out
records core behavior as implemented while noting that full workspace, platform,
release-workflow, and external-service validation still depend on CI and
environment-backed fixtures. Agentic Cloud planning must therefore distinguish
"implemented substrate" from "operationally certified managed service."
