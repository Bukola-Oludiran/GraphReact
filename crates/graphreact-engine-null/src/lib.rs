//! Null engine — implements [`graphreact_engine_zig::Engine`] without
//! executing any guest code.
//!
//! Use cases:
//!
//! 1. **Architecture validation** — exercises the
//!    initialise / create-isolate / register-callback / call-function /
//!    drain / dispose lifecycle so the trait shape is testable
//!    without any real JS engine being linked.
//! 2. **Topology dry-runs** — `grx run --runtime=null my-system.grx`
//!    parses, builds the IR, and "mounts" the topology. Component
//!    bodies are not executed — `eval_module` records the source and
//!    `call_function` returns `HostValue::Null`.
//! 3. **CI / no-JS-engine environments** — workspaces without the JSC
//!    toolchain still get end-to-end smoke coverage of the parser →
//!    runtime path.

#![allow(clippy::module_name_repetitions)]

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use graphreact_engine_zig::{
    BoxCallback, CallbackId, Engine, EngineError, EngineResult, FunctionHandle, HostValue,
    IsolateHandle, IsolateOptions, ModuleHandle,
};

/// Null engine. Cheap to clone via `Default::default()` — each instance
/// has its own fresh handle counters and table.
pub struct NullEngine {
    next_handle: AtomicU64,
    isolates: Mutex<FxHashMap<u64, IsolateState>>,
    initialized: bool,
}

#[derive(Default)]
struct IsolateState {
    modules: FxHashMap<u64, ModuleState>,
    functions: FxHashMap<u64, FunctionState>,
    callbacks: FxHashMap<u64, BoxCallback>,
    callback_ids_by_name: FxHashMap<String, u64>,
    next_module: u64,
    next_function: u64,
    next_callback: u64,
}

#[derive(Default)]
#[allow(dead_code)]
struct ModuleState {
    name: String,
    source: String,
}

#[derive(Default)]
#[allow(dead_code)]
struct FunctionState {
    module: u64,
    name: String,
}

impl Default for NullEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl NullEngine {
    pub fn new() -> Self {
        Self {
            next_handle: AtomicU64::new(1),
            isolates: Mutex::new(FxHashMap::default()),
            initialized: false,
        }
    }

    fn next_handle(&self) -> u64 {
        self.next_handle.fetch_add(1, Ordering::Relaxed)
    }

    fn with_isolate<F, R>(&self, iso: IsolateHandle, f: F) -> EngineResult<R>
    where
        F: FnOnce(&mut IsolateState) -> EngineResult<R>,
    {
        let mut guard = self.isolates.lock();
        let state = guard
            .get_mut(&iso.0)
            .ok_or(EngineError::InvalidIsolate)?;
        f(state)
    }

    /// Number of currently-live isolates. Useful for stress tests
    /// asserting that disposes balance creates.
    pub fn live_isolates(&self) -> usize {
        self.isolates.lock().len()
    }
}

impl Engine for NullEngine {
    fn initialize(&mut self) -> EngineResult<()> {
        self.initialized = true;
        Ok(())
    }

    fn shutdown(&mut self) -> EngineResult<()> {
        let mut guard = self.isolates.lock();
        guard.clear();
        self.initialized = false;
        Ok(())
    }

    fn create_isolate(&mut self, _opts: IsolateOptions) -> EngineResult<IsolateHandle> {
        if !self.initialized {
            return Err(EngineError::Internal(
                "engine not initialized; call initialize() first".to_string(),
            ));
        }
        let id = self.next_handle();
        self.isolates.lock().insert(id, IsolateState::default());
        Ok(IsolateHandle(id))
    }

    fn dispose_isolate(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        let removed = self.isolates.lock().remove(&iso.0);
        if removed.is_none() {
            return Err(EngineError::InvalidIsolate);
        }
        Ok(())
    }

    fn eval_module(
        &mut self,
        iso: IsolateHandle,
        module_name: &str,
        source: &str,
    ) -> EngineResult<ModuleHandle> {
        let id = self.next_handle();
        self.with_isolate(iso, |state| {
            state.next_module = state.next_module.max(id);
            state.modules.insert(
                id,
                ModuleState {
                    name: module_name.to_string(),
                    source: source.to_string(),
                },
            );
            Ok(ModuleHandle(id))
        })
    }

