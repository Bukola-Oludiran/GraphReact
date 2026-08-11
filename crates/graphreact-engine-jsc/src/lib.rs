//! JSC (JavaScriptCore) engine bridge — M12.3a slice.
//!
//! Wraps `zig/bridge/jsc.zig`'s C ABI surface in a safe Rust
//! [`Engine`] implementation. Phase progression:
//! - **phase1** — initialize/shutdown/isolate lifecycle real; eval,
//!   resolve, call, callbacks stubbed.
//! - **phase2** — real `JSEvaluateScript` + `JSObjectGetProperty` +
//!   `JSObjectCallAsFunction` with JSON marshalling and hook prelude
//!   (no-op stubs).
//! - **phase2.5 (this slice)** — real bidirectional callbacks. JS
//!   code calling a registered host name routes through a JSC C
//!   trampoline, into a Rust trampoline below, into the closure
//!   stored in the global [`CALLBACK_REGISTRY`]. Symmetric path
//!   `gr_jsc_callback_invoke` lets Rust call back into JS by
//!   global-name.
//! - **phase3** — wire real hook semantics through these callbacks.
//!
//! On macOS, links against the system `JavaScriptCore.framework`
//! (no vendoring needed). Linux/Windows via vendored WebKit is the
//! M12.3b deliverable.

// FFI crate: every call into the JSC bridge crosses an `extern "C"`
// boundary. The bridge surface itself is the source of unsafety; the
// rest of the file is a safe facade over those calls.
#![allow(unsafe_code)]
#![allow(
    clippy::module_name_repetitions,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr,
    clippy::iter_cloned_collect
)]

mod ffi;

use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::{Mutex, RwLock};
use rustc_hash::{FxHashMap, FxHashSet};

use graphreact_engine_zig::{
    BoxCallback, CallbackId, Engine, EngineError, EngineResult, FunctionHandle, HostSecretCallback,
    HostValue, IsolateHandle, IsolateOptions, ModuleHandle,
};

const RESULT_BUFFER_INITIAL: usize = 4096;
const RESULT_BUFFER_MAX: usize = 1 << 20; // 1 MiB

/// Boxed-callback type stored in the registry. We hold an `Arc` (not
/// the `Box` of the [`BoxCallback`] alias) because the JSC trampoline
/// may invoke the closure from a context that races with
/// register/unregister; cloning an `Arc` out under the lock keeps the
/// closure alive past the lock release while not blocking other
/// registrations.
type SharedCallback = Arc<dyn Fn(&[HostValue]) -> EngineResult<HostValue> + Send + Sync>;

/// Process-global registry mapping callback id → closure. The Zig
/// layer assigns ids monotonically across all isolates; the Rust
/// trampoline reads `cb_data` as the id, looks up the closure here,
/// invokes it, returns the JSON-serialised result.
static CALLBACK_REGISTRY: OnceLock<RwLock<FxHashMap<u64, SharedCallback>>> = OnceLock::new();

fn callback_registry() -> &'static RwLock<FxHashMap<u64, SharedCallback>> {
    CALLBACK_REGISTRY.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// **M-Prod sub-track #3 Phase B Part 1.** Process-global shadow that
/// backs [`JscEngine::host_secret_callback`]. The trait method
/// returns `Option<&dyn HostSecretCallback>` (a `&self`-tied borrow)
/// but the underlying `Arc<dyn HostSecretCallback>` lives behind a
/// `RwLock` on the engine instance for swap semantics. To satisfy
/// the trait shape without unsafe lifetime gymnastics, the engine
/// also publishes the currently-installed callback into this
/// `OnceLock`; the trait method's returned reference points at the
/// `Arc` inside the lock cell (which is `'static`).
///
/// Production model: one `SecretStore` per process, set once at
/// startup before isolates spin up — consistent with Phase A's
/// `EnvStore` / `KeyringStore` semantics. Phase B Part 2's SIGHUP
/// rotation will widen this to `Arc<arc_swap::ArcSwap<Arc<dyn
/// HostSecretCallback>>>` for atomic-swap semantics; the trait shape
/// stays bit-for-bit compatible (the borrow handed out still resolves
/// to the currently-swapped Arc).
///
/// Tests that need to bypass the shadow register a `host_secret`
/// callback directly via [`Engine::register_callback`] — that wiring
/// is independent of this `OnceLock` and observes per-isolate
/// closures instead of the process-global Arc.
static HOST_SECRET_CALLBACK_SHADOW: OnceLock<Arc<dyn HostSecretCallback>> = OnceLock::new();

fn host_secret_callback_shadow() -> &'static OnceLock<Arc<dyn HostSecretCallback>> {
    &HOST_SECRET_CALLBACK_SHADOW
}

/// JSC engine. Wraps the Zig bridge with a safe Rust facade. The Zig
/// side keeps process-global JSC state (the singleton
/// `JSContextGroup`); the Rust side keeps callback closures in
/// [`CALLBACK_REGISTRY`] so the JSC C trampoline can re-enter Rust.
pub struct JscEngine {
    initialized: AtomicBool,
    state: Mutex<State>,
    /// **M16-Backend P9.** Idempotent guard for [`Self::enable_debugger`].
    /// `JSRemoteInspectorStart` is itself singleton-init at the framework
    /// level, but the guard avoids the FFI round-trip on repeat
    /// invocations (e.g., when the watch loop remounts after a reload).
    inspector_installed: AtomicBool,
}

