//! Go runtime engine for GraphReact — **M17 phase 1+2+3a+3b**.
//!
//! Per [ADR-0005](../../../docs/decisions/0005-third-runtime-is-go-not-jvm.md)
//! and the [later-milestone strategic plan](../../../../../.claude/plans/graphreact-rust-later-milestones.md)
//! §M17, the Go runtime ships as the third in-process body-language host
//! after JSC (M12.3) and CPython (M13). The embedding strategy is
//! **cgo + c-shared**: each `.grx` file's `lang="go"` bodies are compiled
//! by [`graphreact-codegen-go`](../../graphreact-codegen-go/) via
//! `go build -buildmode=c-shared` into a `.so` / `.dylib` artifact that
//! the daemon loads through [`libloading`] and dispatches against the
//! [`graphreact_engine_zig::Engine`] trait.
//!
//! # Architecture
//!
//! - **Single Go runtime per process** (ADR-0005 locked decision #2).
//!   Once a [`LoadedRuntime`] is initialised the Go runtime is alive
//!   for the lifetime of the process — cgo's c-shared runtime is not
//!   re-initialisable after teardown. [`GoEngine::shutdown`] is a soft
//!   no-op on the loaded artifact (releases per-isolate state on the
//!   Rust side but does NOT dlclose the library).
//!
//! - **Per-isolate `sync.Mutex` serialisation on the Go side** (matches
//!   the trait's `&mut self` borrow shape). The Rust side keeps a
//!   minimal `state.iso_callbacks` map for cleanup on dispose; isolate
//!   lifecycle is fully owned by the Go runtime via `GrxIsolateCreate`
//!   / `GrxIsolateDispose`.
//!
//! - **JSON marshalling at the FFI boundary**, identical to JSC and
//!   CPython. Per ADR-0005 the wire format is JSON for phase 1+2; a
//!   future MessagePack / BSATN cutover (if hot-path numeric arrays
//!   become a constraint) is a separable refactor across all three
//!   engines.
//!
//! - **Callback trampoline is engine-agnostic.** The
//!   [`host_callback_trampoline`] body is copied verbatim from
//!   [`graphreact_engine_cpython`] — args/result cross the FFI as
//!   JSON, the callback id rides in `cb_data` as an integer-encoded
//!   pointer, and a process-global [`DashMap`] holds the closures.
//!
//! # Status
//!
//! - **Phase 1+2 (engine binding lifecycle):** SHIPPED. Real
//!   libloading-based initialize/shutdown/create_isolate/
//!   dispose_isolate/eval_module/resolve_function/call_function.
//! - **Phase 2.5 (bidirectional callbacks):** SHIPPED. The
//!   trampoline + register_callback path mirrors CPython phase 2.5
//!   exactly.
//! - **Phase 3a (parser surface + goruntime.rs sibling):** SHIPPED
//!   parser-side; goruntime.rs skeleton in
//!   `crates/graphreact-cli/src/`.
//! - **Phase 3b (build-step codegen):** SHIPPED v1 via
//!   [`graphreact-codegen-go`].
//! - **Phase 3c–h (full hook surface + supervision + async):** NOT
//!   YET — landing in subsequent M17 sessions per plan §M17.
//!
//! # Construction
//!
//! For phase 1+2 the engine is constructed against a specific
//! c-shared artifact path:
//!
//! ```ignore
//! use std::path::Path;
//! let mut engine = graphreact_engine_go::GoEngine::with_artifact(
//!     Path::new("/path/to/libgrx_go.so"),
//! );
//! engine.initialize().unwrap();
//! ```
//!
//! Real consumers (M17 phase 3a `goruntime.rs` in `graphreact-cli`)
//! drive the codegen + load pipeline end-to-end.
//!
//! [`libloading`]: https://crates.io/crates/libloading
//! [`graphreact-codegen-go`]: ../../graphreact-codegen-go/
//! [`graphreact_engine_cpython`]: ../../graphreact-engine-cpython/

