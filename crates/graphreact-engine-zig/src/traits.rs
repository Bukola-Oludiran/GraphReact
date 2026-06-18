//! The core [`Engine`] trait.

use crate::error::EngineResult;
use crate::handles::{CallbackId, FunctionHandle, IsolateHandle, ModuleHandle};
use crate::options::IsolateOptions;
use crate::value::HostValue;

/// A single host-call from the engine into Rust. Engine implementers
/// produce these inside [`Engine::call_function`] /
/// [`Engine::drain_microtasks`] when guest code invokes a registered
/// callback (typical names: `host_send`, `host_log`, `host_dispatch`).
#[derive(Debug, Clone, PartialEq)]
pub struct HostCall {
    pub callback: CallbackId,
    pub callback_name: String,
    pub args: Vec<HostValue>,
}

/// Boxed callback type. The runtime registers one per host-exposed
/// function name. Engine impls invoke these synchronously when guest
/// code calls the corresponding name.
pub type BoxCallback = Box<dyn Fn(&[HostValue]) -> EngineResult<HostValue> + Send + Sync>;

/// **M-Prod sub-track #3 Phase B Part 1.** Host-side callback the
/// engine consults when guest code invokes `ctx.secret(name)`.
/// Implementations close over an `Arc<dyn SecretStore>` (and Phase B
/// Part 2: the active node's allowlist). Object-safe + `Send + Sync`
/// so the engine can store it behind `&dyn HostSecretCallback` and
/// dispatch from worker threads.
///
/// Return `None` for default-deny + unmapped names. The engine is
/// responsible for marshalling `Some(bytes)` into the guest's native
/// secret shape (JSC: UTF-8 string; CPython: bytes object; Go:
/// `[]byte`).
///
/// The `SecretValue` carries the `Zeroize` + `ZeroizeOnDrop` derive
/// so the bytes are scrubbed when the value is dropped — but only
/// the original Rust-side `Vec<u8>`; once the bytes cross the
/// engine ABI into the guest VM's heap, that copy is outside the
/// zeroize regime. See ADR-0007 for the boundary discussion + the
/// Phase B Part 2 widening plan to typed-bytes.
pub trait HostSecretCallback: Send + Sync {
    /// Read a secret by name. The implementation enforces the
    /// per-component `needs_secret` allowlist; `None` is the
    /// default-deny signal that bodies see as `null` / `None` /
    /// zero-value depending on the guest language.
    fn read(&self, name: &str) -> Option<graphreact_secrets::SecretValue>;
}

/// Interpreted "what should the engine do next" signal.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineCallback {
    /// Re-enter the guest with the result of a host call.
    Resume(HostValue),
    /// Abort the current guest invocation with an error.
    Throw(String),
}

/// The contract between the GraphReact runtime and a host language.
///
/// Engine impls do **not** hold the runtime — the runtime drives them.
/// The trait is `&mut self` throughout so impls can hold internal
/// mutable state (handle tables, scope guards, …) without locking.
pub trait Engine {
    /// Initialise engine-global state (JSC `JSContextGroup`, V8
    /// platform, CPython initialisation, …). Called once per process.
    fn initialize(&mut self) -> EngineResult<()>;

    /// Tear down engine-global state. Called once per process.
    /// Implementations should be idempotent — both JSC's
    /// `JSContextGroupRelease` and V8's `v8::V8::DisposePlatform` are
    /// process-fatal if double-called.
    fn shutdown(&mut self) -> EngineResult<()>;

    /// Create a fresh isolate / interpreter / VM. Each isolate is
    /// independent — guest code in isolate A cannot see globals from
    /// isolate B.
    fn create_isolate(&mut self, opts: IsolateOptions) -> EngineResult<IsolateHandle>;

    /// Dispose an isolate. After this call, the handle is invalid;
    /// further calls error with [`EngineError::InvalidIsolate`].
    fn dispose_isolate(&mut self, iso: IsolateHandle) -> EngineResult<()>;

    /// Compile + evaluate a module in the given isolate. Returns a
    /// handle to the module's exported namespace.
    ///
    /// `module_name` is a synthetic name used only in error messages
    /// (no module resolver is attached at this layer).
    fn eval_module(
        &mut self,
        iso: IsolateHandle,
        module_name: &str,
        source: &str,
    ) -> EngineResult<ModuleHandle>;

    /// Resolve a named export from a module to a callable function.
    fn resolve_function(
        &mut self,
        iso: IsolateHandle,
        module: ModuleHandle,
        name: &str,
    ) -> EngineResult<FunctionHandle>;

    /// Register a host callback. Guest code calling `name` invokes the
    /// callback synchronously. Returns the callback id assigned by the
    /// engine.
    fn register_callback(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        callback: BoxCallback,
    ) -> EngineResult<CallbackId>;

    /// Invoke a function in the guest with the given arguments.
    fn call_function(
        &mut self,
        iso: IsolateHandle,
        function: FunctionHandle,
        args: &[HostValue],
    ) -> EngineResult<HostValue>;

    /// Drain pending microtasks. The runtime calls this between
    /// scheduler ticks; with [`MicrotaskPolicy::DrainEachTick`] this is
    /// a no-op when the engine drains automatically.
    fn drain_microtasks(&mut self, iso: IsolateHandle) -> EngineResult<()>;

    /// Engine identifier, e.g. `"jsc"`, `"cpython"`, `"null"`. Surfaced
    /// in `grx version` and engine-selection diagnostics.
    fn engine_name(&self) -> &'static str;

    /// Engine version string, e.g. JSC major.minor or `"null-1.0"`.
    fn engine_version(&self) -> String;

    /// **M-Prod sub-track #3 Phase B Part 1.** Optional host-secret
    /// callback surface. Engines that wire `ctx.secret(name)`
    /// override to return `Some(...)`. The default `None` keeps
    /// non-secret-aware engines (cpython / go / echo / null / zig)
    /// compiling unchanged. See ADR-0007.
    ///
    /// The orchestrator queries this method as the gating predicate
    /// for whether to install the JS-side `ctx.secret` shim; engines
    /// that return `None` keep the prelude's `if (typeof host_secret
    /// === 'undefined') return null;` guard in effect so bodies that
    /// call `ctx.secret(name)` get a `null` (default-deny) without
    /// crashing.
    fn host_secret_callback(&self) -> Option<&dyn HostSecretCallback> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check: the trait is object-safe (`dyn Engine`
    /// works). The CLI shim hands a `Box<dyn Engine>` between
    /// subcommands, so this property is load-bearing.
    #[test]
    fn engine_trait_is_object_safe() {
        fn _take_dyn(_: &mut dyn Engine) {}
    }
}