#[derive(Default)]
struct State {
    /// Per-isolate set of callback ids this engine instance owns.
    /// On `dispose_isolate` / `shutdown` we drain the matching ids
    /// out of [`CALLBACK_REGISTRY`] so closures don't outlive the
    /// JS side.
    iso_callbacks: FxHashMap<u64, FxHashSet<u64>>,
}

impl Default for JscEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl JscEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            inspector_installed: AtomicBool::new(false),
        }
    }

    /// Live isolate count according to the native side. Used in stress
    /// tests to assert no leaks.
    #[must_use]
    pub fn live_isolates(&self) -> u64 {
        // SAFETY: pure value read; no pointer dereferences.
        unsafe { ffi::gr_jsc_engine_live_isolates() }
    }

    /// **M16-Backend P9.** Mark `iso`'s JSGlobalContext as remotely
    /// inspectable + start the system RemoteInspector listener so the
    /// JSContext appears in `Safari → Develop → [Hostname] → JSContext`
    /// for breakpoint attach. macOS-only via lockdownd discovery; CDP
    /// over TCP (VS Code's JS debugger / Chrome DevTools / WebStorm)
    /// requires the vendored-WebKit `RemoteInspectorServer` + CDP shim
    /// landing in M-Prod alongside V8's eventual return (V8 is
    /// soft-demoted per [decision 0001]; CDP-shaped clients defer with
    /// V8). For the symmetric Axis B framing see
    /// [feedback_axis_b_no_browser.md] (Python → debugpy + VS Code/PyCharm,
    /// JSC → RemoteInspector + Safari, JVM → JDWP + IntelliJ).
    ///
    /// Idempotent: subsequent calls short-circuit via [`Self::inspector_installed`]
    /// before crossing the FFI boundary.
    ///
    /// `port` is plumbed through for forward-compat with M-Prod's
    /// vendored TCP-bound listener and for symmetry with
    /// `--debug=cpython:<port>` (which IS port-bound via debugpy).
    /// Today the JSC bridge ignores it on macOS lockdownd discovery.
    pub fn enable_debugger(&mut self, iso: IsolateHandle, port: u16) -> EngineResult<()> {
        if self.inspector_installed.load(Ordering::Acquire) {
            return Ok(());
        }
        // SAFETY: `iso.0` is a `u64` identifier validated by the Zig
        // side (`isolateById`); no pointer dereference at the Rust
        // boundary. JSC's RemoteInspector singleton-init is thread-
        // safe per Apple's framework docs.
        let rc = unsafe { ffi::gr_jsc_isolate_enable_debugger(iso.0, port) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        self.inspector_installed.store(true, Ordering::Release);
        Ok(())
    }

    /// **M16-Backend P9.** Verification round-trip for the headless
    /// smoke test — reads back `JSGlobalContextGetRemoteInspectionEnabled`
    /// for `iso`. Returns `Ok(true)` after a successful
    /// [`Self::enable_debugger`] call, `Ok(false)` before it. Test-only
    /// in spirit but not test-gated so external integration tests can
    /// pin the surface.
    pub fn is_inspector_enabled(&self, iso: IsolateHandle) -> EngineResult<bool> {
        // SAFETY: pure read of a process-global flag; identical
        // soundness argument to `enable_debugger`.
        let rc = unsafe { ffi::gr_jsc_isolate_inspection_enabled(iso.0) };
        match rc {
            0 => Ok(false),
            1 => Ok(true),
            err => Err(map_status(err)),
        }
    }

    /// **M-Prod sub-track #3 Phase B Part 1.** Install the
    /// process-global [`HostSecretCallback`] that
    /// [`Engine::host_secret_callback`] surfaces. Set-once per
    /// process: subsequent calls leave the existing entry in place
    /// and return `Err(...)` semantics implicit in `OnceLock::set`
    /// (returns `Err(arc)` if already set).
    ///
    /// The production daemon (`graphreact-cli`'s `start` path) calls
    /// this once at startup with a closure that captures the chosen
    /// `Arc<dyn SecretStore>` + the scheduler-manifest reader. The
    /// integration test path bypasses this entirely and registers a
    /// `host_secret` callback directly via
    /// [`Engine::register_callback`], which avoids the OnceLock and
    /// gives each test fresh per-isolate semantics.
    ///
    /// Phase B Part 2 will widen this to an atomic-swap setter for
    /// SIGHUP rotation. The current set-once shape pins the contract
    /// for Phase B Part 1.
    pub fn set_host_secret_callback(
        &self,
        cb: Arc<dyn HostSecretCallback>,
    ) -> Result<(), Arc<dyn HostSecretCallback>> {
        host_secret_callback_shadow().set(cb)
    }

    /// **M-Prod sub-track #3 Phase B Part 1.** Reader for the
    /// process-global [`HostSecretCallback`] `Arc`. Mirrors
    /// [`Engine::host_secret_callback`] (which returns a `&self`-tied
    /// borrow); callers that need to clone the `Arc` past the borrow
    /// lifetime (e.g. into a `host_secret` BoxCallback closure in
    /// `graphreact-cli`'s daemon wiring) use this accessor.
    #[must_use]
    pub fn host_secret_callback_arc(&self) -> Option<Arc<dyn HostSecretCallback>> {
        host_secret_callback_shadow().get().cloned()
    }

    /// Register a component body as a re-callable function. Diverges
    /// from [`Engine::eval_module`] (which IIFE-wraps the body and
    /// stores the manifest as a one-shot value). This wraps as
    /// `(function(ctx){ <body> })` and stores the function so each
    /// render of the component re-invokes it — top-level hooks fire
    /// every time, which is required for state preservation.
    ///
    /// Caller invokes per render via [`Engine::call_function`] with a
    /// single-element args array `[ctx]` where `ctx` is the
    /// per-instance context (id, props, directory, …).
    ///
    /// JSC-specific (not on the [`Engine`] trait) because the
    /// component-body-as-function-expression pattern is JS-shaped —
    /// CPython, JVM, etc. would use language-native equivalents
    /// (decorators, classes) at this layer.
    pub fn eval_component(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        source: &str,
    ) -> EngineResult<FunctionHandle> {
        let mut function: u64 = 0;
        let rc = unsafe {
            ffi::gr_jsc_component_register(
                iso.0,
                name.as_ptr().cast(),
                name.len(),
                source.as_ptr().cast(),
                source.len(),
                &mut function,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(FunctionHandle(function))
    }

    /// Invoke a JS function installed on the isolate's global object
    /// by name. Symmetric counterpart to [`Engine::register_callback`]
    /// (which makes Rust callable from JS): this calls a JS-side
    /// function from Rust by global name.
    ///
    /// JSC-specific (not on the [`Engine`] trait) because not every
    /// engine has the same "lookup by global name" semantics —
    /// CPython, JVM, etc. don't expose runtime-defined globals the
    /// same way. The runtime's hook layer in phase 3 calls this
    /// directly.
    pub fn invoke_global(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        args: &[HostValue],
    ) -> EngineResult<HostValue> {
        invoke_global_static(iso, name, args)
    }
}

/// Free-function variant of [`JscEngine::drain_microtasks`] for
/// callsites that can't hold a `&mut JscEngine` borrow. **M12.3b-phase1.5.**
/// The async batch loop in `graphreact-cli/src/jsruntime.rs` drains
/// microtasks while awaiting deferred host-resolved Promises — the
/// caller is `JsRuntime::process_batch_async` which already holds an
/// `engine: Rc<RefCell<JscEngine>>` and can't re-borrow without
/// conflicting with the body-time `engine.borrow_mut().call_function(...)`
/// borrows that may be active for queued continuations.
///
/// Soundness mirrors [`invoke_global_static`]: JSC's state is
/// process-global, the iso handle is `Copy`-cheap and identifies the
/// underlying JSContext, and no `JscEngine` field is touched.
pub fn drain_microtasks_static(iso: IsolateHandle) -> EngineResult<()> {
    let rc = unsafe { ffi::gr_jsc_microtasks_drain(iso.0) };
    if rc != ffi::GR_OK {
        return Err(map_status(rc));
    }
    Ok(())
}

/// Free-function variant of [`JscEngine::invoke_global`] for callsites
/// that can't hold a `&mut JscEngine` borrow without conflicting with
/// the outer call frame's borrow. M12.3a-followon-F: the
/// `useEdgeAcceptHandler` predicate fires from inside `host_tier3_*`
/// callbacks (which themselves fire from inside an active
/// `engine.borrow_mut().call_function(...)` borrow held by the body
/// being evaluated). The predicate body needs to re-enter JSC by
/// calling `__invokeCallback` — but going through the regular
/// `engine.borrow_mut().invoke_global(...)` would double-borrow.
///
/// JSC's underlying state is process-global and JSObjectCallAsFunction
/// is re-entrant; the borrow conflict is purely Rust-level. This
/// function bypasses the RefCell by routing the FFI call through the
/// process-global isolate state directly, which is sound because
/// (a) the iso handle is `Copy`-cheap and identifies the same
/// JSContext, (b) no `JscEngine` field is read or written.
pub fn invoke_global_static(
    iso: IsolateHandle,
    name: &str,
    args: &[HostValue],
) -> EngineResult<HostValue> {
    {
        let args_json = host_args_to_json(args)?;
        let result_text = call_with_buffer(|buf, cap, written| unsafe {
            ffi::gr_jsc_callback_invoke(
                iso.0,
                name.as_ptr().cast(),
                name.len(),
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
}

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
        ffi::GR_ERR_INTERNAL => EngineError::Internal("jsc bridge internal error".into()),
        other => EngineError::Internal(format!("unknown jsc bridge status code: {other}")),
    }
}

/// Run a closure that produces a UTF-8 string into a caller-owned
/// buffer using the bridge's "in/out length" convention. Returns the
/// allocated `String` on success.
fn read_string_buffer<F>(mut call: F) -> EngineResult<String>
where
    F: FnMut(*mut std::os::raw::c_char, *mut usize) -> i32,
{
    let mut len: usize = 0;
    let rc = call(std::ptr::null_mut(), &mut len);
    if rc == ffi::GR_ERR_BUFFER_TOO_SMALL {
        // Expected: len now holds the required size.
    } else if rc != ffi::GR_OK {
        return Err(map_status(rc));
    }
    let mut buf = vec![0_u8; len];
    let mut written = len;
    let rc = call(buf.as_mut_ptr().cast(), &mut written);
    if rc != ffi::GR_OK {
        return Err(map_status(rc));
    }
    buf.truncate(written);
    String::from_utf8(buf)
        .map_err(|_| EngineError::Internal("non-utf8 string from jsc bridge".into()))
}

/// Run a closure that produces a JSON-shaped result (e.g. function
/// return value) into a caller-owned buffer. Resizes up to
/// [`RESULT_BUFFER_MAX`] on `GR_ERR_BUFFER_TOO_SMALL`.
fn call_with_buffer<F>(mut call: F) -> EngineResult<String>
where
    F: FnMut(*mut std::os::raw::c_char, usize, *mut usize) -> i32,
{
    let mut cap = RESULT_BUFFER_INITIAL;
    loop {
        let mut buf = vec![0_u8; cap];
        let mut written: usize = cap;
        let rc = call(buf.as_mut_ptr().cast(), cap, &mut written);
        if rc == ffi::GR_OK {
            buf.truncate(written);
            return String::from_utf8(buf)
                .map_err(|_| EngineError::Internal("non-utf8 result from jsc bridge".into()));
        }
        if rc == ffi::GR_ERR_BUFFER_TOO_SMALL {
            cap = (written.max(cap * 2)).min(RESULT_BUFFER_MAX);
            if cap > RESULT_BUFFER_MAX {
                return Err(EngineError::Internal(
                    "jsc bridge result exceeded 1 MiB".into(),
                ));
            }
            continue;
        }
        return Err(map_status(rc));
    }
}

impl Engine for JscEngine {
    fn initialize(&mut self) -> EngineResult<()> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let rc = unsafe { ffi::gr_jsc_engine_initialize() };
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
        let rc = unsafe { ffi::gr_jsc_engine_shutdown() };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        self.initialized.store(false, Ordering::Release);
        // Drop every closure this engine owned. The Zig side has
        // already released the JS function objects via context
        // disposal, so the trampoline can no longer reach them.
        let mut state = self.state.lock();
        let mut registry = callback_registry().write();
        for ids in state.iso_callbacks.values() {
            for id in ids {
                registry.remove(id);
            }
        }
        state.iso_callbacks.clear();
        Ok(())
    }

    fn create_isolate(&mut self, _opts: IsolateOptions) -> EngineResult<IsolateHandle> {
        self.initialize()?;
        let mut handle: u64 = 0;
        let rc = unsafe { ffi::gr_jsc_isolate_create(&mut handle) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(IsolateHandle(handle))
    }

    fn dispose_isolate(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        let rc = unsafe { ffi::gr_jsc_isolate_dispose(iso.0) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        // The Zig side freed the per-isolate `CallbackEntry` structs
        // and unprotected the JS function objects with the context;
        // drop the Rust-side closures that belonged to this isolate
        // so they can be GC'd.
        if let Some(ids) = self.state.lock().iso_callbacks.remove(&iso.0) {
            let mut registry = callback_registry().write();
            for id in ids {
                registry.remove(&id);
            }
        }
        Ok(())
    }

    fn eval_module(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        source: &str,
    ) -> EngineResult<ModuleHandle> {
        let mut module: u64 = 0;
        let rc = unsafe {
            ffi::gr_jsc_module_register(
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

    fn resolve_function(
        &mut self,
        iso: IsolateHandle,
        module: ModuleHandle,
        name: &str,
    ) -> EngineResult<FunctionHandle> {
        let mut function: u64 = 0;
        let rc = unsafe {
            ffi::gr_jsc_function_resolve(
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
        // The Zig side allocates a globally-unique id and writes it
        // back via `out_callback_id`. We use that as the key into
        // [`CALLBACK_REGISTRY`]. `cb_data` is reserved for future
        // engine-identity routing and intentionally left null.
        let mut id: u64 = 0;
        let rc = unsafe {
            ffi::gr_jsc_callback_register(
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

        // Box → Arc for storage. JSC may invoke the closure between
        // registry lookups under load; Arc lets the trampoline drop
        // the lock before invocation.
        let shared: SharedCallback = Arc::from(cb);
        callback_registry().write().insert(id, shared);
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
        let args_json = host_args_to_json(args)?;
        let result_text = call_with_buffer(|buf, cap, written| unsafe {
            ffi::gr_jsc_function_call(
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

    fn drain_microtasks(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        let rc = unsafe { ffi::gr_jsc_microtasks_drain(iso.0) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(())
    }

    fn engine_name(&self) -> &'static str {
        "jsc"
    }

    fn engine_version(&self) -> String {
        // Phase1: query the version string from the Zig side.
        read_string_buffer(|buf, len| unsafe { ffi::gr_jsc_engine_version(buf, len) })
            .unwrap_or_else(|_| "jsc-unknown".to_string())
    }

    /// **M-Prod sub-track #3 Phase B Part 1.** Surface the
    /// process-global [`HostSecretCallback`] installed via
    /// [`Self::set_host_secret_callback`]. Returns `None` when no
    /// callback has been registered — bodies that call
    /// `ctx.secret(name)` then see the prelude's `if (typeof
    /// host_secret === 'undefined') return null;` guard in effect
    /// and get a `null` (default-deny) without crashing.
    ///
    /// The borrow points into the process-static `OnceLock`-backed
    /// shadow; it is `'static` in practice and the trait-required
    /// `'_` lifetime is upheld by tying it to `&self`. See ADR-0007
    /// for the Phase B Part 2 widening plan that swaps the shadow
    /// to an atomic-swap cell.
    fn host_secret_callback(&self) -> Option<&dyn HostSecretCallback> {
        host_secret_callback_shadow()
            .get()
            .map(|arc| arc.as_ref() as &dyn HostSecretCallback)
    }
}

/// Phase 2.5 trampoline. JSC's `callAsFunction` callback inside
/// `jsc.zig` calls this directly with:
///
/// - `cb_data` — the registry id (encoded as a fake pointer; the Zig
///   side stores the id there at register time so we don't need
///   per-engine state to look up the closure).
/// - `args_json` — JSON-array string of the arguments JSC passed.
/// - `out_result` — caller-owned buffer the trampoline writes the
///   JSON-serialised closure result into.
///
/// Returns `GR_OK` on success, `GR_ERR_BUFFER_TOO_SMALL` (with the
/// required size in `*out_result_len`) when the result doesn't fit,
/// `GR_ERR_CALLBACK_NOT_FOUND` if the id isn't in the registry, or
/// `GR_ERR_INTERNAL` on any other failure (the Zig side will surface
/// these as JS exceptions).
unsafe extern "C" fn host_callback_trampoline(
    cb_data: *mut c_void,
    args_json: *const c_char,
    args_len: usize,
    out_result: *mut c_char,
    out_result_cap: usize,
    out_result_len: *mut usize,
) -> c_int {
    // SAFETY: `cb_data` is the id encoded as a pointer on the Zig
    // side; reading it back is a pure integer cast.
    let id = cb_data as usize as u64;

    let cb = {
        let registry = callback_registry().read();
        registry.get(&id).cloned()
    };
    let Some(cb) = cb else {
        return ffi::GR_ERR_CALLBACK_NOT_FOUND;
    };

    // Decode args.
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

    // Invoke.
    let result = match cb(&args) {
        Ok(v) => v,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };

    // Serialise.
    let result_json = match serde_json::to_string(&result.into_json()) {
        Ok(s) => s,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };

    // Write into the caller's buffer using the bridge length convention.
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

fn json_to_host_value(value: serde_json::Value) -> HostValue {
    HostValue::from_json(value)
}

/// Serialise an args slice as a JSON array suitable for the bridge.
/// Routes each [`HostValue`] through [`HostValue::into_json`] so
/// `HostValue::Object` becomes a real JSON object (not the
/// `[[K,V],…]` shape that `#[serde(untagged)]` on a
/// `Vec<(String, HostValue)>` produces by default). Symmetrical with
/// the JSC trampoline's `from_json`-based deserialisation.
fn host_args_to_json(args: &[HostValue]) -> EngineResult<String> {
    let array = serde_json::Value::Array(args.iter().cloned().map(HostValue::into_json).collect());
    serde_json::to_string(&array)
        .map_err(|e| EngineError::Internal(format!("args json serialise: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// JSC's `JSContextGroup` is process-global; tests must serialise.
    fn engine_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn engine_initialize_then_shutdown_idempotent() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init ok");
        e.initialize().expect("init idempotent");
        e.shutdown().expect("shutdown ok");
        e.shutdown().expect("shutdown idempotent");
    }

    #[test]
    fn create_dispose_isolate_round_trip() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        assert!(iso.0 > 0, "handle should be non-zero");
        let live_before = e.live_isolates();
        assert!(live_before >= 1);
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn engine_name_is_jsc() {
        let e = JscEngine::new();
        assert_eq!(e.engine_name(), "jsc");
    }

    #[test]
    fn engine_version_starts_with_jsc() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let v = e.engine_version();
        assert!(v.starts_with("jsc-"), "version: {v}");
        e.shutdown().expect("shutdown");
    }

    /// Read CURRENT resident-set size of this process by shelling
    /// out to `ps -o rss= -p <pid>`. On macOS `ps` reports RSS in
    /// kilobytes; we convert to bytes for cross-call comparability.
    ///
    /// Used over `getrusage(RUSAGE_SELF).ru_maxrss` because the
    /// latter is a *peak* watermark — monotonically non-decreasing
    /// across the process lifetime, useless for steady-state
    /// comparisons.
    fn current_rss_bytes() -> u64 {
        let pid = unsafe { libc::getpid() };
        let output = match std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
        {
            Ok(o) => o,
            Err(_) => return 0,
        };
        let s = String::from_utf8_lossy(&output.stdout);
        let kb: u64 = s.trim().parse().unwrap_or(0);
        kb * 1024
    }

    /// 1,000-cycle isolate stress test (M12.3a acceptance bar).
    ///
    /// **What we're really testing:** that JSC's per-group memory
    /// pool reaches steady state during isolate churn. JSC GROWS
    /// internal pools during the first batch of cycles regardless of
    /// leak-status — JIT code caches, GC arenas, type feedback
    /// vectors are sized lazily based on workload. The plan's "RSS
    /// stable ±5 MiB" target is meaningful only POST steady-state.
    ///
    /// **Strategy:** two 1000-cycle loops. The first loop establishes
    /// JSC's working set (allowed up to a generous 50 MiB cap as a
    /// loose true-leak ceiling). The second 1000-cycle loop should
    /// add ≤5 MiB more — the actual "steady state stable" claim.
    ///
    /// Plus the basic post-conditions: `live_isolates()` zero after
    /// `shutdown`.
    #[test]
    fn isolate_stress_1000_cycles_under_5mib_steady_state_growth() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");

        // Burn-in: small warm-up before the first 1000-cycle batch.
        for _ in 0..50 {
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.dispose_isolate(iso).expect("dispose");
        }
        let r0 = current_rss_bytes();

        // First 1000 — JSC's pools grow to working-set size.
        for _ in 0..1000 {
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.dispose_isolate(iso).expect("dispose");
        }
        let r1 = current_rss_bytes();
        let working_set_growth = r1.saturating_sub(r0);
        let working_set_cap = 50 * 1024 * 1024; // 50 MiB — loose ceiling for true-leak detection
        assert!(
            working_set_growth <= working_set_cap,
            "JSC pool growth across first 1000 cycles exceeded 50 MiB \
             (potential leak): r0={r0}B r1={r1}B growth={working_set_growth}B"
        );

        // Second 1000 — should be at steady state.
        for _ in 0..1000 {
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.dispose_isolate(iso).expect("dispose");
        }
        let r2 = current_rss_bytes();
        let steady_growth = r2.saturating_sub(r1);
        let steady_cap = 5 * 1024 * 1024; // 5 MiB — the actual acceptance bar
        assert!(
            steady_growth <= steady_cap,
            "RSS growth across SECOND 1000 cycles exceeded 5 MiB \
             (steady-state instability): r1={r1}B r2={r2}B growth={steady_growth}B"
        );

        e.shutdown().expect("shutdown");
        assert_eq!(e.live_isolates(), 0, "expected zero live after shutdown");
    }

    #[test]
    fn isolate_stress_create_dispose_no_leaks() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        for _ in 0..200 {
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.dispose_isolate(iso).expect("dispose");
        }
        // Live count should drop back to (whatever was there before — at
        // most a small bounded number from earlier tests in the same
        // process). The strict check is against post-shutdown.
        e.shutdown().expect("shutdown");
        assert_eq!(e.live_isolates(), 0, "expected zero live after shutdown");
    }

    #[test]
    fn dummy_module_register_round_trip() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e.eval_module(iso, "test", "return null;").expect("eval ok");
        assert!(m.0 > 0);
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase2 acceptance: eval a body that returns a manifest with an
    /// `init` function returning a known value, resolve `init`, call
    /// it, and assert the value comes back through JSON marshalling.
    #[test]
    fn phase2_eval_resolve_call_round_trip_returns_computed_value() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e
            .eval_module(
                iso,
                "phase2-test",
                "return { init: function() { return 1 + 41; } };",
            )
            .expect("eval ok");
        let init = e.resolve_function(iso, m, "init").expect("resolve init");
        let result = e.call_function(iso, init, &[]).expect("call init");
        match result {
            HostValue::Number(n) => assert!((n - 42.0).abs() < f64::EPSILON, "got {n}"),
            other => panic!("expected Number(42.0), got {other:?}"),
        }
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase2: returning an object survives the JSON round-trip with
    /// keys preserved.
    #[test]
    fn phase2_call_returning_object_marshals_through_json() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e
            .eval_module(
                iso,
                "phase2-obj",
                "return { init: function() { return { hello: 'world', count: 7 }; } };",
            )
            .expect("eval ok");
        let init = e.resolve_function(iso, m, "init").expect("resolve");
        let result = e.call_function(iso, init, &[]).expect("call");
        let HostValue::Object(pairs) = result else {
            panic!("expected Object, got {result:?}");
        };
        let map: std::collections::HashMap<String, &HostValue> =
            pairs.iter().map(|(k, v)| (k.clone(), v)).collect();
        assert!(matches!(map.get("hello"), Some(HostValue::String(s)) if s == "world"));
        assert!(
            matches!(map.get("count"), Some(HostValue::Number(n)) if (*n - 7.0).abs() < f64::EPSILON)
        );
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase2: hook prelude installs no-op stubs so component bodies
    /// that reference `useState`, `useEmit`, etc. don't ReferenceError.
    /// Tests by evaluating a body that uses several hooks; if the
    /// hook stubs weren't installed, eval_module would fail.
    ///
    /// **Phase 3a note:** the global `ctx` stub was removed when ctx
    /// became a function argument via `eval_component`. The
    /// IIFE-style `eval_module` path no longer carries ctx; bodies
    /// that need ctx must use `eval_component`.
    #[test]
    fn phase2_hook_prelude_lets_component_bodies_reference_hooks() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let body = "\
            const [count, setCount] = useState(0);\n\
            const emit = useEmit();\n\
            useMessage('ping', () => {});\n\
            useEffect(() => {}, []);\n\
            const x = useMemo(() => 99, []);\n\
            return { count, x, accepts: ['ping'], emits: ['pong'] };\n\
        ";
        let m = e
            .eval_module(iso, "hook-test", body)
            .expect("hooks resolve via prelude");
        assert!(m.0 > 0);
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 2.5: a Rust closure registered as `host_add` is callable
    /// from JS code. Verifies the JS→Rust direction: JSC's
    /// `callAsFunction` trampoline invokes the entry's stored Rust
    /// trampoline, which looks up the closure in
    /// [`CALLBACK_REGISTRY`], invokes it, and returns the result back
    /// through `JSValueMakeFromJSONString`.
    #[test]
    fn phase2_5_rust_closure_callable_from_js() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        e.register_callback(
            iso,
            "host_add",
            Box::new(|args| {
                let a = match args.first() {
                    Some(HostValue::Number(n)) => *n,
                    other => return Err(EngineError::Internal(format!("bad arg 0: {other:?}"))),
                };
                let b = match args.get(1) {
                    Some(HostValue::Number(n)) => *n,
                    other => return Err(EngineError::Internal(format!("bad arg 1: {other:?}"))),
                };
                Ok(HostValue::Number(a + b))
            }),
        )
        .expect("register host_add");

        let m = e
            .eval_module(
                iso,
                "phase2_5-add",
                "return { init: function() { return host_add(2, 3); } };",
            )
            .expect("eval ok");
        let init = e.resolve_function(iso, m, "init").expect("resolve");
        let result = e.call_function(iso, init, &[]).expect("call");
        match result {
            HostValue::Number(n) => assert!((n - 5.0).abs() < f64::EPSILON, "got {n}"),
            other => panic!("expected Number(5.0), got {other:?}"),
        }

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 2.5: bidirectional roundtrip. Rust calls a JS function;
    /// the JS function calls back into Rust via a registered host
    /// callback; the Rust callback observes the call by mutating a
    /// shared `Mutex<Vec<String>>`. Proves the trampoline runs
    /// closures with side effects (state-preserving callbacks are
    /// what phase 3 needs for hook integration).
    #[test]
    fn phase2_5_bidirectional_callback_observed_via_shared_state() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = Arc::clone(&observations);
        e.register_callback(
            iso,
            "host_notify",
            Box::new(move |args| {
                let msg = match args.first() {
                    Some(HostValue::String(s)) => s.clone(),
                    other => format!("non-string arg: {other:?}"),
                };
                observations_for_cb.lock().push(msg);
                Ok(HostValue::Null)
            }),
        )
        .expect("register host_notify");

        let m = e
            .eval_module(
                iso,
                "phase2_5-notify",
                "return { init: function() { \
                    host_notify('first'); \
                    host_notify('second'); \
                    return 'done'; \
                } };",
            )
            .expect("eval ok");
        let init = e.resolve_function(iso, m, "init").expect("resolve");
        let result = e.call_function(iso, init, &[]).expect("call");
        match result {
            HostValue::String(ref s) if s == "done" => {}
            other => panic!("expected String(\"done\"), got {other:?}"),
        }

        let collected = observations.lock();
        assert_eq!(
            collected.as_slice(),
            &["first".to_string(), "second".to_string()]
        );
        drop(collected);

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 2.5: `invoke_global` resolves a JS function on the
    /// isolate's global by name and calls it with JSON-marshalled
    /// args. Proves the symmetric path
    /// (`gr_jsc_callback_invoke`) — the runtime uses this in phase 3
    /// to call JS-installed hook helpers.
    #[test]
    fn phase2_5_invoke_global_calls_js_function_by_name() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        // Install a JS function on the global via eval_module's IIFE
        // wrapping (`globalThis.<name> = ...` survives the IIFE).
        e.eval_module(
            iso,
            "phase2_5-install-global-fn",
            "globalThis.double = function(x) { return x * 2; }; return null;",
        )
        .expect("eval install");

        let result = e
            .invoke_global(iso, "double", &[HostValue::Number(7.0)])
            .expect("invoke double");
        match result {
            HostValue::Number(n) => assert!((n - 14.0).abs() < f64::EPSILON, "got {n}"),
            other => panic!("expected Number(14.0), got {other:?}"),
        }

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3a: `eval_component` wraps the body as
    /// `(function(ctx){ <body> })` (function expression, NOT invoked).
    /// The returned handle is callable like any other JS function
    /// with a single arg slot for the per-instance ctx. This proves
    /// the body can be re-invoked across renders — the prerequisite
    /// for state preservation when phase 3b wires `useState`.
    #[test]
    fn phase3a_component_body_is_callable_per_render_with_ctx() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        // The body returns its observed ctx.id back as a string —
        // proves ctx is a real arg, not a global stub.
        let fn_handle = e
            .eval_component(
                iso,
                "Echo",
                "return { observedId: ctx.id, accepts: [], emits: [] };",
            )
            .expect("eval_component");

        let ctx_a = HostValue::Object(vec![
            ("id".to_string(), HostValue::String("node-a".to_string())),
            ("props".to_string(), HostValue::Object(vec![])),
        ]);
        let manifest_a = e.call_function(iso, fn_handle, &[ctx_a]).expect("call a");
        let HostValue::Object(pairs_a) = manifest_a else {
            panic!("expected manifest object");
        };
        let observed_a = pairs_a
            .iter()
            .find(|(k, _)| k == "observedId")
            .map(|(_, v)| v.clone());
        assert!(matches!(observed_a, Some(HostValue::String(ref s)) if s == "node-a"));

        // Same handle, different ctx → different observed id. Proves
        // the function is re-callable (not an IIFE one-shot).
        let ctx_b = HostValue::Object(vec![
            ("id".to_string(), HostValue::String("node-b".to_string())),
            ("props".to_string(), HostValue::Object(vec![])),
        ]);
        let manifest_b = e.call_function(iso, fn_handle, &[ctx_b]).expect("call b");
        let HostValue::Object(pairs_b) = manifest_b else {
            panic!("expected manifest object");
        };
        let observed_b = pairs_b
            .iter()
            .find(|(k, _)| k == "observedId")
            .map(|(_, v)| v.clone());
        assert!(matches!(observed_b, Some(HostValue::String(ref s)) if s == "node-b"));

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3a: ctx.props arrives intact — verifies nested-object
    /// marshalling through the JSON arg path.
    #[test]
    fn phase3a_component_observes_ctx_props() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        let fn_handle = e
            .eval_component(
                iso,
                "Reader",
                "return { threshold: ctx.props.threshold, label: ctx.props.label, accepts: [], emits: [] };",
            )
            .expect("eval_component");

        let ctx = HostValue::Object(vec![
            ("id".to_string(), HostValue::String("reader-1".to_string())),
            (
                "props".to_string(),
                HostValue::Object(vec![
                    ("threshold".to_string(), HostValue::Number(42.0)),
                    ("label".to_string(), HostValue::String("alpha".to_string())),
                ]),
            ),
        ]);
        let manifest = e.call_function(iso, fn_handle, &[ctx]).expect("call");
        let HostValue::Object(pairs) = manifest else {
            panic!("expected manifest object");
        };
        let by_key: std::collections::HashMap<String, HostValue> = pairs.into_iter().collect();
        assert!(
            matches!(by_key.get("threshold"), Some(HostValue::Number(n)) if (*n - 42.0).abs() < f64::EPSILON)
        );
        assert!(matches!(by_key.get("label"), Some(HostValue::String(s)) if s == "alpha"));

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3a: a body that uses top-level hooks (no IIFE) compiles
    /// and evaluates as a re-callable function. Proves the prelude
    /// continues to satisfy hook references when the body is wrapped
    /// as `(function(ctx){ ... })`.
    #[test]
    fn phase3a_component_with_top_level_hooks_compiles_and_runs() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        let body = "\
            const [count, setCount] = useState(0);\n\
            const emit = useEmit();\n\
            useMessage('ping', () => {});\n\
            useEffect(() => {}, []);\n\
            const x = useMemo(() => 99, []);\n\
            return { id: ctx.id, count, x, accepts: ['ping'], emits: ['pong'] };\n\
        ";
        let fn_handle = e.eval_component(iso, "WithHooks", body).expect("eval");

        let ctx = HostValue::Object(vec![
            ("id".to_string(), HostValue::String("hookful".to_string())),
            ("props".to_string(), HostValue::Object(vec![])),
        ]);
        let manifest = e.call_function(iso, fn_handle, &[ctx]).expect("call");
        let HostValue::Object(pairs) = manifest else {
            panic!("expected manifest object");
        };
        let by_key: std::collections::HashMap<String, HostValue> = pairs.into_iter().collect();
        assert!(matches!(by_key.get("id"), Some(HostValue::String(s)) if s == "hookful"));
        // Phase 3a hooks are still no-op stubs: useState returns
        // [initial, noop], so count == 0; useMemo returns the factory
        // result, so x == 99.
        assert!(
            matches!(by_key.get("count"), Some(HostValue::Number(n)) if (*n - 0.0).abs() < f64::EPSILON)
        );
        assert!(
            matches!(by_key.get("x"), Some(HostValue::Number(n)) if (*n - 99.0).abs() < f64::EPSILON)
        );

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 2.5: callbacks are removed from the global registry on
    /// `dispose_isolate`. Closures shouldn't outlive the JS function
    /// objects they back.
    #[test]
    fn phase2_5_dispose_isolate_drops_registered_callbacks() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        let id = e
            .register_callback(iso, "host_x", Box::new(|_| Ok(HostValue::Null)))
            .expect("register host_x");

        assert!(callback_registry().read().contains_key(&id.0));
        e.dispose_isolate(iso).expect("dispose");
        assert!(!callback_registry().read().contains_key(&id.0));

        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // M16-Backend Phase 9 — JSC RemoteInspector enable/round-trip
    // -----------------------------------------------------------------
    //
    // The headless smoke gate for P9: enable_debugger flips the
    // per-context inspection flag; is_inspector_enabled reads it
    // back; the JscEngine's AtomicBool guard short-circuits repeat
    // installs. Lockdownd discovery + Safari Web Inspector attach
    // are the manual gate documented in `docs/backend/m16-debug-jsc.md`.

    #[test]
    fn p9_enable_debugger_flips_inspection_flag() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        // Pre-condition: a fresh JSGlobalContext defaults to non-inspectable.
        let pre = e.is_inspector_enabled(iso).expect("read pre");
        assert!(!pre, "fresh context should default to inspection=false");
        // The act: install RemoteInspector listener + flip per-context flag.
        e.enable_debugger(iso, 9229).expect("enable_debugger");
        // Post-condition: per-context flag reads back true.
        let post = e.is_inspector_enabled(iso).expect("read post");
        assert!(post, "context inspection flag should be true post-install");
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn p9_enable_debugger_idempotent_via_atomic_bool_guard() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        e.enable_debugger(iso, 9229).expect("first enable");
        // Repeat call short-circuits via the AtomicBool guard before
        // crossing the FFI boundary; observable as Ok(()) without
        // any framework-level error.
        e.enable_debugger(iso, 9229)
            .expect("second enable (idempotent)");
        e.enable_debugger(iso, 9229)
            .expect("third enable (idempotent)");
        let after = e.is_inspector_enabled(iso).expect("read after");
        assert!(
            after,
            "context flag should still be true after idempotent calls"
        );
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn p9_enable_debugger_invalid_isolate_returns_error() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        // Garbage handle — never minted by gr_jsc_isolate_create.
        let bogus = IsolateHandle(u64::MAX);
        let err = e
            .enable_debugger(bogus, 9229)
            .expect_err("garbage handle must error");
        assert!(
            matches!(err, EngineError::InvalidIsolate),
            "expected InvalidIsolate, got: {err:?}"
        );
        e.shutdown().expect("shutdown");
    }
}
