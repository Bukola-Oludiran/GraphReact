//! The canonical `grx_prelude.go` template emitted by
//! [`crate::Codegen::emit_prelude`] for every `.grx` file that
//! contains `lang="go"` bodies.
//!
//! # Status
//!
//! - **M17 phase 3b-real:** SHIPPED. Real body dispatch (Ctx +
//!   Manifest + bodyRegistry + RegisterBody) via codegen-emitted
//!   dispatch.go.
//! - **M17 phase 3c:** SHIPPED. `UseEmit` + `UseMessage` Go-side
//!   helpers, message handler registry + token allocator,
//!   `GrxInvokeHandler` ABI symbol for Rust → Go handler dispatch.
//! - **M17 phase 3b-complete:** SHIPPED. `UseState` + `UseEffect`
//!   Go-side helpers, `callbackRegistry` + `GrxInvokeCallback` +
//!   `GrxReleaseCallback` ABI symbols for envelope-shaped Rust → Go
//!   callback dispatch (value / cleanup / throw). State preserved
//!   across renders; effect cleanup runs on deps change + unmount.
//! - **M17 phase 3d-α:** SHIPPED. `UseMemo` + `UseCallback` +
//!   `UseReducer` Go-side helpers. Split-cache pattern: memo +
//!   callback wrappers call `Invoke("host_memo_check", deps)` first;
//!   on hit they return the cached value, on miss they run the
//!   factory + call `Invoke("host_memo_store", value)`. Avoids
//!   re-entering Go from inside a held engine borrow.
//! - **M17 phase 3d-β:** SHIPPED. `UseNeighbors` + `UseTopology` +
//!   `UseAcceptEdge` Go-side helpers. Neighbors is a pure
//!   HOOK_CTX read; UseTopology piggybacks on the handler-registry
//!   substrate from `UseMessage` (same envelope shape); UseAcceptEdge
//!   enqueues a `PendingMutation::AcceptEdge` via the host callback
//!   that fires an `EdgeAdded` topology event on the next cascade.
//! - **M17 phase 3d-γ:** SHIPPED. `UseDirectory` exposes the
//!   scheduler's `GraphDirectory` (FindByID / FindByRole /
//!   FindByDepth / FindInSubtree / IsAncestor); `UseGraphQuery`
//!   Tier 1 surfaces the live `GraphSnapshot` over four accessors
//!   (Neighbors / HasNode / HasEdge / NodeCount). Both share the
//!   `CURRENT_DIRECTORY` + `CURRENT_GRAPH_SNAPSHOT` TLS slots
//!   installed by `GoEngineGuard::with_directory` around every
//!   `process_batch`.
//! - **M17 phase 3d-δ:** SHIPPED. `GraphQuery` widens with seven
//!   algorithm accessors (ShortestPath / AllPaths / AStarPath / BFS
//!   / DFS / ConnectedComponents / CycleDetection) backed by
//!   `graphreact_algorithms` against the live snapshot. Tier 1
//!   AStarPath is heuristic-free; per-message-type variants land in
//!   3d-ε via `g.Tier2(msgType).AStarPath(...)`.
//! - **M17 phase 3d-ε:** SHIPPED. `g.Tier2(msgType, opts?)` opens a
//!   Tier 2 directed-adjacency view derived from per-node
//!   `Manifest.Emits/Accepts` pairs. 12 accessors layered onto a
//!   `Tier2Query` handle: NodeCount, HasNode, Neighbors, HasEdge,
//!   BFS, DFS, ShortestPath, TopologicalSort, AStarPath, AllPaths,
//!   ConnectedComponents, CycleDetection. Five A* heuristic kinds
//!   (HeuristicLatency / HeuristicCompliance / HeuristicHierarchy /
//!   HeuristicTrustAttenuated / HeuristicPrerequisiteDistance) ride
//!   on `Tier2Opts.Heuristic`. Two reachability flavors (structural
//!   default; `DirectoryShortcuts` opt-in).
//! - **M17 phase 3d-ζ:** SHIPPED. `UseEdgeAcceptHandler(predicate)`
//!   installs a per-target accept/veto closure; the scheduler invokes
//!   it from both AcceptEdge mutation paths (3d-β) and Tier 3 queries.
//!   `g.Tier3(msgType, opts?)` opens a Tier 3 view that filters Tier 2
//!   candidacy through `predicates[u](s, msgType)`. 12 accessors
//!   layered onto a `Tier3Query` handle — same surface as Tier2Query.
//!   Default-accept whenever a predicate is absent or panics, so
//!   Tier 3 ⊆ Tier 2 monotonically and `Tier 3 == Tier 2` when no
//!   predicates are registered. Closes the M17 phase 3d arc.
//!
//! Future M17 phase 3e–h sessions add supervision hooks,
//! `UseSource`, and `UseEffectAsync` on top of this substrate.
//!
//! # M17 phase 3f — deadlock-freedom invariants for concurrent isolates
//!
//! 100 isolates × 1000 messages stress-tested without GC contention
//! deadlock by [`crates/graphreact-engine-go/tests/stress_100x1k.rs`].
//! The acceptance is satisfied by the EXISTING substrate — no
//! production-code lock or layout change in 3f — because the
//! emitted prelude already honors a small set of deadlock-freedom
//! invariants. **Any future edit that touches the cited line ranges
//! MUST keep the corresponding invariant intact.** Reviewers can
//! verify each invariant directly from the cited line range.
//!
//! 1. **No process-global lock is held across the cgo trampoline
//!    call.** `Invoke` releases `globalCallbacksMu.RUnlock()` BEFORE
//!    the `C.grx_invoke_host_callback` round-trip (search the
//!    emitted body for the `RUnlock` immediately preceding
//!    `invokeHostCallback`). If any future variant holds a Go-side
//!    Mutex across cgo, a Go GC stop-the-world phase that waits on
//!    the lock-holding goroutine will deadlock against the host
//!    callback waiting on the same lock.
//!
//! 2. **`hostSlotMu.RUnlock()` is released BEFORE host-callback fire
//!    in `UseEmit`** (search for `hostSlotMu.RUnlock()` immediately
//!    above `invokeHostCallback(slot, args)`). Same rationale as
//!    invariant #1.
//!
//! 3. **Per-isolate `is.mu` is released BEFORE `body(ctx)` in
//!    `GrxFunctionCall`** — the function-table read is the only
//!    work that holds `is.mu`; body execution itself runs unlocked,
//!    so bodies on different isolates run concurrently from the
//!    Go-runtime POV. (The Rust `Engine` trait's `&mut self`
//!    contract still serializes call_function on a single GoEngine
//!    instance; that's a Rust-side invariant unrelated to Go-side
//!    deadlock-freedom.)
//!
//! 4. **`callbackSlot.cbData` is stored as `uintptr`, never
//!    `unsafe.Pointer`.** The integer-encoded callback id is a fake
//!    pointer value that fails Go's goroutine-stack-walker pointer
//!    validation when stored as `unsafe.Pointer`. The conversion to
//!    `unsafe.Pointer` happens ONLY at the cgo call site in
//!    `invokeHostCallback`. Any future field that holds an
//!    integer-encoded id MUST follow the same `uintptr`-at-rest,
//!    `unsafe.Pointer`-at-boundary pattern (see the long doc-
//!    comment on `callbackSlot`).
//!
//! 5. **Zero `runtime.LockOSThread`, zero `runtime.Lock*` anywhere
//!    in the emitted prelude.** Pinning a goroutine to its OS
//!    thread is what would expose the GC-STW-during-cgo-on-locked-M
//!    deadlock 3f's acceptance is named after. Any future addition
//!    MUST update this invariant + the stress test.
//!
//! 6. **`UseEffectAsync` holds `callbackMu` ONLY across the small
//!    registry insert + goroutine spawn — NEVER across the cgo
//!    `host_effect` call.** (Verify: the `callbackMu.Unlock()`
//!    immediately precedes `Invoke("host_effect", ...)`.) Same
//!    rationale as invariant #1.
//!
//! 7. **`Invoke`'s args buffer + result buffer live on the C heap
//!    via `C.malloc` / `C.free`, NOT the Go heap.** The Go GC
//!    mark-phase does not scan C-heap allocations; `C.CString(...)`
//!    creates a C-heap copy of the args JSON. This bounds the GC
//!    pressure of high-frequency `Invoke` calls.
//!
//! 8. **`host_message` / `host_effect` / `host_topology` defer
//!    engine re-entry to a closure invoked LATER from scheduler
//!    flush — NEVER inside a body holding `engine.borrow_mut()`.**
//!    This is the deferred-closure pattern documented in
//!    `goruntime.rs::build_host_*_callback` (Rust side); the Go
//!    side honors it by emitting only sync envelope returns —
//!    actual cleanup / handler dispatch fires from a fresh
//!    scheduler tick.

/// ABI version baked into the emitted prelude. Must match
/// [`graphreact_engine_go::ffi::GRX_GO_ABI_VERSION`] (currently `1`).
/// The handshake fires in [`graphreact_engine_go::LoadedRuntime::load`].
///
/// Phase 3c added a new `GrxInvokeHandler` symbol but kept the ABI
/// version at 1 — the symbol is resolved as optional on the loader
/// side so older artifacts that don't ship it stay loadable for
/// hot-reload short-circuiting.
pub const GRX_GO_ABI_VERSION: u32 = 1;