#![allow(unsafe_code)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr
)]
// The cb_data trampoline encoding is `u64 → *mut c_void → u64`.
// On 32-bit targets `usize` is u32, which truncates ids > u32::MAX.
// The wire format and the M17 phase 1+2 acceptance gate assume
// 64-bit `usize`; document and enforce explicitly rather than ship
// a silent-truncation footgun on i686 / armv7.
#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "graphreact-engine-go requires a 64-bit target (cb_data trampoline encoding \
     is pointer-width; M17 phase 1+2 wire format is u64-locked). \
     Track 32-bit support in plan §M17 phase 3g."
);

mod ffi;
mod loader;

use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use graphreact_engine_zig::{
    BoxCallback, CallbackId, Engine, EngineError, EngineResult, FunctionHandle, HostValue,
    IsolateHandle, IsolateOptions, ModuleHandle,
};

pub use loader::LoadedRuntime;

const RESULT_BUFFER_INITIAL: usize = 4096;
const RESULT_BUFFER_MAX: usize = 1 << 20; // 1 MiB

/// Boxed-callback type stored in the registry. Held as `Arc` (not the
/// `Box` of [`BoxCallback`]) because the Go trampoline may invoke the
/// closure from a goroutine that races with register/unregister;
/// cloning the `Arc` out under the registry guard lets the
/// trampoline drop the shard lock before invoking. Mirrors
/// `graphreact_engine_cpython::SharedCallback`.
type SharedCallback = Arc<dyn Fn(&[HostValue]) -> EngineResult<HostValue> + Send + Sync>;

/// Process-global registry mapping callback id → closure. The Go side
/// receives the id back as a `cb_data` pointer-encoded integer and
/// the [`host_callback_trampoline`] dispatches into the closure
/// stored here. Same shape as `graphreact-engine-jsc::CALLBACK_REGISTRY`
/// and `graphreact-engine-cpython::CALLBACK_REGISTRY` — the
/// trampoline is engine-agnostic since args/result cross the FFI
/// boundary as JSON.
static CALLBACK_REGISTRY: OnceLock<DashMap<u64, SharedCallback>> = OnceLock::new();

fn callback_registry() -> &'static DashMap<u64, SharedCallback> {
    CALLBACK_REGISTRY.get_or_init(DashMap::new)
}

/// Test-only accessor: returns true iff the given callback id is
/// currently registered in the process-global registry. Used by the
/// engine-conformance suite to assert `dispose_isolate` / `shutdown`
/// actually drain the per-isolate closures.
#[doc(hidden)]
#[must_use]
pub fn __test_callback_registry_contains(id: u64) -> bool {
    callback_registry().contains_key(&id)
}

/// Go runtime engine. Wraps the cgo + c-shared bridge with a safe
/// Rust facade. Holds the [`LoadedRuntime`] in an `OnceLock<Arc<...>>`
/// so the Go runtime is loaded lazily on `initialize()` and lives
/// until the process exits.
pub struct GoEngine {
    /// Path to the c-shared artifact this engine will load on
    /// `initialize`. Set at construction via [`Self::with_artifact`];
    /// `Self::new` constructs an engine that defers artifact
    /// selection until [`Self::set_artifact`] is called.
    artifact: Mutex<Option<PathBuf>>,

    /// The resolved runtime. Loaded on the first `initialize()` and
    /// then reused — see the module-level doc on Go runtime
    /// singleton lifetime.
    runtime: OnceLock<Arc<LoadedRuntime>>,

    /// Serialises concurrent `initialize()` callers around the
    /// dlopen + `OnceLock::set` sequence so only one Arc<LoadedRuntime>
    /// is ever built. Cheap and uncontended in the common case.
    load_lock: Mutex<()>,

    initialized: AtomicBool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Per-isolate set of callback ids this engine instance owns.
    /// On `dispose_isolate` / `shutdown` we drain matching ids out
    /// of [`CALLBACK_REGISTRY`] so closures don't outlive the Go
    /// side. Mirrors `CPythonEngine::State::iso_callbacks` and
    /// `JscEngine::State::iso_callbacks`.
    iso_callbacks: FxHashMap<u64, FxHashSet<u64>>,
}