    fn resolve_function(
        &mut self,
        iso: IsolateHandle,
        module: ModuleHandle,
        name: &str,
    ) -> EngineResult<FunctionHandle> {
        let id = self.next_handle();
        self.with_isolate(iso, |state| {
            if !state.modules.contains_key(&module.0) {
                return Err(EngineError::InvalidModule);
            }
            state.next_function = state.next_function.max(id);
            state.functions.insert(
                id,
                FunctionState {
                    module: module.0,
                    name: name.to_string(),
                },
            );
            Ok(FunctionHandle(id))
        })
    }

    fn register_callback(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        callback: BoxCallback,
    ) -> EngineResult<CallbackId> {
        let id = self.next_handle();
        self.with_isolate(iso, |state| {
            if state.callback_ids_by_name.contains_key(name) {
                return Err(EngineError::CallbackAlreadyRegistered {
                    name: name.to_string(),
                });
            }
            state.next_callback = state.next_callback.max(id);
            state.callback_ids_by_name.insert(name.to_string(), id);
            state.callbacks.insert(id, callback);
            Ok(CallbackId(id))
        })
    }

    fn call_function(
        &mut self,
        iso: IsolateHandle,
        function: FunctionHandle,
        _args: &[HostValue],
    ) -> EngineResult<HostValue> {
        // Null engine does not execute guest code — it just verifies
        // the function handle is alive and returns null.
        self.with_isolate(iso, |state| {
            if !state.functions.contains_key(&function.0) {
                return Err(EngineError::Internal(format!(
                    "function handle {} not found",
                    function.0
                )));
            }
            Ok(HostValue::Null)
        })
    }

    fn drain_microtasks(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        // Null engine has no queue; just validate isolate.
        self.with_isolate(iso, |_| Ok(()))
    }

    fn engine_name(&self) -> &'static str {
        "null"
    }

    fn engine_version(&self) -> String {
        format!("null-{}", env!("CARGO_PKG_VERSION"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> NullEngine {
        let mut e = NullEngine::new();
        e.initialize().expect("init");
        e
    }

    #[test]
    fn create_and_dispose_isolate() {
        let mut e = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        assert!(iso.is_valid());
        assert_eq!(e.live_isolates(), 1);
        e.dispose_isolate(iso).expect("dispose");
        assert_eq!(e.live_isolates(), 0);
    }

    #[test]
    fn dispose_invalid_isolate_errors() {
        let mut e = engine();
        let r = e.dispose_isolate(IsolateHandle(999));
        assert!(matches!(r, Err(EngineError::InvalidIsolate)));
    }

    #[test]
    fn eval_module_and_resolve_function() {
        let mut e = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        let module = e
            .eval_module(iso, "Counter", "function init() {}")
            .expect("eval");
        let f = e.resolve_function(iso, module, "init").expect("resolve");
        let v = e
            .call_function(iso, f, &[HostValue::Number(1.0)])
            .expect("call");
        assert_eq!(v, HostValue::Null);
    }

    #[test]
    fn duplicate_callback_registration_errors() {
        let mut e = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        let cb1: BoxCallback = Box::new(|_args| Ok(HostValue::Null));
        let cb2: BoxCallback = Box::new(|_args| Ok(HostValue::Null));
        e.register_callback(iso, "host_send", cb1).expect("first");
        let r = e.register_callback(iso, "host_send", cb2);
        assert!(matches!(r, Err(EngineError::CallbackAlreadyRegistered { .. })));
    }

    #[test]
    fn call_unknown_function_errors() {
        let mut e = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        let r = e.call_function(iso, FunctionHandle(99), &[]);
        assert!(r.is_err());
    }

    #[test]
    fn create_without_initialize_errors() {
        let mut e = NullEngine::new();
        let r = e.create_isolate(IsolateOptions::default());
        assert!(matches!(r, Err(EngineError::Internal(_))));
    }

    #[test]
    fn shutdown_clears_isolates() {
        let mut e = engine();
        let _ = e.create_isolate(IsolateOptions::default()).expect("iso");
        e.shutdown().expect("shutdown");
        assert_eq!(e.live_isolates(), 0);
    }

    #[test]
    fn engine_name_and_version_are_reported() {
        let e = NullEngine::new();
        assert_eq!(e.engine_name(), "null");
        assert!(e.engine_version().starts_with("null-"));
    }

    #[test]
    fn stress_create_dispose_does_not_leak() {
        let mut e = engine();
        for _ in 0..1000 {
            let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
            e.dispose_isolate(iso).expect("dispose");
        }
        assert_eq!(e.live_isolates(), 0);
    }
}