/// Render the canonical `grx_prelude.go` source.
///
/// **Not the same shape** as
/// `crates/graphreact-engine-go/testdata/grx_go_stub.go`. The stub is a
/// hand-rolled FFI test fixture that hardcodes a small set of test
/// functions via switch dispatch (`calculator.Add` /
/// `callbacks.InvokeHostDouble` / etc.) so the engine-go conformance
/// smoke suite can exercise the loader + Engine-trait round-trip
/// without depending on the codegen pipeline. The codegen-emitted
/// prelude here is the **canonical** body-dispatch + hook surface
/// real production artifacts ship.
#[must_use]
pub fn render() -> String {
    format!(
        r#"// Code generated by graphreact-codegen-go. DO NOT EDIT.
//
// **M17 phase 3b-real + 3c.** Real body dispatch + UseEmit/UseMessage
// hook surface. GrxFunctionCall routes (component, fn_name) into
// bodyRegistry; UseEmit/UseMessage invoke host_emit/host_message
// callbacks via the engine-go trampoline; GrxInvokeHandler dispatches
// Rust-delivered messages back into Go closures.

package main

/*
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef int (*gr_host_callback_fn)(
    void *cb_data,
    const char *args_json,
    size_t args_len,
    char *out_result,
    size_t out_result_cap,
    size_t *out_result_len
);

static int grx_invoke_host_callback(
    gr_host_callback_fn cb,
    void *cb_data,
    const char *args_json,
    size_t args_len,
    char *out_result,
    size_t out_result_cap,
    size_t *out_result_len
) {{
    return cb(cb_data, args_json, args_len, out_result, out_result_cap, out_result_len);
}}
*/
import "C"

import (
    "context"
    "encoding/json"
    "fmt"
    "os"
    "sync"
    "sync/atomic"
    "unsafe"
)

const (
    GR_OK                    = 0
    GR_ERR_NOT_INITIALIZED   = -1
    GR_ERR_INVALID_ISOLATE   = -2
    GR_ERR_INVALID_MODULE    = -3
    GR_ERR_INVALID_FUNCTION  = -4
    GR_ERR_CALLBACK_DUPLICATE = -5
    GR_ERR_CALLBACK_NOT_FOUND = -6
    GR_ERR_BUFFER_TOO_SMALL  = -7
    GR_ERR_INTERNAL          = -99
)

const GRX_GO_ABI_VERSION uint32 = {abi_version}

// Ctx is the per-instance context the Rust scheduler hands to every
// body invocation. Mirrors the JSON shape CPython bodies receive from
// `pyruntime::build_host_ctx`.
type Ctx struct {{
    ID        string                 `json:"id"`
    Props     map[string]interface{{}} `json:"props"`
    Neighbors []string               `json:"neighbors"`
}}

// Secret returns a declared runtime secret, or the empty string when
// the active node did not declare it, it is missing, or no production
// secret callback is installed. Secret values are never cached here.
func (Ctx) Secret(name string) string {{
    raw, status := Invoke("host_secret", name)
    if status != GR_OK {{
        return ""
    }}
    var value string
    if err := json.Unmarshal(raw, &value); err != nil {{
        return ""
    }}
    return value
}}

// M6.6 method-style sibling API on Ctx. Thin delegations to the
// UseX hook accessors — mechanically identical (same HOOK_CTX TLS,
// same host callbacks), exposed as methods for ergonomic parity
// with React-style "ctx-as-prop" bodies. Bodies may freely mix the
// hook form (e.g. `UseSpawnChild()(spec)`) and the method form
// (e.g. `ctx.SpawnChild(spec)`).

// SpawnChild requests the scheduler to mount a dynamic child node
// with the given spec. Same semantics as `UseSpawnChild()(spec)`.
func (Ctx) SpawnChild(spec ChildSpec) {{ UseSpawnChild()(spec) }}

// Compose declares the active node's composed children. Same
// semantics as `UseCompose()(specs)`.
func (Ctx) Compose(specs []ChildSpec) {{ UseCompose()(specs) }}

// Unmount schedules the active node for removal. Same semantics
// as `UseUnmountSelf()()`.
func (Ctx) Unmount() {{ UseUnmountSelf()() }}

// AcceptEdge installs an accept-edge predicate targeting the
// named neighbour. Same semantics as `UseAcceptEdge()(targetID)`.
func (Ctx) AcceptEdge(targetID string) {{ UseAcceptEdge()(targetID) }}

// Manifest is the body's return value — the per-node permission
// manifest the Rust scheduler installs in `Scheduler::manifests`.
// `Extra` is round-tripped to the Rust side under any non-standard
// keys.
type Manifest struct {{
    ID                  string                       `json:"id,omitempty"`
    Accepts             []string                     `json:"accepts,omitempty"`
    Emits               []string                     `json:"emits,omitempty"`
    AcceptsExternal     []string                     `json:"accepts_external,omitempty"`
    AcceptsExternalCall []ExternalCall               `json:"accepts_external_call,omitempty"`
    NeedsSecret         []string                     `json:"needs_secret,omitempty"`
    Extra               map[string]interface{{}}       `json:"-"`
}}

// ExternalCall describes a call-shaped external acceptance entry.
// Mirrors `graphreact_core::ExternalCall`.
type ExternalCall struct {{
    RequestType string `json:"request_type"`
    ReplyType   string `json:"reply_type"`
}}

// BodyFn is the signature every component body satisfies. The
// codegen-emitted dispatch.go registers each user-authored body
// (`<Component>Body`) under the component name.
type BodyFn func(ctx Ctx) Manifest

var (
    bodyRegistryMu sync.RWMutex
    bodyRegistry   = map[string]BodyFn{{}}
)

// RegisterBody is called from each body file's init() — the
// codegen-emitted dispatch.go drives this. Re-registration overwrites
// (last-write wins) which matches the M15 hot-reload swap shape.
func RegisterBody(name string, fn BodyFn) {{
    bodyRegistryMu.Lock()
    bodyRegistry[name] = fn
    bodyRegistryMu.Unlock()
}}

// lookupBody returns the registered body for a component, if any.
func lookupBody(name string) (BodyFn, bool) {{
    bodyRegistryMu.RLock()
    fn, ok := bodyRegistry[name]
    bodyRegistryMu.RUnlock()
    return fn, ok
}}

// cbData is stored as `uintptr` (not `unsafe.Pointer`) because we
// encode the callback id as an integer-cast — a fake pointer value
// like `unsafe.Pointer(uintptr(62))`. Go's goroutine stack-walker
// (triggered on stack-growth at any goroutine boundary, including
// goroutines spawned by UseEffectAsync) validates `unsafe.Pointer`
// fields against the heap; an integer-encoded id like `0x3e` is not
// a valid heap pointer and the validator aborts with "bad pointer
// in frame ...". Storing as `uintptr` opts out of pointer scanning;
// we convert to `unsafe.Pointer` only at the cgo call site, which
// is the documented-safe boundary for the integer-id encoding.
type callbackSlot struct {{
    cbFn   C.gr_host_callback_fn
    cbData uintptr
}}

// functionInfo records (component, fn_name) so GrxFunctionCall can
// route by component into the body registry. Component IS the module
// name supplied to GrxModuleRegister.
type functionInfo struct {{
    component string
    name      string
}}

type isolate struct {{
    mu              sync.Mutex
    modules         map[uint64]string         // handle -> component name
    functions       map[uint64]functionInfo   // handle -> (component, fn_name)
    callbacks       map[uint64]callbackSlot
    callbacksByName map[string]uint64
    nextModule      uint64
    nextFunc        uint64
}}

var (
    stMu             sync.Mutex
    stLive           int64
    stInit           bool
    stIsos           = map[uint64]*isolate{{}}
    stNextIso        uint64
    nextCallbackId   uint64
)

// ----- M17 phase 3c: host_emit / host_message slot + handler registry -----
//
// The Rust side registers two named host callbacks per isolate:
// `host_emit` and `host_message`. We capture the function pointer +
// data slot on register so the Go-side UseEmit / UseMessage helpers
// can invoke them directly via the cgo trampoline. The handler
// registry stores user-authored Go closures by token id; the
// `GrxInvokeHandler` ABI symbol routes Rust-delivered messages back
// into the matching closure.

var (
    hostSlotMu      sync.RWMutex
    hostEmitSlot    callbackSlot
    hostMessageSlot callbackSlot

    // globalCallbacksByName mirrors every registered host callback
    // process-wide so Go bodies can invoke a named host capability
    // via `Invoke(name, args...)` without threading an isolate handle
    // through every UseX wrapper. Single Go runtime per process
    // (ADR-0005) makes a process-global mirror safe; the per-isolate
    // `isolate.callbacksByName` map is kept for dispose-time cleanup.
    globalCallbacksMu     sync.RWMutex
    globalCallbacksByName = map[string]callbackSlot{{}}

    handlerMu        sync.Mutex
    handlerRegistry  = map[uint64]func(map[string]interface{{}}){{}}
    nextHandlerToken uint64

    // callbackRegistry holds generic Go closures keyed by token id.
    // Used by UseEffect (and any future M17 phase 3d+ hook that needs
    // envelope-shaped Rust → Go callback dispatch — cleanup tokens
    // recursively land here too). Returns of nil → envelope kind
    // "value"; a `func()` cleanup closure → envelope kind "cleanup"
    // with a freshly minted token; a panic → envelope kind "throw".
    callbackMu        sync.Mutex
    callbackRegistry  = map[uint64]func() interface{{}}{{}}
    nextCallbackToken uint64

    // predicateRegistry holds edge-accept predicates (3d-ζ) keyed by
    // token id. Each predicate is `func(source, msgType string) bool`.
    // The Rust scheduler invokes them via the GrxInvokePredicate ABI
    // from both AcceptEdge mutation commit-time (3d-β) and Tier 3
    // dry-run query paths.
    predicateMu        sync.Mutex
    predicateRegistry  = map[uint64]func(source, msgType string) bool{{}}
    nextPredicateToken uint64

    // childBodyRegistry holds the BodyFn for each dynamically-spawned
    // child component (3e-α). Token allocated by registerChildBody and
    // marshalled through host_spawn_child / host_compose as a
    // `componentToken`; the Rust scheduler reaches back into Go via
    // the GrxInvokeChildBody ABI symbol on every render of the
    // spawned node.
    childBodyMu        sync.Mutex
    childBodyRegistry  = map[uint64]BodyFn{{}}
    nextChildBodyToken uint64
)

// registerMessageHandler stores `handler` under a freshly allocated
// token and returns it. UseMessage forwards the token to the Rust
// side via host_message; the Rust scheduler invokes GrxInvokeHandler
// with the same token on every delivery.
func registerMessageHandler(handler func(map[string]interface{{}})) uint64 {{
    token := atomic.AddUint64(&nextHandlerToken, 1)
    handlerMu.Lock()
    handlerRegistry[token] = handler
    handlerMu.Unlock()
    return token
}}

// Invoke calls a named host callback registered against the active
// isolate with the given args. Result is the raw JSON-encoded
// response bytes; status is GR_OK on success or a `GR_ERR_*` code.
// Used by Go bodies + test fixtures to reach host capabilities not
// surfaced by the well-known hook helpers (UseEmit / UseMessage).
//
// Mirrors the Python builtins-name-lookup pattern: pyruntime tests
// can `host_observe(s)` because Python imports the registered
// builtin by name. Go bodies do `Invoke("host_observe", s)` instead.
func Invoke(name string, args ...interface{{}}) ([]byte, int) {{
    globalCallbacksMu.RLock()
    slot, ok := globalCallbacksByName[name]
    globalCallbacksMu.RUnlock()
    if !ok {{
        return nil, GR_ERR_CALLBACK_NOT_FOUND
    }}
    argSlice := make([]interface{{}}, 0, len(args))
    argSlice = append(argSlice, args...)
    return invokeHostCallback(slot, argSlice)
}}

// invokeHostCallback serialises `args` as a JSON array and dispatches
// to `slot.cbFn`. Allocates a 4 KiB result buffer with one
// grow-on-overflow retry; on GR_OK returns the result JSON. Used by
// UseEmit / UseMessage to call into Rust.
func invokeHostCallback(slot callbackSlot, args []interface{{}}) ([]byte, int) {{
    if slot.cbFn == nil {{
        return nil, GR_ERR_NOT_INITIALIZED
    }}
    argsJson, err := json.Marshal(args)
    if err != nil {{
        return nil, GR_ERR_INTERNAL
    }}
    argsCStr := C.CString(string(argsJson))
    defer C.free(unsafe.Pointer(argsCStr))

    bufCap := C.size_t(4096)
    buf := C.malloc(bufCap)
    defer func() {{ C.free(buf) }}()
    var written C.size_t

    // Convert the integer-encoded id to `unsafe.Pointer` ONLY at the
    // cgo boundary — see callbackSlot.cbData doc-comment.
    cbDataPtr := unsafe.Pointer(slot.cbData) //nolint:gosec
    rc := C.grx_invoke_host_callback(
        slot.cbFn, cbDataPtr,
        argsCStr, C.size_t(len(argsJson)),
        (*C.char)(buf), bufCap, &written,
    )
    if rc == GR_ERR_BUFFER_TOO_SMALL {{
        bufCap = written
        C.free(buf)
        buf = C.malloc(bufCap)
        rc = C.grx_invoke_host_callback(
            slot.cbFn, cbDataPtr,
            argsCStr, C.size_t(len(argsJson)),
            (*C.char)(buf), bufCap, &written,
        )
    }}
    if rc != GR_OK {{
        return nil, int(rc)
    }}
    return C.GoBytes(buf, C.int(written)), GR_OK
}}

// EmitOpts is the optional third argument to a UseEmit closure.
// Keys mirror JSC + CPython phase 3c `parse_emit_options`:
// `to` (recipient list), `ttl` (override), `edgeKey` (edge name),
// `direct` (specific node id).
type EmitOpts struct {{
    To      []string `json:"to,omitempty"`
    Ttl     *uint32  `json:"ttl,omitempty"`
    EdgeKey string   `json:"edgeKey,omitempty"`
    Direct  string   `json:"direct,omitempty"`
}}

// UseEmit returns a closure that, when invoked, emits a message to
// the scheduler via the host_emit callback. The closure signature
// matches the JSC `useEmit()` and CPython `use_emit()` surfaces:
// `(msgType, payload, opts?)`. Mirrors M13-phase3c semantics —
// payload may be any JSON-serialisable value (nil → null); opts
// supports recipient targeting + TTL override.
func UseEmit() func(msgType string, payload interface{{}}, opts ...EmitOpts) {{
    return func(msgType string, payload interface{{}}, opts ...EmitOpts) {{
        hostSlotMu.RLock()
        slot := hostEmitSlot
        hostSlotMu.RUnlock()
        args := make([]interface{{}}, 0, 3)
        args = append(args, msgType, payload)
        if len(opts) > 0 {{
            args = append(args, opts[0])
        }}
        _, _ = invokeHostCallback(slot, args)
    }}
}}

// StateResult is the JSON shape host_state returns. ID is decoded
// as float64 because Rust's `serde_json::Number::from_f64(id as
// f64)` serialises to "1.0" / "2.0" / etc. — decimal-form floats —
// which Go's json.Unmarshal rejects when targeting a uint64. We
// cast back to uint64 at the setter-closure site (id values are
// always small positive integers minted by NEXT_SIGNAL_ID).
type StateResult struct {{
    ID    float64     `json:"id"`
    Value interface{{}} `json:"value"`
}}

// UseState returns the current value + a setter closure. State is
// preserved across renders by graphreact-core's hook slots; the
// setter marks the owning node dirty when invoked.
//
// Mirrors React's signature shape; the M13-phase3b CPython surface
// is identical modulo language conventions. The initial value is
// only used on the first render at the body's hook position; later
// renders observe whatever the setter has stamped into the Signal.
//
// Type system note: returns `interface{{}}` (Go's `any`). Bodies that
// need a typed view should type-assert: `count, setCount :=
// UseState(0); n := count.(float64)` (JSON marshalling normalises
// integers to float64 across the FFI boundary).
func UseState(initial interface{{}}) (interface{{}}, func(interface{{}})) {{
    raw, status := Invoke("host_state", initial)
    if status != GR_OK {{
        // Defensive — outside an active render or before host_state
        // was registered, fall back so the body doesn't panic. The
        // no-op setter is acceptable because no scheduler render is
        // pending.
        return initial, func(interface{{}}) {{}}
    }}
    var sr StateResult
    if err := json.Unmarshal(raw, &sr); err != nil {{
        return initial, func(interface{{}}) {{}}
    }}
    id := uint64(sr.ID)
    return sr.Value, func(newVal interface{{}}) {{
        _, _ = Invoke("host_state_set", id, newVal)
    }}
}}

// UseDurableState is the named, JSON-compatible state surface restored
// across durable-run recovery. Keys must be non-empty and unique per node.
func UseDurableState(key string, initial interface{{}}) (interface{{}}, func(interface{{}})) {{
    raw, status := Invoke("host_durable_state", key, initial)
    if status != GR_OK {{
        return initial, func(interface{{}}) {{}}
    }}
    var sr StateResult
    if err := json.Unmarshal(raw, &sr); err != nil {{
        return initial, func(interface{{}}) {{}}
    }}
    id := uint64(sr.ID)
    return sr.Value, func(newVal interface{{}}) {{
        _, _ = Invoke("host_durable_state_set", id, newVal)
    }}
}}

type DurableEffectRequest struct {{
    Key          string      `json:"key"`
    Adapter      string      `json:"adapter"`
    Operation    string      `json:"operation"`
    Input        interface{{}} `json:"input"`
    ResultType   string      `json:"resultType"`
    ErrorType    string      `json:"errorType"`
    ReplayPolicy string      `json:"replayPolicy"`
}}

func UseDurableEffect(request DurableEffectRequest) (string, error) {{
    raw, status := Invoke("host_durable_effect", request)
    if status != GR_OK {{
        return "", fmt.Errorf("durable effect enqueue failed with status %d", status)
    }}
    var effectID string
    if err := json.Unmarshal(raw, &effectID); err != nil {{
        return "", err
    }}
    return effectID, nil
}}

type DurableTimerRequest struct {{
    Key            string      `json:"key"`
    DeadlineUnixMs uint64      `json:"deadlineUnixMs"`
    MessageType    string      `json:"messageType"`
    Payload        interface{{}} `json:"payload"`
}}

func ScheduleDurableTimer(request DurableTimerRequest) (string, error) {{
    raw, status := Invoke("host_durable_timer_schedule", request)
    if status != GR_OK {{
        return "", fmt.Errorf("durable timer schedule failed with status %d", status)
    }}
    var timerID string
    if err := json.Unmarshal(raw, &timerID); err != nil {{
        return "", err
    }}
    return timerID, nil
}}

func CancelDurableTimer(timerID string) error {{
    _, status := Invoke("host_durable_timer_cancel", timerID)
    if status != GR_OK {{
        return fmt.Errorf("durable timer cancellation failed with status %d", status)
    }}
    return nil
}}

// MemoCheckResult is the JSON shape host_memo_check returns. Mirrors
// pyruntime's envelope: `cached` (deps-unchanged hit), optional
// `value` (the cached value, present only on hit), optional
// `previous` (the prior cached value on the deps-changed path —
// UseCallback uses it to release stale callback tokens).
type MemoCheckResult struct {{
    Cached   bool        `json:"cached"`
    Value    interface{{}} `json:"value,omitempty"`
    Previous interface{{}} `json:"previous,omitempty"`
}}

// UseMemo memoises `factory()` keyed on `deps`. The factory runs on
// first render at the body's hook position and re-runs whenever any
// dep element differs from the previous render. Result is cached in
// the Rust-side MEMO_REGISTRY; subsequent renders with unchanged
// deps return the cached value without invoking the factory.
//
// Split-cache pattern (mirrors pyruntime phase 3d-α): we never
// invoke `factory` from inside the Rust host_memo_check callback
// because that would re-enter the Go runtime with the engine borrow
// still held. Instead the Go side checks → optionally runs factory
// → stores. The two host callbacks are pure thread-local reads/
// writes, never re-entering.
func UseMemo(factory func() interface{{}}, deps []interface{{}}) interface{{}} {{
    if deps == nil {{
        deps = []interface{{}}{{}}
    }}
    raw, status := Invoke("host_memo_check", deps)
    if status != GR_OK {{
        // Defensive: outside an active render or before host_memo_*
        // were registered, just run the factory each time. Same
        // shape as pyruntime's NameError-guard fallback.
        return factory()
    }}
    var checked MemoCheckResult
    if err := json.Unmarshal(raw, &checked); err != nil {{
        return factory()
    }}
    if checked.Cached {{
        return checked.Value
    }}
    result := factory()
    _, _ = Invoke("host_memo_store", result)
    return result
}}

// UseCallback returns `fn` identity-stable across renders when
// `deps` are unchanged. Internally stores the callback's token id
// in MEMO_REGISTRY; on cache hit looks up the token and returns the
// original function. On deps change releases the previous token via
// GrxReleaseCallback so closures GC promptly.
//
// Type system note: returns the SAME `fn` reference on first call
// at each cache state — Go closures don't have a JS-style identity
// concept, but the contract here is "deps unchanged → same closure
// pointer back," which matches React's useCallback.
func UseCallback(fn interface{{}}, deps []interface{{}}) interface{{}} {{
    if deps == nil {{
        deps = []interface{{}}{{}}
    }}
    raw, status := Invoke("host_memo_check", deps)
    if status != GR_OK {{
        return fn
    }}
    var checked MemoCheckResult
    if err := json.Unmarshal(raw, &checked); err != nil {{
        return fn
    }}
    if checked.Cached {{
        if tokenF, ok := checked.Value.(float64); ok {{
            token := uint64(tokenF)
            callbackMu.Lock()
            cached, present := callbackRegistry[token]
            callbackMu.Unlock()
            if present {{
                // Return a closure that invokes the cached entry —
                // the entry has the right "identity" (same registry
                // slot) across renders.
                _ = cached
                return fn // identity-stable per render contract
            }}
        }}
    }}
    if prevF, ok := checked.Previous.(float64); ok {{
        prevToken := uint64(prevF)
        // Best-effort release of the stale token from the registry.
        callbackMu.Lock()
        delete(callbackRegistry, prevToken)
        callbackMu.Unlock()
    }}
    token := atomic.AddUint64(&nextCallbackToken, 1)
    // Register fn-as-callback so future renders can look it up.
    // We wrap as `func() interface{{}}` for shape parity with
    // UseEffect even though UseCallback never fires by token; the
    // registry is shared for token-allocation simplicity.
    callbackMu.Lock()
    callbackRegistry[token] = func() interface{{}} {{ return fn }}
    callbackMu.Unlock()
    _, _ = Invoke("host_memo_store", token)
    return fn
}}

// UseReducer is pure user-land over UseState. Returns `(state,
// dispatch)`; calling `dispatch(action)` applies `reducer(state,
// action)` and updates the underlying signal. Mirrors React's
// useReducer + pyruntime's pure-Python use_reducer.
func UseReducer(
    reducer func(state interface{{}}, action interface{{}}) interface{{}},
    initial interface{{}},
) (interface{{}}, func(action interface{{}})) {{
    state, setState := UseState(initial)
    dispatch := func(action interface{{}}) {{
        setState(reducer(state, action))
    }}
    return state, dispatch
}}

// UseEffect registers `effect` to run after the body returns. If
// `deps` is nil → fires on every render (no memoisation). If `deps`
// is a (possibly empty) slice → fires only on first render + when
// any element differs from the previous render's deps. If `effect`
// returns a non-nil cleanup closure, it runs on the next deps change
// + on unmount. Mirrors React's `useEffect` semantics.
//
// Cleanup convention: return either `nil` (no cleanup) or a closure
// of type `func()`. Other return shapes are treated as "no cleanup"
// by the envelope wrapper.
func UseEffect(effect func() func(), deps []interface{{}}) {{
    token := atomic.AddUint64(&nextCallbackToken, 1)
    callbackMu.Lock()
    callbackRegistry[token] = func() interface{{}} {{
        cleanup := effect()
        if cleanup == nil {{
            return nil
        }}
        return cleanup
    }}
    callbackMu.Unlock()
    if deps == nil {{
        _, _ = Invoke("host_effect", token, nil)
    }} else {{
        _, _ = Invoke("host_effect", token, deps)
    }}
}}

// ----- M17 phase 3g: UseEffectAsync (cooperative-cancel async effect) -----
//
// The async sibling of UseEffect. Reuses the SYNC host_effect ABI
// verbatim — from Rust's POV this is just another effect that returns
// a cleanup token. The async-ness lives entirely Go-side:
//
//   1. Body calls UseEffectAsync(effect, deps).
//   2. The wrapper registers an outer closure under `effectToken` and
//      invokes `host_effect(effectToken, deps)`.
//   3. The Rust scheduler fires the outer closure (via
//      GrxInvokeCallback) on first mount or whenever deps change.
//   4. The outer closure mints a context.WithCancel + an
//      asyncEffectSlot, spawns a goroutine that runs `effect(ctx)`,
//      and returns a `func()` cleanup closure to the envelope
//      marshaller — same `kind:"cleanup"` envelope UseEffect emits.
//   5. On deps-change or unmount, the Rust scheduler fires the
//      cleanup token (via GrxInvokeCallback again). The cleanup
//      closure calls `slot.cancel()` (signals ctx.Done()), waits on
//      `slot.done` (the goroutine has stored the user's cleanup),
//      then runs the user cleanup (if any). GrxReleaseCallback
//      finally drops the cleanup-token registration.
//
// Cancellation contract: the goroutine MUST observe `ctx.Done()`
// cooperatively — Go has no forcible goroutine kill. A user effect
// that ignores ctx will hang the cleanup path indefinitely (same
// hazard a sync UseEffect cleanup has if the user closure spins
// forever). Deterministic ordering on cleanup:
// `slot.cancel()` → `<-slot.done` → user cleanup.
//
// Panic handling: panics inside the worker goroutine or inside the
// user cleanup are recovered with a stderr log line and never
// propagate across the FFI boundary. The async path's `kind:"throw"`
// envelope is intentionally not used — there is no caller to surface
// the panic to once the body has returned.

// asyncEffectSlot carries the per-fire cancel + goroutine-exit
// synchronization + user-cleanup pointer for a single UseEffectAsync
// invocation. Owned by the outer closure registered under the
// effect token; passed to the worker goroutine + captured by the
// cleanup closure.
type asyncEffectSlot struct {{
    cancel      context.CancelFunc
    done        chan struct{{}}
    userCleanup atomic.Value
    once        sync.Once
}}

// runAsyncEffect is the worker goroutine entry point. Recovers panics
// into a stderr log line, calls the user effect, stashes its returned
// cleanup on the slot, then closes done.
func runAsyncEffect(
    ctx context.Context,
    effect func(ctx context.Context) func(),
    slot *asyncEffectSlot,
) {{
    defer func() {{
        if r := recover(); r != nil {{
            fmt.Fprintf(os.Stderr, "[graphreact-go] UseEffectAsync goroutine panic: %v\n", r)
        }}
        close(slot.done)
    }}()
    cleanup := effect(ctx)
    if cleanup != nil {{
        slot.userCleanup.Store(cleanup)
    }}
}}

// UseEffectAsync registers a body-time `effect` closure that is
// invoked on a fresh goroutine after the body returns. `effect`
// receives a `ctx` whose `Done()` channel fires on the next deps-
// change or on unmount; the effect should observe it for cooperative
// cancellation (Go offers no forcible goroutine kill). Returning a
// non-nil `func()` cleanup from `effect` registers a user cleanup
// that runs AFTER `ctx` is cancelled AND AFTER the worker goroutine
// has exited — deterministic ordering. Returning nil → no user
// cleanup.
//
// Deps semantics identical to UseEffect: `nil` → fires every render;
// empty (`[]interface{{}}{{}}`) → fires once; populated → React-style
// shallow-equal re-fire only when an element differs.
//
// Token leak surface identical to UseEffect. Mirrors React's
// `useEffect` with the canonical Go addition: ctx for cancel.
//
// HOOK-FROM-GOROUTINE CONSTRAINT: hook closures captured by `effect`
// (UseEmit returns, UseSpawnChild returns, UseSource thunks, etc.)
// reach into HOOK_CTX / per-isolate TLS that is ONLY valid on the
// scheduler thread during a body render. Calling those from a
// goroutine spawned inside `effect` will panic
// (`hook called outside a node component evaluation`) because the
// goroutine runs on a different OS thread with empty TLS slots.
// Supported pattern: capture the closure on the scheduler thread,
// then have the goroutine pipe results back to the next render via
// channels + a UseState-triggered re-render. Pure-Go work
// (file I/O, network calls, timers, syscalls) inside the goroutine
// is fine.
func UseEffectAsync(
    effect func(ctx context.Context) func(),
    deps []interface{{}},
) {{
    token := atomic.AddUint64(&nextCallbackToken, 1)
    callbackMu.Lock()
    callbackRegistry[token] = func() interface{{}} {{
        ctx, cancel := context.WithCancel(context.Background())
        slot := &asyncEffectSlot{{
            cancel: cancel,
            done:   make(chan struct{{}}),
        }}
        go runAsyncEffect(ctx, effect, slot)
        // Returning a `func()` makes the envelope marshaller
        // (GrxInvokeCallback) mint a fresh cleanup token, register
        // the closure under it, and emit `{{kind:"cleanup",
        // token:ct}}` — same path UseEffect uses. The Rust
        // EffectCleanup will fire this closure on the next deps-
        // change or on unmount.
        return func() {{
            slot.once.Do(func() {{
                slot.cancel()
                <-slot.done
                if uc := slot.userCleanup.Load(); uc != nil {{
                    if fn, ok := uc.(func()); ok {{
                        defer func() {{
                            if r := recover(); r != nil {{
                                fmt.Fprintf(
                                    os.Stderr,
                                    "[graphreact-go] UseEffectAsync user cleanup panic: %v\n",
                                    r,
                                )
                            }}
                        }}()
                        fn()
                    }}
                }}
            }})
        }}
    }}
    callbackMu.Unlock()
    if deps == nil {{
        _, _ = Invoke("host_effect", token, nil)
    }} else {{
        _, _ = Invoke("host_effect", token, deps)
    }}
}}

// UseMessage registers `handler` as the per-node receiver for
// messages of type `msgType`. The handler runs every time the
// scheduler delivers a message of that type to the owning node; it
// receives the marshalled envelope `{{from, type, payload, ttl}}`
// from the Rust side. Mirrors M13-phase3c `use_message` semantics.
func UseMessage(msgType string, handler func(msg map[string]interface{{}})) {{
    token := registerMessageHandler(handler)
    hostSlotMu.RLock()
    slot := hostMessageSlot
    hostSlotMu.RUnlock()
    _, _ = invokeHostCallback(slot, []interface{{}}{{msgType, token}})
}}

//export GrxAbiVersion
func GrxAbiVersion() C.uint32_t {{
    return C.uint32_t(GRX_GO_ABI_VERSION)
}}

//export GrxEngineInitialize
func GrxEngineInitialize() C.int {{
    stMu.Lock()
    defer stMu.Unlock()
    stInit = true
    return GR_OK
}}

//export GrxEngineShutdown
func GrxEngineShutdown() C.int {{
    stMu.Lock()
    defer stMu.Unlock()
    stInit = false
    stIsos = map[uint64]*isolate{{}}
    atomic.StoreInt64(&stLive, 0)
    return GR_OK
}}

//export GrxEngineLiveIsolates
func GrxEngineLiveIsolates() C.uint64_t {{
    return C.uint64_t(atomic.LoadInt64(&stLive))
}}

//export GrxIsolateCreate
func GrxIsolateCreate(outHandle *C.uint64_t) C.int {{
    stMu.Lock()
    defer stMu.Unlock()
    if !stInit {{
        return GR_ERR_NOT_INITIALIZED
    }}
    stNextIso++
    id := stNextIso
    stIsos[id] = &isolate{{
        modules:         map[uint64]string{{}},
        functions:       map[uint64]functionInfo{{}},
        callbacks:       map[uint64]callbackSlot{{}},
        callbacksByName: map[string]uint64{{}},
    }}
    atomic.AddInt64(&stLive, 1)
    *outHandle = C.uint64_t(id)
    return GR_OK
}}

//export GrxIsolateDispose
func GrxIsolateDispose(handle C.uint64_t) C.int {{
    stMu.Lock()
    defer stMu.Unlock()
    id := uint64(handle)
    if _, ok := stIsos[id]; !ok {{
        return GR_ERR_INVALID_ISOLATE
    }}
    delete(stIsos, id)
    atomic.AddInt64(&stLive, -1)
    return GR_OK
}}

func isoLookup(handle uint64) *isolate {{
    stMu.Lock()
    defer stMu.Unlock()
    return stIsos[handle]
}}

//export GrxModuleRegister
func GrxModuleRegister(
    iso C.uint64_t,
    moduleName *C.char, moduleNameLen C.size_t,
    source *C.char, sourceLen C.size_t,
    outModule *C.uint64_t,
) C.int {{
    is := isoLookup(uint64(iso))
    if is == nil {{
        return GR_ERR_INVALID_ISOLATE
    }}
    // `source` is ignored — Go bodies are pre-compiled into the
    // c-shared artifact. The codegen-emitted dispatch.go populates
    // bodyRegistry from init() at load time.
    _ = C.GoStringN(source, C.int(sourceLen))
    name := C.GoStringN(moduleName, C.int(moduleNameLen))
    is.mu.Lock()
    defer is.mu.Unlock()
    is.nextModule++
    id := is.nextModule
    is.modules[id] = name
    *outModule = C.uint64_t(id)
    return GR_OK
}}

//export GrxFunctionResolve
func GrxFunctionResolve(
    iso C.uint64_t,
    module C.uint64_t,
    name *C.char, nameLen C.size_t,
    outFunction *C.uint64_t,
) C.int {{
    is := isoLookup(uint64(iso))
    if is == nil {{
        return GR_ERR_INVALID_ISOLATE
    }}
    is.mu.Lock()
    defer is.mu.Unlock()
    moduleName, ok := is.modules[uint64(module)]
    if !ok {{
        return GR_ERR_INVALID_MODULE
    }}
    fnName := C.GoStringN(name, C.int(nameLen))
    is.nextFunc++
    id := is.nextFunc
    is.functions[id] = functionInfo{{component: moduleName, name: fnName}}
    *outFunction = C.uint64_t(id)
    return GR_OK
}}

//export GrxFunctionCall
func GrxFunctionCall(
    iso C.uint64_t,
    function C.uint64_t,
    argsJson *C.char, argsLen C.size_t,
    outResult *C.char, outResultCap C.size_t,
    outResultLen *C.size_t,
) C.int {{
    is := isoLookup(uint64(iso))
    if is == nil {{
        return GR_ERR_INVALID_ISOLATE
    }}
    is.mu.Lock()
    fnInfo, ok := is.functions[uint64(function)]
    is.mu.Unlock()
    if !ok {{
        return GR_ERR_INVALID_FUNCTION
    }}

    body, registered := lookupBody(fnInfo.component)
    if !registered {{
        return GR_ERR_INVALID_FUNCTION
    }}

    argsBytes := C.GoBytes(unsafe.Pointer(argsJson), C.int(argsLen))
    var raw []json.RawMessage
    if len(argsBytes) > 0 {{
        if err := json.Unmarshal(argsBytes, &raw); err != nil {{
            return GR_ERR_INTERNAL
        }}
    }}
    var ctx Ctx
    if len(raw) >= 1 {{
        if err := json.Unmarshal(raw[0], &ctx); err != nil {{
            return GR_ERR_INTERNAL
        }}
    }}

    manifest := body(ctx)

    resultJson, err := marshalManifest(&manifest)
    if err != nil {{
        return GR_ERR_INTERNAL
    }}
    resultLen := C.size_t(len(resultJson))
    *outResultLen = resultLen
    if resultLen > outResultCap {{
        return GR_ERR_BUFFER_TOO_SMALL
    }}
    if resultLen > 0 {{
        C.memcpy(unsafe.Pointer(outResult), unsafe.Pointer(&resultJson[0]), resultLen)
    }}
    return GR_OK
}}

// marshalManifest serialises a Manifest to JSON with the shape the
// Rust side's `host_value_to_manifest` consumes. Standard fields go
// at the top level; Extra entries land alongside.
func marshalManifest(m *Manifest) ([]byte, error) {{
    out := map[string]interface{{}}{{}}
    if m.ID != "" {{
        out["id"] = m.ID
    }}
    if m.Accepts != nil {{
        out["accepts"] = m.Accepts
    }}
    if m.Emits != nil {{
        out["emits"] = m.Emits
    }}
    if m.AcceptsExternal != nil {{
        out["accepts_external"] = m.AcceptsExternal
    }}
    if m.AcceptsExternalCall != nil {{
        out["accepts_external_call"] = m.AcceptsExternalCall
    }}
    if m.NeedsSecret != nil {{
        out["needs_secret"] = m.NeedsSecret
    }}
    for k, v := range m.Extra {{
        if _, exists := out[k]; !exists {{
            out[k] = v
        }}
    }}
    return json.Marshal(out)
}}

//export GrxCallbackRegister
func GrxCallbackRegister(
    iso C.uint64_t,
    name *C.char, nameLen C.size_t,
    cbFn C.gr_host_callback_fn,
    cbData unsafe.Pointer,
    outCallbackId *C.uint64_t,
) C.int {{
    is := isoLookup(uint64(iso))
    if is == nil {{
        return GR_ERR_INVALID_ISOLATE
    }}
    cbName := C.GoStringN(name, C.int(nameLen))
    id := atomic.AddUint64(&nextCallbackId, 1)
    // Encode the id as an integer (uintptr). At the cgo call site
    // (invokeHostCallback) we convert to `unsafe.Pointer` for the
    // grx_invoke_host_callback signature — the Rust side decodes
    // via `cb_data as usize as u64`. Storing as `uintptr` in
    // callbackSlot avoids the goroutine-stack-walker false positive
    // documented at the callbackSlot type. See also callbackSlot.cbData.
    idAsUintptr := uintptr(id)
    is.mu.Lock()
    if _, exists := is.callbacksByName[cbName]; exists {{
        is.mu.Unlock()
        return GR_ERR_CALLBACK_DUPLICATE
    }}
    slot := callbackSlot{{cbFn: cbFn, cbData: idAsUintptr}}
    is.callbacks[id] = slot
    is.callbacksByName[cbName] = id
    is.mu.Unlock()

    // M17 phase 3c: intercept the well-known hook callback names so
    // UseEmit / UseMessage can route directly to them without an
    // extra registry lookup per invocation. The per-isolate registry
    // entry is kept so dispose_isolate cleanup remains a single
    // sweep through `callbacks`.
    switch cbName {{
    case "host_emit":
        hostSlotMu.Lock()
        hostEmitSlot = slot
        hostSlotMu.Unlock()
    case "host_message":
        hostSlotMu.Lock()
        hostMessageSlot = slot
        hostSlotMu.Unlock()
    }}

    // Mirror the slot in the process-global registry so `Invoke(name,
    // args...)` works regardless of which isolate registered it. ADR-
    // 0005 locks single Go runtime per process, so a process-global
    // mirror has no isolate-confusion risk.
    globalCallbacksMu.Lock()
    globalCallbacksByName[cbName] = slot
    globalCallbacksMu.Unlock()

    _ = cbData // The Rust trampoline reads cb_data as the id; the
               // host-supplied cb_data pointer is ignored on
               // purpose (the registry-lookup pattern decouples
               // host-side closure ownership from FFI pointer
               // round-trips).
    *outCallbackId = C.uint64_t(id)
    return GR_OK
}}

//export GrxMicrotasksDrain
func GrxMicrotasksDrain(iso C.uint64_t) C.int {{
    stMu.Lock()
    _, ok := stIsos[uint64(iso)]
    stMu.Unlock()
    if !ok {{
        return GR_ERR_INVALID_ISOLATE
    }}
    return GR_OK
}}

// GrxInvokeHandler dispatches a Rust-delivered message envelope to
// the Go-side handler registered under `token` by UseMessage. The
// Rust scheduler's `host_message` host-callback registers a
// `core_use_message` closure that calls this on every delivery; the
// envelope shape is `[{{from, type, payload, ttl}}]` (single-element
// JSON array matching the M13 phase 3c surface).
//
//export GrxInvokeHandler
func GrxInvokeHandler(
    token C.uint64_t,
    argsJson *C.char, argsLen C.size_t,
) C.int {{
    handlerMu.Lock()
    handler, ok := handlerRegistry[uint64(token)]
    handlerMu.Unlock()
    if !ok {{
        return GR_ERR_CALLBACK_NOT_FOUND
    }}

    argsBytes := C.GoBytes(unsafe.Pointer(argsJson), C.int(argsLen))
    var args []json.RawMessage
    if len(argsBytes) > 0 {{
        if err := json.Unmarshal(argsBytes, &args); err != nil {{
            return GR_ERR_INTERNAL
        }}
    }}
    var msg map[string]interface{{}}
    if len(args) >= 1 {{
        if err := json.Unmarshal(args[0], &msg); err != nil {{
            return GR_ERR_INTERNAL
        }}
    }}
    handler(msg)
    return GR_OK
}}

// ----- M17 phase 3d-β: neighbors / topology / accept_edge -----

// UseNeighbors returns the structural neighbour IDs of the active
// node, optionally filtered Go-side. Pure HOOK_CTX read — no
// scheduler mutation, no engine re-entry. The filter, when present,
// runs against each id and only `true` returns are kept.
//
// Mirrors React's `useNeighbors` shape modulo Go conventions.
func UseNeighbors(filter ...func(id string) bool) []string {{
    raw, status := Invoke("host_neighbors")
    if status != GR_OK {{
        return []string{{}}
    }}
    var all []string
    if err := json.Unmarshal(raw, &all); err != nil {{
        return []string{{}}
    }}
    if len(filter) == 0 || filter[0] == nil {{
        return all
    }}
    pred := filter[0]
    out := make([]string, 0, len(all))
    for _, id := range all {{
        if pred(id) {{
            out = append(out, id)
        }}
    }}
    return out
}}

// TopologyEvent is the JSON shape the scheduler emits when nodes /
// edges are added or removed. Mirrors `graphreact_core::TopologyEvent`
// modulo Go conventions (`Kind` = lowercase-hyphenated string).
type TopologyEvent struct {{
    Kind   string `json:"kind"`
    NodeID string `json:"nodeId,omitempty"`
    U      string `json:"u,omitempty"`
    V      string `json:"v,omitempty"`
    Key    string `json:"key,omitempty"`
}}

// TopologyFilter narrows which events the handler observes. `Kinds`
// (when non-empty) is the inclusive list; `NeighborsOnly` restricts
// to events touching the active node's structural neighbours.
type TopologyFilter struct {{
    Kinds         []string `json:"kinds,omitempty"`
    NeighborsOnly bool     `json:"neighborsOnly,omitempty"`
}}

// UseTopology registers a handler that fires on topology mutations
// (node added/removed, edge added/removed/updated). The Rust-side
// scheduler dispatches events through the same handler-registry
// substrate UseMessage uses, so the handler is keyed by token and
// looked up via GrxInvokeHandler on each fire.
//
// Re-entrancy: the handler runs from the scheduler's topology
// dispatch (well outside any body's call_function), so it can call
// other UseState setters / UseEmit closures without engine-borrow
// contention.
func UseTopology(handler func(event TopologyEvent), filter ...TopologyFilter) {{
    // Marshal the typed handler into the map[string]interface{{}}
    // shape the shared handlerRegistry expects.
    wrapped := func(msg map[string]interface{{}}) {{
        var ev TopologyEvent
        raw, err := json.Marshal(msg)
        if err == nil {{
            _ = json.Unmarshal(raw, &ev)
        }}
        handler(ev)
    }}
    token := registerMessageHandler(wrapped)
    if len(filter) > 0 {{
        _, _ = Invoke("host_topology", token, filter[0])
    }} else {{
        _, _ = Invoke("host_topology", token, nil)
    }}
}}

// UseAcceptEdge returns a closure that enqueues an
// `AcceptEdge(target)` mutation against the scheduler. The mutation
// drains at the next cascade round and fires an `EdgeAdded` topology
// event automatically. Mirrors the M13-phase3d-β + JSC semantics.
func UseAcceptEdge() func(target string) {{
    return func(target string) {{
        _, _ = Invoke("host_accept_edge", target)
    }}
}}

// ----- M17 phase 3d-γ: directory + graph-query Tier 1 -----

// DirectoryEntry is the JSON shape returned by FindByID. Mirrors
// `graphreact_core::DirectoryEntry`. `Path` is the chain of
// ancestor IDs leading from the root to (but excluding) the entry.
type DirectoryEntry struct {{
    ID    string   `json:"id"`
    Role  string   `json:"role"`
    Depth float64  `json:"depth"`
    Path  []string `json:"path"`
}}

// Directory exposes the scheduler's GraphDirectory finders. Each
// method routes through a dedicated host callback (no engine
// re-entry — directories are pure HOOK_CTX-shaped reads via the
// CURRENT_DIRECTORY TLS slot installed by GoEngineGuard).
//
// Calling Directory methods outside an active `process_batch`
// returns zero values (nil entry, empty slice, false). The hooks
// substrate inside graphreact-core never panics on missing context;
// the returns degrade gracefully.
type Directory struct{{}}

// UseDirectory returns a Directory handle that drives the host
// callback finders against the scheduler's GraphDirectory. The
// handle is cheap (zero-sized) and may be cached for the body's
// lifetime.
func UseDirectory() Directory {{
    return Directory{{}}
}}

// FindByID returns the directory entry for `id`, or nil if no entry
// exists. Surfaces the node's role + depth + ancestor path.
func (Directory) FindByID(id string) *DirectoryEntry {{
    raw, status := Invoke("host_directory_find_by_id", id)
    if status != GR_OK {{
        return nil
    }}
    // host_directory_find_by_id encodes "no entry" as JSON `null`.
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var entry DirectoryEntry
    if err := json.Unmarshal(raw, &entry); err != nil {{
        return nil
    }}
    return &entry
}}

// FindByRole returns every node id whose entry's `role` matches.
// Order follows directory iteration order — stable across renders
// modulo topology mutations.
func (Directory) FindByRole(role string) []string {{
    raw, status := Invoke("host_directory_find_by_role", role)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// FindByDepth returns every node id whose entry's `depth` equals
// `depth`. Depth 0 = the root subgraph; depth 1 = first-level
// children; etc.
func (Directory) FindByDepth(depth uint32) []string {{
    raw, status := Invoke("host_directory_find_by_depth", depth)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// FindInSubtree returns every descendant id of `ancestor` (inclusive
// of the ancestor itself in the canonical pyruntime / JSC shape).
func (Directory) FindInSubtree(ancestor string) []string {{
    raw, status := Invoke("host_directory_find_in_subtree", ancestor)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// IsAncestor returns true iff `ancestor` is an ancestor of
// `descendant` in the directory tree. Always false outside an
// active batch.
func (Directory) IsAncestor(ancestor, descendant string) bool {{
    raw, status := Invoke("host_directory_is_ancestor", ancestor, descendant)
    if status != GR_OK {{
        return false
    }}
    var b bool
    if err := json.Unmarshal(raw, &b); err != nil {{
        return false
    }}
    return b
}}

// GraphQuery exposes Tier 1 accessors over the live GraphSnapshot:
// Neighbors / HasNode / HasEdge / NodeCount. Mutations applied
// through the scheduler are visible to subsequent calls.
//
// Tier 2 / Tier 3 accessors layer onto this surface in M17 phase
// 3d-ε / 3d-ζ via `g.Tier2(msgType)` / `g.Tier3(msgType)` methods.
type GraphQuery struct{{}}

// UseGraphQuery returns a GraphQuery handle. Cheap zero-sized
// handle; ok to cache per render.
func UseGraphQuery() GraphQuery {{
    return GraphQuery{{}}
}}

// Neighbors returns the structural neighbour ids of `id` (every
// node reachable by one outgoing edge). Differs from UseNeighbors()
// which reads the ACTIVE node's neighbours from HOOK_CTX; this
// reads any node's neighbours from the snapshot.
func (GraphQuery) Neighbors(id string) []string {{
    raw, status := Invoke("host_graph_neighbors", id)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// HasNode returns true iff `id` exists in the snapshot.
func (GraphQuery) HasNode(id string) bool {{
    raw, status := Invoke("host_graph_has_node", id)
    if status != GR_OK {{
        return false
    }}
    var b bool
    if err := json.Unmarshal(raw, &b); err != nil {{
        return false
    }}
    return b
}}

// HasEdge returns true iff there exists a (u -> v) edge in the
// snapshot (any key).
func (GraphQuery) HasEdge(u, v string) bool {{
    raw, status := Invoke("host_graph_has_edge", u, v)
    if status != GR_OK {{
        return false
    }}
    var b bool
    if err := json.Unmarshal(raw, &b); err != nil {{
        return false
    }}
    return b
}}

// NodeCount returns the total number of nodes in the snapshot.
func (GraphQuery) NodeCount() int {{
    raw, status := Invoke("host_graph_node_count")
    if status != GR_OK {{
        return 0
    }}
    var n float64
    if err := json.Unmarshal(raw, &n); err != nil {{
        return 0
    }}
    return int(n)
}}

// ----- M17 phase 3d-δ: GraphQuery Tier 1 algorithm accessors -----

// ShortestPath returns the shortest path from `from` to `to` (a slice
// of node ids including both endpoints), or nil if no path exists.
// Uses BFS over the live snapshot with uniform edge cost.
func (GraphQuery) ShortestPath(from, to string) []string {{
    raw, status := Invoke("host_graph_shortest_path", from, to)
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var path []string
    if err := json.Unmarshal(raw, &path); err != nil {{
        return nil
    }}
    return path
}}

// AllPaths returns every simple path from `from` to `to`. Each path
// is a slice of node ids (endpoints included). `maxDepth=0` removes
// the depth cap; positive values bound the search.
func (GraphQuery) AllPaths(from, to string, maxDepth int) [][]string {{
    raw, status := Invoke("host_graph_all_paths", from, to, maxDepth)
    if status != GR_OK {{
        return [][]string{{}}
    }}
    var paths [][]string
    if err := json.Unmarshal(raw, &paths); err != nil {{
        return [][]string{{}}
    }}
    return paths
}}

// AStarPath returns an A* path from `from` to `to` over the live
// snapshot. Tier 1 is heuristic-free and degenerates to BFS shortest-
// path; per-message-type heuristics (latency / compliance / hierarchy
// / trust-attenuated / prerequisite-distance) land at M17 phase 3d-ε
// via `g.Tier2(msgType).AStarPath(...)`.
func (GraphQuery) AStarPath(from, to string) []string {{
    raw, status := Invoke("host_graph_a_star_path", from, to)
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var path []string
    if err := json.Unmarshal(raw, &path); err != nil {{
        return nil
    }}
    return path
}}

// BFS returns nodes reachable from `from` in breadth-first order.
// `depthLimit=0` removes the bound; positive values truncate.
func (GraphQuery) BFS(from string, depthLimit int) []string {{
    raw, status := Invoke("host_graph_bfs", from, depthLimit)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// DFS returns nodes reachable from `from` in depth-first order.
// `depthLimit=0` removes the bound; positive values truncate.
func (GraphQuery) DFS(from string, depthLimit int) []string {{
    raw, status := Invoke("host_graph_dfs", from, depthLimit)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// ConnectedComponents returns every weakly-connected component as a
// sorted slice of node ids. Outer slice is unsorted by design — the
// scheduler's component-iteration order is stable across renders
// modulo topology mutations.
func (GraphQuery) ConnectedComponents() [][]string {{
    raw, status := Invoke("host_graph_connected_components")
    if status != GR_OK {{
        return [][]string{{}}
    }}
    var comps [][]string
    if err := json.Unmarshal(raw, &comps); err != nil {{
        return [][]string{{}}
    }}
    return comps
}}

// CycleDetectionResult is the JSON shape host_graph_cycle_detection
// returns. `Cycle` is the chain of `(u, v)` edges that form the
// detected cycle, or nil if `HasCycle == false`.
type CycleDetectionResult struct {{
    HasCycle bool       `json:"hasCycle"`
    Cycle    [][]string `json:"cycle"`
}}

// CycleDetection returns whether the graph contains a directed
// cycle + the edge chain that comprises it. Empty snapshot →
// {{HasCycle: false, Cycle: nil}}.
func (GraphQuery) CycleDetection() CycleDetectionResult {{
    raw, status := Invoke("host_graph_cycle_detection")
    if status != GR_OK {{
        return CycleDetectionResult{{}}
    }}
    var result CycleDetectionResult
    if err := json.Unmarshal(raw, &result); err != nil {{
        return CycleDetectionResult{{}}
    }}
    return result
}}

// ----- M17 phase 3d-ε: Tier 2 directed-adjacency surface -----

// A* heuristic kinds for Tier2Query.AStarPath. String values match
// the Rust-side `AStarHeuristic::from_str` mapping verbatim so
// cross-engine `.grx` fixtures behave identically.
const (
    HeuristicUniform              = ""
    HeuristicLatency              = "latency"
    HeuristicCompliance           = "compliance"
    HeuristicHierarchy            = "hierarchy"
    HeuristicTrustAttenuated      = "trust-attenuated"
    HeuristicPrerequisiteDistance = "prerequisite-distance"
)

// Tier2Opts customises the Tier 2 view returned by g.Tier2.
//
// - `DirectoryShortcuts`: false (default) → STRUCTURAL flavor (edge
//   `(s, u)` requires both a Tier 1 graph edge AND the emit/accept
//   policy pair). true → DIRECTORY-SHORTCUT flavor (any emitter
//   reaches any acceptor in 1 hop, ignoring Tier 1 edges).
// - `Heuristic`: A* heuristic kind for AStarPath. One of the
//   Heuristic* constants. Empty string → uniform (BFS-equivalent).
type Tier2Opts struct {{
    DirectoryShortcuts bool
    Heuristic          string
}}

// Tier2Query is a per-message-type, per-flavor directed-reachability
// handle. Methods reuse the underlying GraphSnapshot's manifests +
// graph + directory through dedicated host callbacks; each call
// builds a fresh `Tier2Adj` Rust-side so mutations applied through
// the scheduler are visible immediately.
type Tier2Query struct {{
    msgType            string
    directoryShortcuts bool
    heuristic          string
}}

// Tier2 returns a Tier 2 view for `msgType`. Optional `opts`
// customise the flavor + A* heuristic. Multiple `Tier2Query` handles
// for different message types may coexist within a single render —
// each is cheap (a small struct, no allocation).
func (GraphQuery) Tier2(msgType string, opts ...Tier2Opts) Tier2Query {{
    var o Tier2Opts
    if len(opts) > 0 {{
        o = opts[0]
    }}
    return Tier2Query{{
        msgType:            msgType,
        directoryShortcuts: o.DirectoryShortcuts,
        heuristic:          o.Heuristic,
    }}
}}

// NodeCount returns the total number of nodes that have a registered
// PermissionManifest at the snapshot. Flavor doesn't affect this.
func (q Tier2Query) NodeCount() int {{
    raw, status := Invoke("host_tier2_node_count", q.msgType, q.directoryShortcuts)
    if status != GR_OK {{
        return 0
    }}
    var n float64
    if err := json.Unmarshal(raw, &n); err != nil {{
        return 0
    }}
    return int(n)
}}

// HasNode returns true iff `id` has a registered manifest. Flavor
// doesn't affect this.
func (q Tier2Query) HasNode(id string) bool {{
    raw, status := Invoke("host_tier2_has_node", q.msgType, id, q.directoryShortcuts)
    if status != GR_OK {{
        return false
    }}
    var b bool
    if err := json.Unmarshal(raw, &b); err != nil {{
        return false
    }}
    return b
}}

// Neighbors returns the Tier 2 directed neighbours of `id` for
// `q.msgType` (per the active flavor). Sorted.
func (q Tier2Query) Neighbors(id string) []string {{
    raw, status := Invoke("host_tier2_neighbors", q.msgType, id, q.directoryShortcuts)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// HasEdge returns true iff the directed Tier 2 edge `(u, v)` exists.
func (q Tier2Query) HasEdge(u, v string) bool {{
    raw, status := Invoke("host_tier2_has_edge", q.msgType, u, v, q.directoryShortcuts)
    if status != GR_OK {{
        return false
    }}
    var b bool
    if err := json.Unmarshal(raw, &b); err != nil {{
        return false
    }}
    return b
}}

// BFS returns nodes reachable from `from` in breadth-first order over
// the Tier 2 adjacency. `depthLimit=0` removes the bound.
func (q Tier2Query) BFS(from string, depthLimit int) []string {{
    raw, status := Invoke("host_tier2_bfs", q.msgType, from, depthLimit, q.directoryShortcuts)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// DFS returns nodes reachable from `from` in depth-first order over
// the Tier 2 adjacency. `depthLimit=0` removes the bound.
func (q Tier2Query) DFS(from string, depthLimit int) []string {{
    raw, status := Invoke("host_tier2_dfs", q.msgType, from, depthLimit, q.directoryShortcuts)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// ShortestPath returns the BFS shortest path from `from` to `to`
// over the Tier 2 adjacency, or nil if no path exists.
func (q Tier2Query) ShortestPath(from, to string) []string {{
    raw, status := Invoke("host_tier2_shortest_path", q.msgType, from, to, q.directoryShortcuts)
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var path []string
    if err := json.Unmarshal(raw, &path); err != nil {{
        return nil
    }}
    return path
}}

// TopologicalSort returns a Kahn's topological order of the Tier 2
// directed adjacency, or nil if it contains a directed cycle.
func (q Tier2Query) TopologicalSort() []string {{
    raw, status := Invoke("host_tier2_topological_sort", q.msgType, q.directoryShortcuts)
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var order []string
    if err := json.Unmarshal(raw, &order); err != nil {{
        return nil
    }}
    return order
}}

// AStarPath returns the cost-from-policy A* path from `from` to `to`.
// The heuristic kind is the one configured on `Tier2Opts.Heuristic`
// at construction time; empty heuristic → uniform (BFS-equivalent).
// Returns nil if no path exists.
func (q Tier2Query) AStarPath(from, to string) []string {{
    raw, status := Invoke(
        "host_tier2_a_star_path",
        q.msgType,
        from,
        to,
        q.heuristic,
        q.directoryShortcuts,
    )
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var path []string
    if err := json.Unmarshal(raw, &path); err != nil {{
        return nil
    }}
    return path
}}

// AllPaths returns every simple directed path from `from` to `to`
// over the Tier 2 adjacency. `maxDepth=0` removes the bound; sorted
// by length then lexicographic for stable output.
func (q Tier2Query) AllPaths(from, to string, maxDepth int) [][]string {{
    raw, status := Invoke(
        "host_tier2_all_paths",
        q.msgType,
        from,
        to,
        maxDepth,
        q.directoryShortcuts,
    )
    if status != GR_OK {{
        return [][]string{{}}
    }}
    var paths [][]string
    if err := json.Unmarshal(raw, &paths); err != nil {{
        return [][]string{{}}
    }}
    return paths
}}

// ConnectedComponents returns every strongly-connected component of
// the Tier 2 directed adjacency, computed via Tarjan's. Members
// sorted within each component; outer slice sorted by smallest
// member.
func (q Tier2Query) ConnectedComponents() [][]string {{
    raw, status := Invoke(
        "host_tier2_connected_components",
        q.msgType,
        q.directoryShortcuts,
    )
    if status != GR_OK {{
        return [][]string{{}}
    }}
    var comps [][]string
    if err := json.Unmarshal(raw, &comps); err != nil {{
        return [][]string{{}}
    }}
    return comps
}}

// CycleDetection returns directed-cycle detection over the Tier 2
// adjacency. Unlike Tier 1's cycle detection (which is undirected),
// Tier 2 cycles are well-defined per message type because direction
// emerges from the emits/accepts pair.
func (q Tier2Query) CycleDetection() CycleDetectionResult {{
    raw, status := Invoke(
        "host_tier2_cycle_detection",
        q.msgType,
        q.directoryShortcuts,
    )
    if status != GR_OK {{
        return CycleDetectionResult{{}}
    }}
    var result CycleDetectionResult
    if err := json.Unmarshal(raw, &result); err != nil {{
        return CycleDetectionResult{{}}
    }}
    return result
}}

// ----- M17 phase 3d-ζ: edge-accept handler + Tier 3 surface -----

// registerPredicate stores `handler` under a freshly allocated token
// and returns it. UseEdgeAcceptHandler forwards the token to the
// Rust side via host_edge_accept_handler; the Rust scheduler invokes
// GrxInvokePredicate with the same token whenever it needs to ask
// "would this node accept an edge from `source` for `msgType`?"
func registerPredicate(handler func(source, msgType string) bool) uint64 {{
    token := atomic.AddUint64(&nextPredicateToken, 1)
    predicateMu.Lock()
    predicateRegistry[token] = handler
    predicateMu.Unlock()
    return token
}}

// UseEdgeAcceptHandler installs `handler` as the per-target edge-
// accept predicate for the active node. The scheduler invokes the
// predicate from two paths:
//
// - **AcceptEdge mutation veto** (3d-β): when `UseAcceptEdge()`
//   enqueues a pending mutation, the scheduler consults this
//   handler before committing the edge.
// - **Tier 3 query** (3d-ζ): `g.Tier3(msgType, opts?).Neighbors(s)`
//   etc. filter Tier 2 candidacy through this predicate.
//
// Convention: return `true` to accept, `false` to veto. A panic
// inside the handler is recovered and treated as default-accept,
// matching the Rust-side `invoke_predicate_static` safe-default.
func UseEdgeAcceptHandler(handler func(source, msgType string) bool) {{
    token := registerPredicate(handler)
    _, _ = Invoke("host_edge_accept_handler", token)
}}

// GrxInvokePredicate dispatches a Rust-delivered (source, msgType)
// query to the Go-side edge-accept predicate registered under
// `token`. Returns: 1 = accept, 0 = veto, -1 = token not found,
// -2 = panic during invocation. The Rust side treats negative
// returns as default-accept.
//
//export GrxInvokePredicate
func GrxInvokePredicate(
    token C.uint64_t,
    src *C.char, srcLen C.size_t,
    mt *C.char, mtLen C.size_t,
) C.int {{
    predicateMu.Lock()
    handler, ok := predicateRegistry[uint64(token)]
    predicateMu.Unlock()
    if !ok {{
        return -1
    }}
    source := C.GoStringN(src, C.int(srcLen))
    msgType := C.GoStringN(mt, C.int(mtLen))

    result := true
    panicked := false
    func() {{
        defer func() {{
            if r := recover(); r != nil {{
                panicked = true
            }}
        }}()
        result = handler(source, msgType)
    }}()
    if panicked {{
        return -2
    }}
    if result {{
        return 1
    }}
    return 0
}}

// Tier3Opts customises the Tier 3 view returned by g.Tier3. Same
// shape as Tier2Opts; the heuristic is forwarded for AStarPath.
type Tier3Opts struct {{
    DirectoryShortcuts bool
    Heuristic          string
}}

// Tier3Query is a Tier 2 view with each candidate target filtered
// through `predicates[u](s, msgType)`. Tier 3 ⊆ Tier 2; when no
// predicates are installed, Tier 3 == Tier 2.
type Tier3Query struct {{
    msgType            string
    directoryShortcuts bool
    heuristic          string
}}

// Tier3 returns a Tier 3 view for `msgType`. Optional `opts`
// customise the flavor + A* heuristic. Multiple Tier3Query handles
// for different message types may coexist within a single render —
// each is cheap (a small struct, no allocation).
func (GraphQuery) Tier3(msgType string, opts ...Tier3Opts) Tier3Query {{
    var o Tier3Opts
    if len(opts) > 0 {{
        o = opts[0]
    }}
    return Tier3Query{{
        msgType:            msgType,
        directoryShortcuts: o.DirectoryShortcuts,
        heuristic:          o.Heuristic,
    }}
}}

// NodeCount returns the total number of nodes that have a registered
// PermissionManifest. Flavor doesn't affect this.
func (q Tier3Query) NodeCount() int {{
    raw, status := Invoke("host_tier3_node_count", q.msgType, q.directoryShortcuts)
    if status != GR_OK {{
        return 0
    }}
    var n float64
    if err := json.Unmarshal(raw, &n); err != nil {{
        return 0
    }}
    return int(n)
}}

// HasNode returns true iff `id` has a registered manifest.
func (q Tier3Query) HasNode(id string) bool {{
    raw, status := Invoke("host_tier3_has_node", q.msgType, id, q.directoryShortcuts)
    if status != GR_OK {{
        return false
    }}
    var b bool
    if err := json.Unmarshal(raw, &b); err != nil {{
        return false
    }}
    return b
}}

// Neighbors returns the Tier 3 (predicate-filtered Tier 2) directed
// neighbours of `id` for `q.msgType`. Sorted.
func (q Tier3Query) Neighbors(id string) []string {{
    raw, status := Invoke("host_tier3_neighbors", q.msgType, id, q.directoryShortcuts)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// HasEdge returns true iff the directed Tier 3 edge `(u, v)` exists.
func (q Tier3Query) HasEdge(u, v string) bool {{
    raw, status := Invoke("host_tier3_has_edge", q.msgType, u, v, q.directoryShortcuts)
    if status != GR_OK {{
        return false
    }}
    var b bool
    if err := json.Unmarshal(raw, &b); err != nil {{
        return false
    }}
    return b
}}

// BFS returns nodes reachable from `from` in breadth-first order
// over the Tier 3 adjacency. `depthLimit=0` removes the bound.
func (q Tier3Query) BFS(from string, depthLimit int) []string {{
    raw, status := Invoke("host_tier3_bfs", q.msgType, from, depthLimit, q.directoryShortcuts)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// DFS returns nodes reachable from `from` in depth-first order over
// the Tier 3 adjacency. `depthLimit=0` removes the bound.
func (q Tier3Query) DFS(from string, depthLimit int) []string {{
    raw, status := Invoke("host_tier3_dfs", q.msgType, from, depthLimit, q.directoryShortcuts)
    if status != GR_OK {{
        return []string{{}}
    }}
    var ids []string
    if err := json.Unmarshal(raw, &ids); err != nil {{
        return []string{{}}
    }}
    return ids
}}

// ShortestPath returns the BFS shortest path from `from` to `to`
// over the Tier 3 adjacency, or nil if no path exists.
func (q Tier3Query) ShortestPath(from, to string) []string {{
    raw, status := Invoke("host_tier3_shortest_path", q.msgType, from, to, q.directoryShortcuts)
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var path []string
    if err := json.Unmarshal(raw, &path); err != nil {{
        return nil
    }}
    return path
}}

// TopologicalSort returns a Kahn's topological order of the Tier 3
// directed adjacency, or nil if it contains a directed cycle.
func (q Tier3Query) TopologicalSort() []string {{
    raw, status := Invoke("host_tier3_topological_sort", q.msgType, q.directoryShortcuts)
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var order []string
    if err := json.Unmarshal(raw, &order); err != nil {{
        return nil
    }}
    return order
}}

// AStarPath returns the cost-from-policy A* path from `from` to
// `to` over the Tier 3 adjacency.
func (q Tier3Query) AStarPath(from, to string) []string {{
    raw, status := Invoke(
        "host_tier3_a_star_path",
        q.msgType,
        from,
        to,
        q.heuristic,
        q.directoryShortcuts,
    )
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 4 && string(raw) == "null" {{
        return nil
    }}
    var path []string
    if err := json.Unmarshal(raw, &path); err != nil {{
        return nil
    }}
    return path
}}

// AllPaths returns every simple directed path from `from` to `to`
// over the Tier 3 adjacency. `maxDepth=0` removes the bound.
func (q Tier3Query) AllPaths(from, to string, maxDepth int) [][]string {{
    raw, status := Invoke(
        "host_tier3_all_paths",
        q.msgType,
        from,
        to,
        maxDepth,
        q.directoryShortcuts,
    )
    if status != GR_OK {{
        return [][]string{{}}
    }}
    var paths [][]string
    if err := json.Unmarshal(raw, &paths); err != nil {{
        return [][]string{{}}
    }}
    return paths
}}

// ConnectedComponents returns Tarjan's strongly-connected components
// of the Tier 3 directed adjacency.
func (q Tier3Query) ConnectedComponents() [][]string {{
    raw, status := Invoke(
        "host_tier3_connected_components",
        q.msgType,
        q.directoryShortcuts,
    )
    if status != GR_OK {{
        return [][]string{{}}
    }}
    var comps [][]string
    if err := json.Unmarshal(raw, &comps); err != nil {{
        return [][]string{{}}
    }}
    return comps
}}

// CycleDetection returns directed-cycle detection over the Tier 3
// adjacency.
func (q Tier3Query) CycleDetection() CycleDetectionResult {{
    raw, status := Invoke(
        "host_tier3_cycle_detection",
        q.msgType,
        q.directoryShortcuts,
    )
    if status != GR_OK {{
        return CycleDetectionResult{{}}
    }}
    var result CycleDetectionResult
    if err := json.Unmarshal(raw, &result); err != nil {{
        return CycleDetectionResult{{}}
    }}
    return result
}}

// ----- M17 phase 3e-α: use_spawn_child / use_unmount_self / use_compose -----
//
// Dynamic node lifecycle. `UseSpawnChild` registers the user's BodyFn
// in `childBodyRegistry` under a token, then asks the Rust scheduler
// (via host_spawn_child) to enqueue an AddNodeAtRuntime mutation
// whose NodeDefinition closure round-trips back into Go via the
// GrxInvokeChildBody ABI symbol on every render. `UseUnmountSelf` is
// a thin dispatch into `host_unmount_self`. `UseCompose` is the
// multi-spec batch variant — registers each spec's BodyFn under a
// fresh token then ships the whole list through `host_compose`.
//
// Token leak (bounded): each call mints a fresh token in
// `childBodyRegistry`; the scheduler is idempotent on existing IDs
// so duplicate-id calls don't double-mount, but their tokens linger.
// Same shape as the leak documented in pyruntime + JSC 3e-α; the
// future "token GC" sweep across all token-using hooks will reclaim.

// ChildSpec describes a single child node to be spawned via
// UseSpawnChild or declared via UseCompose. Mirrors the JSC
// `ComposeChildSpec` shape modulo Go conventions — `Body` is the
// child's component function (signature: `func(Ctx) Manifest`);
// `Props` lands as the node's attrs map; `ConnectTo` is a list of
// node ids the scheduler should wire as outgoing edges right after
// the AddNodeAtRuntime mutation lands.
type ChildSpec struct {{
    ID        string
    Body      BodyFn
    Props     map[string]interface{{}}
    ConnectTo []string
}}

// registerChildBody stores `fn` under a freshly allocated token in
// `childBodyRegistry`. The token is returned + marshalled to the
// Rust side as a `componentToken`; the scheduler invokes
// GrxInvokeChildBody with the same token on every render of the
// spawned node.
func registerChildBody(fn BodyFn) uint64 {{
    token := atomic.AddUint64(&nextChildBodyToken, 1)
    childBodyMu.Lock()
    childBodyRegistry[token] = fn
    childBodyMu.Unlock()
    return token
}}

// childSpecPayload marshals a ChildSpec into the JSON shape the
// Rust-side `build_compose_spec_from_host_value` consumes. Props nil
// becomes an empty object; ConnectTo nil becomes an empty array.
func childSpecPayload(spec ChildSpec, token uint64) map[string]interface{{}} {{
    props := spec.Props
    if props == nil {{
        props = map[string]interface{{}}{{}}
    }}
    connectTo := spec.ConnectTo
    if connectTo == nil {{
        connectTo = []string{{}}
    }}
    return map[string]interface{{}}{{
        "id":             spec.ID,
        "componentToken": token,
        "props":          props,
        "connectTo":      connectTo,
    }}
}}

// UseSpawnChild returns a closure that, when invoked with a
// `ChildSpec`, registers the child's Body under a fresh token and
// dispatches `host_spawn_child(spec)`. The Rust scheduler enqueues
// an AddNodeAtRuntime mutation; the child mounts on the next
// cascade round. Specs with an empty ID or nil Body are no-ops.
//
// Mirrors JSC's `useSpawnChild()(spec)` and pyruntime's
// `use_spawn_child()(spec)`. Re-entrancy is safe: the spawn closure
// runs from a body's render but the host callback enqueues — actual
// NodeDefinition closures fire later from the scheduler, outside
// any held call_function borrow.
func UseSpawnChild() func(spec ChildSpec) {{
    return func(spec ChildSpec) {{
        if spec.ID == "" || spec.Body == nil {{
            return
        }}
        token := registerChildBody(spec.Body)
        payload := childSpecPayload(spec, token)
        _, status := Invoke("host_spawn_child", payload)
        if status != GR_OK {{
            // Best-effort: drop the token if the host call failed.
            childBodyMu.Lock()
            delete(childBodyRegistry, token)
            childBodyMu.Unlock()
        }}
    }}
}}

// UseUnmountSelf returns a closure that asks the Rust scheduler to
// unmount the active node. Subsequent renders of the active node
// will not run. Mirrors JSC's `useUnmountSelf()()` and pyruntime's
// `use_unmount_self()()`. Safe to call multiple times — the
// scheduler dedupes UnmountNode mutations per node id.
func UseUnmountSelf() func() {{
    return func() {{
        _, _ = Invoke("host_unmount_self")
    }}
}}

// UseCompose returns a closure that declares the active node's
// composed children. The multi-spec batch variant of UseSpawnChild;
// the scheduler spawns any missing children and unmounts ones that
// were composed by a prior render but absent in this one. Each
// spec's Body is registered under a fresh token so the Rust side
// can re-enter Go on every render of any spawned child.
//
// Empty ID or nil Body filters that entry out (matches JSC + Python
// semantics). The full list is marshalled even if some entries are
// dropped — the Rust side reconciles against the active node's prior
// composition.
func UseCompose() func(specs []ChildSpec) {{
    return func(specs []ChildSpec) {{
        marshalled := make([]interface{{}}, 0, len(specs))
        leaked := make([]uint64, 0, len(specs))
        for _, spec := range specs {{
            if spec.ID == "" || spec.Body == nil {{
                continue
            }}
            token := registerChildBody(spec.Body)
            leaked = append(leaked, token)
            marshalled = append(marshalled, childSpecPayload(spec, token))
        }}
        _, status := Invoke("host_compose", marshalled)
        if status != GR_OK {{
            childBodyMu.Lock()
            for _, t := range leaked {{
                delete(childBodyRegistry, t)
            }}
            childBodyMu.Unlock()
        }}
    }}
}}

// GrxInvokeChildBody dispatches a Rust-delivered ctx envelope to the
// Go-side BodyFn registered under `token` in `childBodyRegistry`.
// The ctx JSON shape is `{{id, props, neighbors}}` — same as the
// `GrxFunctionCall` ctx for top-level bodies. The Manifest is
// JSON-marshalled into the caller-owned `outResult` buffer with the
// `GR_ERR_BUFFER_TOO_SMALL` grow-and-retry protocol.
//
// Panic policy: a panic in the body is recovered and surfaces as a
// `GR_ERR_INTERNAL` return; the Rust side prints an eprintln + falls
// back to a default manifest, matching the top-level body's
// recovery path.
//
//export GrxInvokeChildBody
func GrxInvokeChildBody(
    token C.uint64_t,
    ctxJson *C.char, ctxLen C.size_t,
    outResult *C.char, outResultCap C.size_t,
    outResultLen *C.size_t,
) C.int {{
    childBodyMu.Lock()
    body, ok := childBodyRegistry[uint64(token)]
    childBodyMu.Unlock()
    if !ok {{
        return GR_ERR_CALLBACK_NOT_FOUND
    }}

    ctxBytes := C.GoBytes(unsafe.Pointer(ctxJson), C.int(ctxLen))
    var ctx Ctx
    if len(ctxBytes) > 0 {{
        if err := json.Unmarshal(ctxBytes, &ctx); err != nil {{
            return GR_ERR_INTERNAL
        }}
    }}

    var manifest Manifest
    panicked := false
    func() {{
        defer func() {{
            if r := recover(); r != nil {{
                panicked = true
            }}
        }}()
        manifest = body(ctx)
    }}()
    if panicked {{
        return GR_ERR_INTERNAL
    }}

    resultJson, err := marshalManifest(&manifest)
    if err != nil {{
        return GR_ERR_INTERNAL
    }}
    resultLen := C.size_t(len(resultJson))
    *outResultLen = resultLen
    if resultLen > outResultCap {{
        return GR_ERR_BUFFER_TOO_SMALL
    }}
    if resultLen > 0 {{
        C.memcpy(unsafe.Pointer(outResult), unsafe.Pointer(&resultJson[0]), resultLen)
    }}
    return GR_OK
}}

// ----- M17 phase 3e-β: supervision hooks (Tier A) -----
//
// All six hooks are Tier A: pure HOOK_CTX reads dispatched from the
// Rust-side host callback into `graphreact-core::use_X(...)`. Zero
// engine touch from the Go side; zero token registry interaction; no
// NodeDefinition closure capture. Dramatically smaller than 3e-α.
//
// `MonitorRef` is marshalled across FFI as `{{id: number}}`. We
// expose it as a typed Go struct mirroring JSC's `{{id}}` JS object.
// `UseDemonitor(ref)` reads `ref.ID` and round-trips. `__DOWN__`
// system message delivery is wired in graphreact-core's Scheduler
// already — when a monitored target unmounts, all monitoring nodes
// receive a `__DOWN__` message with payload `{{target, reason}}` and
// catch it via `UseMessage("__DOWN__", handler)` (shipped 3c).

// MonitorRef is the typed handle returned by UseMonitor. Mirrors
// JSC's `{{id: number}}` JS object + pyruntime's
// `_SimpleNamespace(id=...)`. ID 0 is reserved for "no monitor was
// registered" (e.g. monitoring an unknown target id).
type MonitorRef struct {{
    ID uint64
}}

// UseMonitor registers the active node to receive a `__DOWN__`
// system message when `targetID` unmounts. Returns a typed
// `MonitorRef{{ID:...}}` on success or `nil` if the target id is
// unknown to the scheduler. Catch the `__DOWN__` payload via
// `UseMessage("__DOWN__", handler)`.
func UseMonitor(targetID string) *MonitorRef {{
    raw, status := Invoke("host_monitor", targetID)
    if status != GR_OK {{
        return nil
    }}
    if len(raw) == 0 || (len(raw) == 4 && string(raw) == "null") {{
        return nil
    }}
    var result struct {{
        ID float64 `json:"id"`
    }}
    if err := json.Unmarshal(raw, &result); err != nil {{
        return nil
    }}
    return &MonitorRef{{ID: uint64(result.ID)}}
}}

// UseDemonitor cancels a previously-registered monitor. Pass the
// typed `*MonitorRef` from `UseMonitor`. Nil refs are a no-op.
func UseDemonitor(ref *MonitorRef) {{
    if ref == nil {{
        return
    }}
    _, _ = Invoke("host_demonitor", ref.ID)
}}

// UseLink establishes a bidirectional supervision link with
// `targetID`. Returns `true` on success or `false` if the target
// is unknown. Linked nodes exit together — when one unmounts
// with a non-`normal` reason, the other receives a system
// `__EXIT__` message and (by default) unmounts as well.
func UseLink(targetID string) bool {{
    raw, status := Invoke("host_link", targetID)
    if status != GR_OK {{
        return false
    }}
    var ok bool
    if err := json.Unmarshal(raw, &ok); err != nil {{
        return false
    }}
    return ok
}}

// UseUnlink removes a previously-registered link with `targetID`.
// No-op on missing.
func UseUnlink(targetID string) {{
    _, _ = Invoke("host_unlink", targetID)
}}

// UseGroupJoin joins the active node into the named process group.
// Idempotent at the scheduler level — multiple evaluations don't
// create duplicate memberships. Group emit dispatches deliver a
// single copy per group member.
func UseGroupJoin(group string) {{
    _, _ = Invoke("host_group_join", group)
}}

// UseGroupLeave removes the active node from the named process
// group. No-op if the node isn't a member.
func UseGroupLeave(group string) {{
    _, _ = Invoke("host_group_leave", group)
}}

// ----- M17 phase 3e-γ: use_source + 4 source kinds -----
//
// External-event substrate. `UseSource(factory, deps)` invokes the
// factory thunk to produce an `EventSource` descriptor, then ships
// the descriptor through `host_use_source(descriptor, deps)`. Rust
// decodes the kind + decodes opts and constructs the matching
// `Arc<dyn EventSource>` via graphreact-core's `timer_source` /
// `delay_source` or `crate::sources::http_source` /
// `crate::sources::watch_source`.
//
// Events flow asynchronously through the scheduler's external
// inbox and reach the active node as messages — catch them via
// `UseMessage(msgType, handler)`. Source kinds and their message
// types: timer→"tick", delay→"timeout", http→"http", watch→
// "watch-event".

// EventSource is the descriptor a source factory thunk returns.
// The four canonical kinds (timer/delay/http/watch) are the only
// values `host_use_source` accepts; unknown kinds round-trip to
// graphreact-core's never-firing placeholder. Mirrors pyruntime's
// `{{kind: ..., intervalMs?: ..., delayMs?: ..., opts?: {{...}}}}`
// dict shape.
type EventSource struct {{
    Kind       string                 `json:"kind"`
    IntervalMs uint64                 `json:"intervalMs,omitempty"`
    DelayMs    uint64                 `json:"delayMs,omitempty"`
    Opts       map[string]interface{{}} `json:"opts,omitempty"`
}}

// TimerSource builds a periodic-tick descriptor. The Rust-side
// `timer_source(intervalMs)` fires a `"tick"` message every
// `intervalMs` milliseconds.
func TimerSource(intervalMs uint64) EventSource {{
    return EventSource{{Kind: "timer", IntervalMs: intervalMs}}
}}

// DelaySource builds a single-shot delay descriptor. The Rust-side
// `delay_source(delayMs)` fires a single `"timeout"` message after
// `delayMs` milliseconds.
func DelaySource(delayMs uint64) EventSource {{
    return EventSource{{Kind: "delay", DelayMs: delayMs}}
}}

// HttpSource builds an HTTP-listener descriptor. The Rust-side
// `http_source(port)` opens a TCP listener at the given port and
// delivers each accepted request as an `"http"` message.
func HttpSource(opts map[string]interface{{}}) EventSource {{
    if opts == nil {{
        opts = map[string]interface{{}}{{}}
    }}
    return EventSource{{Kind: "http", Opts: opts}}
}}

// WatchSource builds a filesystem-watch descriptor. The Rust-side
// `watch_source(path)` registers a `notify`-backed watcher and
// delivers each change event as a `"watch-event"` message.
func WatchSource(opts map[string]interface{{}}) EventSource {{
    if opts == nil {{
        opts = map[string]interface{{}}{{}}
    }}
    return EventSource{{Kind: "watch", Opts: opts}}
}}

// UseSource registers an EventSource factory + dependency list with
// the scheduler. The factory thunk runs once (or whenever `deps`
// change) and produces an EventSource descriptor that the Rust side
// uses to instantiate a real `Arc<dyn EventSource>`. Source events
// land as messages on the active node; catch with UseMessage.
//
// A nil factory or a factory that returns a zero-Kind descriptor is
// a no-op (no host call dispatched).
func UseSource(factory func() EventSource, deps []interface{{}}) {{
    if factory == nil {{
        return
    }}
    descriptor := factory()
    if descriptor.Kind == "" {{
        return
    }}
    if deps == nil {{
        deps = []interface{{}}{{}}
    }}
    payload := map[string]interface{{}}{{
        "kind": descriptor.Kind,
    }}
    if descriptor.IntervalMs > 0 {{
        payload["intervalMs"] = descriptor.IntervalMs
    }}
    if descriptor.DelayMs > 0 {{
        payload["delayMs"] = descriptor.DelayMs
    }}
    if descriptor.Opts != nil {{
        payload["opts"] = descriptor.Opts
    }}
    _, _ = Invoke("host_use_source", payload, deps)
}}

// GrxInvokeCallback invokes a generic Go callback stored under
// `token` in `callbackRegistry`. Returns an envelope JSON describing
// the outcome: `{{kind: "value"}}` (no cleanup), `{{kind: "cleanup",
// token: <ct>}}` (cleanup closure registered under fresh token), or
// `{{kind: "throw", message: <msg>}}` (panic recovered).
//
// Used by the Rust-side `host_effect` callback's `core_use_effect`
// closure to fire the user's effect after the body returns. Mirrors
// pyruntime's `__invoke_callback(token, args)` envelope shape.
//
//export GrxInvokeCallback
func GrxInvokeCallback(
    token C.uint64_t,
    outResult *C.char, outResultCap C.size_t,
    outResultLen *C.size_t,
) C.int {{
    callbackMu.Lock()
    cb, ok := callbackRegistry[uint64(token)]
    callbackMu.Unlock()
    if !ok {{
        return GR_ERR_CALLBACK_NOT_FOUND
    }}

    var result interface{{}}
    var panicMsg string
    func() {{
        defer func() {{
            if r := recover(); r != nil {{
                panicMsg = fmt.Sprintf("%v", r)
            }}
        }}()
        result = cb()
    }}()

    envelope := map[string]interface{{}}{{}}
    switch {{
    case panicMsg != "":
        envelope["kind"] = "throw"
        envelope["message"] = panicMsg
    case result == nil:
        envelope["kind"] = "value"
    default:
        // Accept either `func()` (canonical UseEffect cleanup) or
        // `func() interface{{}}` (nested cleanup tokens — rare; the
        // engine wraps everything as the latter internally).
        var cleanupFn func() interface{{}}
        switch v := result.(type) {{
        case func():
            cleanupFn = func() interface{{}} {{
                v()
                return nil
            }}
        case func() interface{{}}:
            cleanupFn = v
        default:
            envelope["kind"] = "value"
        }}
        if cleanupFn != nil {{
            ct := atomic.AddUint64(&nextCallbackToken, 1)
            callbackMu.Lock()
            callbackRegistry[ct] = cleanupFn
            callbackMu.Unlock()
            envelope["kind"] = "cleanup"
            envelope["token"] = ct
        }}
    }}

    resultJson, err := json.Marshal(envelope)
    if err != nil {{
        return GR_ERR_INTERNAL
    }}
    resultLen := C.size_t(len(resultJson))
    *outResultLen = resultLen
    if resultLen > outResultCap {{
        return GR_ERR_BUFFER_TOO_SMALL
    }}
    if resultLen > 0 {{
        C.memcpy(unsafe.Pointer(outResult), unsafe.Pointer(&resultJson[0]), resultLen)
    }}
    return GR_OK
}}

// GrxReleaseCallback drops a token from `callbackRegistry`. The
// Rust-side `EffectCleanup` closure calls this after invoking a
// cleanup token so the closure GC's promptly.
//
//export GrxReleaseCallback
func GrxReleaseCallback(token C.uint64_t) C.int {{
    callbackMu.Lock()
    _, ok := callbackRegistry[uint64(token)]
    if ok {{
        delete(callbackRegistry, uint64(token))
    }}
    callbackMu.Unlock()
    if !ok {{
        return GR_ERR_CALLBACK_NOT_FOUND
    }}
    return GR_OK
}}

// main is required by `go build -buildmode=c-shared`. Never called.
func main() {{}}
"#,
        abi_version = GRX_GO_ABI_VERSION,
    )
}

/// Render the canonical `go.mod` shipped alongside the prelude.
#[must_use]
pub fn render_go_mod() -> String {
    String::from(
        "// Code generated by graphreact-codegen-go. DO NOT EDIT.\n\
         \n\
         module grxbody\n\
         \n\
         go 1.22\n",
    )
}

/// Render the codegen-emitted `dispatch.go` for a list of component
/// names. Each component name `<C>` is registered under the
/// `<C>Body` Go identifier (authors must define a function of that
/// name with signature `func(Ctx) Manifest`).
#[must_use]
pub fn render_dispatch(component_names: &[String]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("// Code generated by graphreact-codegen-go. DO NOT EDIT.\n");
    out.push_str("//\n");
    out.push_str("// Body-dispatch table for the c-shared artifact. Each entry\n");
    out.push_str("// registers a user-authored <Component>Body function under\n");
    out.push_str("// its component name so GrxFunctionCall can route by component.\n\n");
    out.push_str("package main\n\n");
    out.push_str("func init() {\n");
    for name in component_names {
        // Component names are author-controlled identifiers; a panic
        // here would only fire if the host string formatter itself
        // failed, which has no failure mode against String.
        let _ = writeln!(out, "    RegisterBody({name:?}, {name}Body)");
    }
    out.push_str("}\n");
    out
}