impl Default for GoEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl GoEngine {
    /// Construct an engine that will pick its c-shared artifact at
    /// the first [`Engine::initialize`] call. Useful when the artifact
    /// is supplied later — e.g. when `goruntime.rs` runs
    /// `Codegen::build_artifact` and then hands the path to the
    /// engine via [`Self::set_artifact`].
    ///
    /// `initialize()` against an engine constructed this way without
    /// a subsequent `set_artifact` call returns
    /// [`EngineError::Internal`] flagging the missing artifact.
    #[must_use]
    pub fn new() -> Self {
        Self {
            artifact: Mutex::new(None),
            runtime: OnceLock::new(),
            load_lock: Mutex::new(()),
            initialized: AtomicBool::new(false),
            state: Mutex::new(State::default()),
        }
    }

    /// Construct an engine pre-pointed at `artifact`. Equivalent to
    /// `Self::new()` followed by `set_artifact`.
    #[must_use]
    pub fn with_artifact<P: Into<PathBuf>>(artifact: P) -> Self {
        let engine = Self::new();
        *engine.artifact.lock() = Some(artifact.into());
        engine
    }

    /// Set or replace the artifact path. Returns an error if the
    /// runtime has already been loaded — Go's c-shared runtime is
    /// not swappable mid-process, and silently ignoring the
    /// reassignment would mask the operator's bug.
    pub fn set_artifact<P: Into<PathBuf>>(&self, artifact: P) -> EngineResult<()> {
        if self.runtime.get().is_some() {
            return Err(EngineError::Internal(
                "graphreact-engine-go: cannot set_artifact after initialize — \
                 the Go runtime is a process singleton. Construct a fresh \
                 GoEngine per artifact (M17 phase 3g hot-reload landing) or \
                 keep using the originally-loaded artifact."
                    .into(),
            ));
        }
        *self.artifact.lock() = Some(artifact.into());
        Ok(())
    }

    /// Live isolate count according to the Go side. Returns 0 if the
    /// optional `GrxEngineLiveIsolates` symbol was not present in
    /// the loaded artifact (e.g. older codegen pass).
    #[must_use]
    pub fn live_isolates(&self) -> u64 {
        let Some(runtime) = self.runtime.get() else {
            return 0;
        };
        if let Some(live) = runtime.engine_live_isolates {
            unsafe { live() }
        } else {
            0
        }
    }

    /// Return a clone of the loaded runtime handle. `None` if
    /// `initialize` has not yet succeeded.
    fn runtime(&self) -> EngineResult<Arc<LoadedRuntime>> {
        self.runtime
            .get()
            .cloned()
            .ok_or_else(|| EngineError::Internal("graphreact-engine-go: not initialised".into()))
    }

    /// **M17 phase 3c.** Dispatch a marshalled message envelope to
    /// the Go-side `UseMessage` handler registered under `token`. The
    /// `goruntime::build_host_message_callback` closure invokes this
    /// on every scheduler delivery; the closure captures the
    /// engine + isolate via TLS-installed [`Rc`] and the token
    /// returned at registration time.
    ///
    /// `msg` is the `{from, type, payload, ttl}` envelope built by
    /// the host_message callback; it crosses the FFI boundary as a
    /// single-element JSON array `[envelope]` mirroring the M13
    /// phase 3c shape so the Go side can deserialise into
    /// `map[string]interface{}` cleanly.
    ///
    /// # Errors
    ///
    /// - [`EngineError::Internal`] if the loaded artifact does not
    ///   export `GrxInvokeHandler` (older phase 3b-real codegen).
    /// - [`EngineError::CallbackNotFound`] if `token` was never
    ///   registered (or was already dropped).
    /// - JSON serialisation errors surface as
    ///   [`EngineError::Internal`].
    pub fn invoke_handler(&self, token: u64, msg: &HostValue) -> EngineResult<()> {
        let runtime = self.runtime()?;
        let invoke = runtime.invoke_handler.ok_or_else(|| {
            EngineError::Internal(
                "graphreact-engine-go: artifact missing GrxInvokeHandler — rebuild via codegen \
                 with M17 phase 3c+ (use_emit / use_message)"
                    .into(),
            )
        })?;
        let envelope = serde_json::Value::Array(vec![msg.clone().into_json()]);
        let envelope_json = serde_json::to_string(&envelope)
            .map_err(|e| EngineError::Internal(format!("invoke_handler envelope: {e}")))?;
        let bytes = envelope_json.as_bytes();
        // SAFETY: `bytes` is a borrowed slice from `envelope_json`,
        // which lives for the duration of this call. The callee reads
        // up to `bytes.len()` from `bytes.as_ptr()` and immediately
        // dispatches synchronously to the registered handler before
        // returning. No pointers escape the Go side.
        let rc = unsafe { invoke(token, bytes.as_ptr().cast::<c_char>(), bytes.len()) };
        if rc == ffi::GR_OK {
            Ok(())
        } else if rc == ffi::GR_ERR_CALLBACK_NOT_FOUND {
            Err(EngineError::CallbackNotFound {
                name: format!("handler-token:{token}"),
            })
        } else {
            Err(map_status(rc))
        }
    }
}

