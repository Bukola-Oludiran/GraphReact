//! Dynamic loader for the Go-side c-shared artifact.
//!
//! Resolves the `gr_go_*` function pointers from a `libgrx_go.{so,dylib,dll}`
//! produced by `graphreact-codegen-go` (`go build -buildmode=c-shared`)
//! and holds them in a [`LoadedRuntime`] that the engine owns for the
//! process lifetime.
//!
//! # Safety invariants
//!
//! - The held [`libloading::Library`] outlives every raw function
//!   pointer this module hands out. The pointers borrow from the
//!   `Library`; dropping it makes them dangling.
//! - The Go runtime is **not re-initialisable** within a single
//!   process — cgo's `runtime.LockOSThread` + the c-shared runtime
//!   initialise on first call and cannot be cleanly torn down.
//!   `GoEngine::shutdown` therefore does NOT drop the `Library`; the
//!   engine holds it inside an `OnceLock<Arc<LoadedRuntime>>` so re-
//!   initialisation reuses the same handle.
//! - ABI mismatch is detected by calling [`gr_go_abi_version_fn`]
//!   FIRST after `Library::new`; on mismatch the loader returns an
//!   error and never touches any other symbol.

#![allow(unsafe_code)]

use std::path::Path;

use libloading::{Library, Symbol};

use graphreact_engine_zig::{EngineError, EngineResult};

use crate::ffi;

/// One resolved-and-validated Go c-shared artifact. Holds the
/// [`Library`] handle plus the raw function pointers for every
/// `gr_go_*` exported symbol.
///
/// Field order is **load-bearing**: Rust drops struct fields in
/// declaration order, so every function-pointer field is declared
/// BEFORE `_lib` to ensure the Library is the LAST field dropped.
/// The fn-ptrs are `Copy` so dropping them is a no-op, but if a
/// future refactor makes them non-Copy or adds a non-Copy field
/// holding pointers into the Library, the declaration-last-drops-
/// last rule keeps the documented "Library outlives fn-ptrs"
/// invariant honest.
pub struct LoadedRuntime {
    pub abi_version: ffi::gr_go_abi_version_fn,

    pub engine_initialize: ffi::gr_go_engine_initialize_fn,
    pub engine_shutdown: ffi::gr_go_engine_shutdown_fn,
    pub engine_live_isolates: Option<ffi::gr_go_engine_live_isolates_fn>,

    pub isolate_create: ffi::gr_go_isolate_create_fn,
    pub isolate_dispose: ffi::gr_go_isolate_dispose_fn,

    pub module_register: ffi::gr_go_module_register_fn,
    pub function_resolve: ffi::gr_go_function_resolve_fn,
    pub function_call: ffi::gr_go_function_call_fn,

    pub callback_register: ffi::gr_go_callback_register_fn,

    pub microtasks_drain: ffi::gr_go_microtasks_drain_fn,

    /// **M17 phase 3c.** Rust → Go handler-invocation symbol. Optional
    /// so older phase 3b-real artifacts that don't ship the symbol
    /// keep loading; in that case, scheduled `host_message` deliveries
    /// will surface a clear runtime error rather than dlopen-fail at
    /// load time.
    pub invoke_handler: Option<ffi::gr_go_invoke_handler_fn>,

    /// **M17 phase 3b-complete.** Rust → Go envelope-shaped callback
    /// invocation symbol. Optional; older phase 3c artifacts that
    /// don't ship it stay loadable but `host_effect` cleanup will
    /// surface a clear runtime error.
    pub invoke_callback: Option<ffi::gr_go_invoke_callback_fn>,

    /// **M17 phase 3b-complete.** Rust → Go callback-release symbol.
    /// Optional.
    pub release_callback: Option<ffi::gr_go_release_callback_fn>,

    /// Owns the dlopen handle. Declared LAST so it drops LAST —
    /// any future non-Copy field pointing into the Library is
    /// safely outlived. The leading underscore documents that we
    /// never read this field directly; its job is lifetime control.
    _lib: Library,
}

/// SAFETY: every field is a raw `unsafe extern "C" fn` pointer. The
/// Go runtime itself manages thread safety on the callee side via
/// `sync.Mutex` serialization per isolate (M17 design decision); the
/// Rust side serialises calls via the `Engine` trait's `&mut self`
/// borrow. The `Library` handle is `Send + Sync` per `libloading`'s
/// documented guarantees for `Library`.
unsafe impl Send for LoadedRuntime {}
unsafe impl Sync for LoadedRuntime {}