/// Map a Go-bridge status code to a typed `EngineError`. Mirrors
/// `graphreact-engine-cpython::map_status` and
/// `graphreact-engine-jsc::map_status` — values are intentionally
/// identical so the same mapping function shape works.
fn map_status(rc: i32) -> EngineError {
    match rc {
        ffi::GR_OK => EngineError::Internal("expected error, got GR_OK".into()),
        ffi::GR_ERR_NOT_INITIALIZED => EngineError::Internal("engine not initialised".into()),
        ffi::GR_ERR_INVALID_ISOLATE => EngineError::InvalidIsolate,
        ffi::GR_ERR_INVALID_MODULE => EngineError::InvalidModule,
        ffi::GR_ERR_INVALID_FUNCTION => EngineError::Internal("invalid function handle".into()),
        ffi::GR_ERR_CALLBACK_DUPLICATE => EngineError::CallbackAlreadyRegistered {
            name: "<unknown>".into(),
        },
        ffi::GR_ERR_CALLBACK_NOT_FOUND => EngineError::CallbackNotFound {
            name: "<unknown>".into(),
        },
        ffi::GR_ERR_BUFFER_TOO_SMALL => EngineError::Internal("buffer too small".into()),
        ffi::GR_ERR_SCRIPT => EngineError::Internal("go script error".into()),
        ffi::GR_ERR_INTERNAL => EngineError::Internal("go bridge internal error".into()),
        other => EngineError::Internal(format!("unknown go bridge status code: {other}")),
    }
}

/// Run a closure that produces a JSON-shaped result into a
/// caller-owned buffer, growing on `GR_ERR_BUFFER_TOO_SMALL` up to
/// [`RESULT_BUFFER_MAX`]. Mirrors the identical helpers in JSC and
/// CPython.
fn call_with_buffer<F>(mut call: F) -> EngineResult<String>
where
    F: FnMut(*mut c_char, usize, *mut usize) -> i32,
{
    let mut cap = RESULT_BUFFER_INITIAL;
    loop {
        let mut buf = vec![0_u8; cap];
        let mut written: usize = cap;
        let rc = call(buf.as_mut_ptr().cast(), cap, &mut written);
        if rc == ffi::GR_OK {
            buf.truncate(written);
            return String::from_utf8(buf)
                .map_err(|_| EngineError::Internal("non-utf8 result from go bridge".into()));
        }
        if rc == ffi::GR_ERR_BUFFER_TOO_SMALL {
            // Bug-fix vs the CPython precedent: check the required
            // size BEFORE clamping to RESULT_BUFFER_MAX, otherwise
            // the cap-exceeded branch is unreachable and the loop
            // spins forever at cap=1 MiB on payloads >1 MiB.
            let required = written.max(cap * 2);
            if required > RESULT_BUFFER_MAX {
                return Err(EngineError::Internal(format!(
                    "go bridge result {required} bytes exceeded 1 MiB cap"
                )));
            }
            cap = required;
            continue;
        }
        return Err(map_status(rc));
    }
}

/// Serialise an args slice as a JSON array suitable for the bridge.
/// Symmetrical with the JSC + CPython equivalents.
fn host_args_to_json(args: &[HostValue]) -> EngineResult<String> {
    let array = serde_json::Value::Array(args.iter().cloned().map(HostValue::into_json).collect());
    serde_json::to_string(&array)
        .map_err(|e| EngineError::Internal(format!("args json serialise: {e}")))
}

fn json_to_host_value(value: serde_json::Value) -> HostValue {
    HostValue::from_json(value)
}

/// Phase 2.5 trampoline. The Go-side prelude calls this directly
/// (registered via `GrxCallbackRegister` with the trampoline as
/// `cb_fn` and the callback id pointer-encoded as `cb_data`).
///
/// Body is **engine-agnostic** — copied verbatim from
/// `graphreact-engine-cpython::host_callback_trampoline` since
/// args/result cross the FFI as JSON and the registry lookup is
/// shared.
///
/// Returns `GR_OK` on success, `GR_ERR_BUFFER_TOO_SMALL` (with the
/// required size in `*out_result_len`) when the result doesn't fit,
/// `GR_ERR_CALLBACK_NOT_FOUND` if the id isn't in the registry, or
/// `GR_ERR_INTERNAL` on any other failure.
unsafe extern "C" fn host_callback_trampoline(
    cb_data: *mut c_void,
    args_json: *const c_char,
    args_len: usize,
    out_result: *mut c_char,
    out_result_cap: usize,
    out_result_len: *mut usize,
) -> c_int {
    // SAFETY: `cb_data` is the id encoded as a pointer on the Go
    // side. Reading it back is a pure integer cast.
    let id = cb_data as usize as u64;

    let cb = callback_registry().get(&id).map(|entry| entry.clone());
    let Some(cb) = cb else {
        return ffi::GR_ERR_CALLBACK_NOT_FOUND;
    };

    let args_slice: &[u8] = if args_json.is_null() || args_len == 0 {
        b"[]"
    } else {
        unsafe { std::slice::from_raw_parts(args_json.cast::<u8>(), args_len) }
    };
    let args_str = match std::str::from_utf8(args_slice) {
        Ok(s) => s,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };
    let parsed: serde_json::Value = match serde_json::from_str(args_str) {
        Ok(v) => v,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };
    let args: Vec<HostValue> = match parsed {
        serde_json::Value::Array(arr) => arr.into_iter().map(HostValue::from_json).collect(),
        _ => return ffi::GR_ERR_INTERNAL,
    };

    let result = match cb(&args) {
        Ok(v) => v,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };

    let result_json = match serde_json::to_string(&result.into_json()) {
        Ok(s) => s,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };

    let bytes = result_json.as_bytes();
    if !out_result_len.is_null() {
        unsafe { *out_result_len = bytes.len() };
    }
    if bytes.len() > out_result_cap {
        return ffi::GR_ERR_BUFFER_TOO_SMALL;
    }
    if !out_result.is_null() && !bytes.is_empty() {
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_result.cast::<u8>(), bytes.len());
        }
    }
    ffi::GR_OK
}

impl Engine for GoEngine {
    fn initialize(&mut self) -> EngineResult<()> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        // Load the c-shared artifact the FIRST time initialize is
        // called. Subsequent inits are no-ops because the Go runtime
        // is not re-initialisable.
        //
        // CRITICAL: serialise the load via `self.load_lock` so dlopen
        // runs inside the OnceLock arbitration. The naive shape
        // (load then OnceLock::set) builds two Arcs on contended
        // init; only one wins set(), and the loser's Arc drops its
        // Library — which is a dlclose. On POSIX dlopen-coalescing
        // platforms that can decrement the WINNER's refcount to
        // zero, unloading the live Go runtime. The mutex + post-
        // acquire `runtime.get()` re-check guarantees exactly one
        // dlopen per process. (`OnceLock::get_or_try_init` is the
        // ergonomic stdlib answer but it's still unstable — issue
        // #109737 — so we hand-roll the double-checked-locking
        // pattern for now.)
        let runtime = if let Some(rt) = self.runtime.get() {
            rt.clone()
        } else {
            let _guard = self.load_lock.lock();
            if let Some(rt) = self.runtime.get() {
                rt.clone()
            } else {
                let artifact = self.artifact.lock().clone().ok_or_else(|| {
                    EngineError::Internal(
                        "graphreact-engine-go: no artifact path configured. \
                         Call `with_artifact` or `set_artifact` before `initialize`."
                            .into(),
                    )
                })?;
                let loaded = LoadedRuntime::load(&artifact)?;
                let arc = Arc::new(loaded);
                // Should always succeed because we hold the
                // load_lock; if some other path raced past it (it
                // can't, but be defensive), reuse the winner's.
                let _ = self.runtime.set(arc.clone());
                self.runtime.get().cloned().unwrap_or(arc)
            }
        };