impl LoadedRuntime {
    /// Load a Go c-shared artifact from `library_path` and resolve
    /// every required symbol.
    ///
    /// # Errors
    ///
    /// - [`EngineError::Internal`] wrapping the dlopen / LoadLibraryW
    ///   error if the library can't be loaded.
    /// - [`EngineError::Internal`] if any required symbol is missing
    ///   (the message names the symbol so codegen-vs-loader version
    ///   drift is debuggable).
    /// - [`EngineError::Internal`] flagging the ABI mismatch if the
    ///   loaded library's `GrxAbiVersion` does not equal
    ///   [`ffi::GRX_GO_ABI_VERSION`].
    pub fn load(library_path: &Path) -> EngineResult<Self> {
        // SAFETY: `Library::new` is unsafe because it runs the
        // library's static initialisers, which can execute arbitrary
        // code on load. We trust the artifact produced by
        // `graphreact-codegen-go` — its only initialiser is the cgo
        // runtime bootstrap.
        let lib = unsafe { Library::new(library_path) }.map_err(|e| {
            EngineError::Internal(format!(
                "graphreact-engine-go: failed to dlopen {}: {e}",
                library_path.display()
            ))
        })?;

        // Resolve ABI version FIRST and validate before touching any
        // other symbol.
        let abi_version_sym: Symbol<ffi::gr_go_abi_version_fn> =
            unsafe { lib.get(b"GrxAbiVersion\0") }.map_err(|e| symbol_error("GrxAbiVersion", e))?;
        let abi_version = *abi_version_sym;
        // Symbol borrows from `lib`; the deref above copies out the
        // raw fn-ptr. The Symbol goes out of scope at the end of
        // this block — clippy's drop_non_drop lint correctly notes
        // that explicit drop is a no-op for a borrow-only type.

        let version = unsafe { abi_version() };
        if version != ffi::GRX_GO_ABI_VERSION {
            return Err(EngineError::Internal(format!(
                "graphreact-engine-go: ABI version mismatch — artifact at {} reports v{}, \
                 loader expects v{}. Rebuild via `grx build` against the current toolchain.",
                library_path.display(),
                version,
                ffi::GRX_GO_ABI_VERSION,
            )));
        }

        // Resolve every required symbol. Each `get` returns a
        // `Symbol` that borrows from `lib`; we deref to the raw
        // function pointer + drop the Symbol immediately so the
        // borrow doesn't pin the field.
        let engine_initialize = resolve::<ffi::gr_go_engine_initialize_fn>(&lib, "GrxEngineInitialize")?;
        let engine_shutdown = resolve::<ffi::gr_go_engine_shutdown_fn>(&lib, "GrxEngineShutdown")?;

        // engine_live_isolates is optional — used only by stress
        // tests. Missing-symbol is not fatal.
        let engine_live_isolates =
            resolve_optional::<ffi::gr_go_engine_live_isolates_fn>(&lib, "GrxEngineLiveIsolates");

        let isolate_create = resolve::<ffi::gr_go_isolate_create_fn>(&lib, "GrxIsolateCreate")?;
        let isolate_dispose = resolve::<ffi::gr_go_isolate_dispose_fn>(&lib, "GrxIsolateDispose")?;

        let module_register = resolve::<ffi::gr_go_module_register_fn>(&lib, "GrxModuleRegister")?;
        let function_resolve =
            resolve::<ffi::gr_go_function_resolve_fn>(&lib, "GrxFunctionResolve")?;
        let function_call = resolve::<ffi::gr_go_function_call_fn>(&lib, "GrxFunctionCall")?;

        let callback_register =
            resolve::<ffi::gr_go_callback_register_fn>(&lib, "GrxCallbackRegister")?;

        let microtasks_drain =
            resolve::<ffi::gr_go_microtasks_drain_fn>(&lib, "GrxMicrotasksDrain")?;

        // M17 phase 3c — optional. Older artifacts that don't ship
        // the symbol can still be hot-reload short-circuited; only
        // host_message-driven dispatch needs it.
        let invoke_handler =
            resolve_optional::<ffi::gr_go_invoke_handler_fn>(&lib, "GrxInvokeHandler");

        // M17 phase 3b-complete — optional. Older phase 3c artifacts
        // that don't ship these stay loadable; only host_effect-
        // driven dispatch needs them.
        let invoke_callback =
            resolve_optional::<ffi::gr_go_invoke_callback_fn>(&lib, "GrxInvokeCallback");
        let release_callback =
            resolve_optional::<ffi::gr_go_release_callback_fn>(&lib, "GrxReleaseCallback");

        Ok(Self {
            abi_version,
            engine_initialize,
            engine_shutdown,
            engine_live_isolates,
            isolate_create,
            isolate_dispose,
            module_register,
            function_resolve,
            function_call,
            callback_register,
            microtasks_drain,
            invoke_handler,
            invoke_callback,
            release_callback,
            _lib: lib,
        })
    }
}

fn symbol_error(name: &str, e: libloading::Error) -> EngineError {
    EngineError::Internal(format!(
        "graphreact-engine-go: required symbol `{name}` missing from artifact: {e}. \
         The artifact was likely built with an older codegen pass; \
         rebuild via `grx build`."
    ))
}

fn resolve<F: Copy>(lib: &Library, name: &str) -> EngineResult<F> {
    let mangled = format!("{name}\0");
    // SAFETY: `Library::get` is unsafe because the resolved symbol's
    // signature must match `F`. We control both ends of this
    // boundary: the Rust signature in `crate::ffi` and the Go
    // `//export` declaration in `graphreact-codegen-go`'s emitted
    // prelude. Mismatch is caught by the ABI version handshake done
    // BEFORE any non-`GrxAbiVersion` symbol is read.
    let sym: Symbol<F> =
        unsafe { lib.get(mangled.as_bytes()) }.map_err(|e| symbol_error(name, e))?;
    Ok(*sym)
}

fn resolve_optional<F: Copy>(lib: &Library, name: &str) -> Option<F> {
    let mangled = format!("{name}\0");
    // SAFETY: same justification as `resolve`. We return None on
    // any error since optional symbols by definition may be absent.
    let sym: Option<Symbol<F>> = unsafe { lib.get(mangled.as_bytes()) }.ok();
    sym.map(|s| *s)
}