        let rc = unsafe { (runtime.engine_initialize)() };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn shutdown(&mut self) -> EngineResult<()> {
        if !self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        let runtime = self.runtime()?;
        let rc = unsafe { (runtime.engine_shutdown)() };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        self.initialized.store(false, Ordering::Release);

        // Drop every closure this engine owned. The Go side ended
        // each isolate inside `GrxEngineShutdown`; we drop the
        // Rust-side Arc refs so closures are released.
        let mut state = self.state.lock();
        let registry = callback_registry();
        for ids in state.iso_callbacks.values() {
            for id in ids {
                registry.remove(id);
            }
        }
        state.iso_callbacks.clear();

        // NOTE: we do NOT drop `self.runtime` here. The Go runtime
        // is a process-level singleton; `dlclose` after `Py_Finalize`-
        // style finalisation triggers cgo runtime bugs. Subsequent
        // initialize() calls reuse the same handle.
        Ok(())
    }

    fn create_isolate(&mut self, _opts: IsolateOptions) -> EngineResult<IsolateHandle> {
        self.initialize()?;
        let runtime = self.runtime()?;
        let mut handle: u64 = 0;
        let rc = unsafe { (runtime.isolate_create)(&mut handle) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(IsolateHandle(handle))
    }

    fn dispose_isolate(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        let runtime = self.runtime()?;
        let rc = unsafe { (runtime.isolate_dispose)(iso.0) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }

        // Drop the Rust-side closures that belonged to this isolate.
        if let Some(ids) = self.state.lock().iso_callbacks.remove(&iso.0) {
            let registry = callback_registry();
            for id in ids {
                registry.remove(&id);
            }
        }
        Ok(())
    }

    /// Register `source` as a module under the isolate. The Go side
    /// stashes the source by name; bodies are pre-compiled into the
    /// c-shared artifact, so the runtime treats `source` as a
    /// **module key** rather than a source string (there is no Go
    /// runtime eval — see ADR-0005 locked decision #3). Per-isolate
    /// module registry on the Go side lets multiple components
    /// coexist.
    fn eval_module(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        source: &str,
    ) -> EngineResult<ModuleHandle> {
        let runtime = self.runtime()?;
        let mut module: u64 = 0;
        let rc = unsafe {
            (runtime.module_register)(
                iso.0,
                name.as_ptr().cast(),
                name.len(),
                source.as_ptr().cast(),
                source.len(),
                &mut module,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(ModuleHandle(module))
    }

    /// Resolve `name` as a function exported by the registered Go
    /// module. The Go side looks the symbol up in its per-isolate
    /// function table; the resulting handle is opaque on the Rust
    /// side.
    fn resolve_function(
        &mut self,
        iso: IsolateHandle,
        module: ModuleHandle,
        name: &str,
    ) -> EngineResult<FunctionHandle> {
        let runtime = self.runtime()?;
        let mut function: u64 = 0;
        let rc = unsafe {
            (runtime.function_resolve)(
                iso.0,
                module.0,
                name.as_ptr().cast(),
                name.len(),
                &mut function,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(FunctionHandle(function))
    }

    fn register_callback(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        cb: BoxCallback,
    ) -> EngineResult<CallbackId> {
        let runtime = self.runtime()?;
        let mut id: u64 = 0;
        let rc = unsafe {
            (runtime.callback_register)(
                iso.0,
                name.as_ptr().cast(),
                name.len(),
                host_callback_trampoline,
                std::ptr::null_mut(),
                &mut id,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }

        // Store the Box → Arc in the registry. The trampoline reads
        // this on every Go-side host_callback invocation.
        let shared: SharedCallback = Arc::from(cb);
        callback_registry().insert(id, shared);
        self.state
            .lock()
            .iso_callbacks
            .entry(iso.0)
            .or_default()
            .insert(id);
        Ok(CallbackId(id))
    }

    fn call_function(
        &mut self,
        iso: IsolateHandle,
        function: FunctionHandle,
        args: &[HostValue],
    ) -> EngineResult<HostValue> {
        let runtime = self.runtime()?;
        let args_json = host_args_to_json(args)?;
        let result_text = call_with_buffer(|buf, cap, written| unsafe {
            (runtime.function_call)(
                iso.0,
                function.0,
                args_json.as_ptr().cast(),
                args_json.len(),
                buf,
                cap,
                written,
            )
        })?;
        let value: serde_json::Value = serde_json::from_str(&result_text)
            .map_err(|e| EngineError::Internal(format!("result json parse: {e}")))?;
        Ok(json_to_host_value(value))
    }

    /// Phase 1+2 is a soft no-op on the Go side. Real microtask
    /// drain semantics for body-spawned goroutines arrive at M17
    /// phase 3g (context.Context cancellation + `sync.WaitGroup`
    /// drain). Returns whatever the Go side reports — typically
    /// `GR_OK`.
    fn drain_microtasks(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        let runtime = self.runtime()?;
        let rc = unsafe { (runtime.microtasks_drain)(iso.0) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(())
    }

    fn engine_name(&self) -> &'static str {
        "go"
    }

    fn engine_version(&self) -> String {
        format!("go-{}-m17-phase1", env!("CARGO_PKG_VERSION"))
    }
}

#[cfg(test)]
mod unit_tests {
    //! Pure unit tests that don't touch the FFI. The Engine-
    //! conformance suite (12 scenarios that load a real Go-built
    //! libgrx_go_test artifact + exercise the full lifecycle +
    //! callback round-trip + buffer-grow paths) lives in
    //! [`tests/conformance_smoke.rs`](../tests/conformance_smoke.rs)
    //! as an integration test — one process per test binary lets
    //! `std::process::exit(0)` dodge the Go c-shared TSD-cleanup
    //! deadlock that fires on macOS when `pthread_exit` runs a
    //! Go-registered destructor (golang/go#47100). Inline `#[test]`
    //! functions can't use that workaround because cargo-test
    //! joins every test thread.

    use super::*;

    #[test]
    fn engine_name_is_go() {
        let e = GoEngine::new();
        assert_eq!(e.engine_name(), "go");
    }

    #[test]
    fn engine_version_cites_m17() {
        let e = GoEngine::new();
        assert!(e.engine_version().contains("m17"));
    }

    #[test]
    fn initialize_without_artifact_returns_clear_error() {
        let mut e = GoEngine::new();
        let result = e.initialize();
        let Err(EngineError::Internal(msg)) = result else {
            panic!("expected EngineError::Internal");
        };
        assert!(
            msg.contains("artifact"),
            "error should cite missing artifact: {msg}"
        );
    }

    #[test]
    fn map_status_round_trip() {
        assert!(matches!(
            map_status(ffi::GR_ERR_INVALID_ISOLATE),
            EngineError::InvalidIsolate
        ));
        assert!(matches!(
            map_status(ffi::GR_ERR_INVALID_MODULE),
            EngineError::InvalidModule
        ));
        // GR_OK as input is itself an error (caller bug).
        assert!(matches!(map_status(ffi::GR_OK), EngineError::Internal(_)));
    }

    #[test]
    fn host_args_to_json_round_trips_primitives() {
        let args = vec![
            HostValue::Null,
            HostValue::Bool(true),
            HostValue::Number(42.0),
            HostValue::String("hi".into()),
        ];
        let json = host_args_to_json(&args).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let serde_json::Value::Array(arr) = parsed else {
            panic!("expected array");
        };
        assert_eq!(arr.len(), 4);
    }
}

