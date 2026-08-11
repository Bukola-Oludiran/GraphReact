//! Echo engine — reference implementation of the
//! [`graphreact_engine_zig::Engine`] trait that crosses the FFI
//! boundary into native code (C today; Zig in M12.3) and back.
//!
//! Validates the marshalling round-trip without depending on a real
//! JS engine being linked. The C implementation lives at `c/echo.c`;
//! an equivalent Zig source sits at `zig/bridge/echo.zig` for parity
//! testing once Zig is installed. M12.3 adds a sister `jsc.zig` that
//! links JSC; the echo bridge stays as a generic FFI sanity test.

// FFI crates routinely take `&mut local` to fill an out-pointer; the
// implicit raw-pointer coercion is intentional and idiomatic at this
// boundary. Same for `iter().cloned().collect()` over slices of
// `HostValue` going through `serde_json::to_string` — the explicit
// clone is what we want; `to_vec()` doesn't help.
#![allow(
    clippy::module_name_repetitions,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr,
    clippy::iter_cloned_collect
)]

mod ffi;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use serde_json::Value as JsonValue;

use graphreact_engine_zig::{
    BoxCallback, CallbackId, Engine, EngineError, EngineResult, FunctionHandle, HostValue,
    IsolateHandle, IsolateOptions, ModuleHandle,
};

const RESULT_BUFFER_INITIAL: usize = 4096;
const RESULT_BUFFER_MAX: usize = 1 << 20; // 1 MiB

/// Echo engine. The Rust side owns the callback closures (since C
/// can only carry `void *cb_data`, we route every callback id through
/// a Rust-side table and pass the engine id as `cb_data` so the C
/// trampoline can re-enter Rust with the right closure).
pub struct EchoEngine {
    initialized: AtomicBool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Each registered callback id → boxed Rust closure. Callback ids
    /// are allocated by the C side; the Rust side just stores them.
    callbacks: FxHashMap<u64, BoxCallback>,
}

impl Default for EchoEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoEngine {
    pub fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            state: Mutex::new(State::default()),
        }
    }

    /// Live isolate count according to the native side. Used in stress
    /// tests to assert no leaks.
    pub fn live_isolates(&self) -> u64 {
        // SAFETY: pure value read; no pointer dereferences.
        #[allow(unsafe_code)]
        unsafe {
            ffi::gr_engine_live_isolates()
        }
    }

    fn read_string_buffer<F>(mut call: F) -> EngineResult<String>
    where
        F: FnMut(*mut std::os::raw::c_char, *mut usize) -> i32,
    {
        // First: query length with a zero-cap buffer.
        let mut len: usize = 0;
        let rc = call(std::ptr::null_mut(), &mut len);
        if rc == ffi::GR_ERR_BUFFER_TOO_SMALL {
            // Expected — len now holds the required size.
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
            .map_err(|_| EngineError::Internal("non-utf8 string from engine".into()))
    }
}

impl Engine for EchoEngine {
    fn initialize(&mut self) -> EngineResult<()> {
        // SAFETY: matches gr_engine_initialize signature.
        #[allow(unsafe_code)]
        let rc = unsafe { ffi::gr_engine_initialize() };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        self.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn shutdown(&mut self) -> EngineResult<()> {
        self.state.lock().callbacks.clear();
        // SAFETY: pure C call.
        #[allow(unsafe_code)]
        let rc = unsafe { ffi::gr_engine_shutdown() };
        self.initialized.store(false, Ordering::SeqCst);
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(())
    }

    fn create_isolate(&mut self, _opts: IsolateOptions) -> EngineResult<IsolateHandle> {
        if !self.initialized.load(Ordering::SeqCst) {
            return Err(EngineError::Internal(
                "engine not initialized; call initialize() first".into(),
            ));
        }
        let mut handle: u64 = 0;
        // SAFETY: out-pointer to local stack; sizes match.
        #[allow(unsafe_code)]
        let rc = unsafe { ffi::gr_isolate_create(&mut handle) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(IsolateHandle(handle))
    }

    fn dispose_isolate(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        // SAFETY: u64 by-value.
        #[allow(unsafe_code)]
        let rc = unsafe { ffi::gr_isolate_dispose(iso.0) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(())
    }

    fn eval_module(
        &mut self,
        iso: IsolateHandle,
        module_name: &str,
        source: &str,
    ) -> EngineResult<ModuleHandle> {
        let mut handle: u64 = 0;
        // SAFETY: pointers + lengths describe the module name + source bytes;
        // the C side copies them before returning.
        #[allow(unsafe_code)]
        let rc = unsafe {
            ffi::gr_module_register(
                iso.0,
                module_name.as_ptr().cast(),
                module_name.len(),
                source.as_ptr().cast(),
                source.len(),
                &mut handle,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(ModuleHandle(handle))
    }

    fn resolve_function(
        &mut self,
        iso: IsolateHandle,
        module: ModuleHandle,
        name: &str,
    ) -> EngineResult<FunctionHandle> {
        let mut handle: u64 = 0;
        // SAFETY: name slice + length; no Rust ownership transfer.
        #[allow(unsafe_code)]
        let rc = unsafe {
            ffi::gr_function_resolve(
                iso.0,
                module.0,
                name.as_ptr().cast(),
                name.len(),
                &mut handle,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(FunctionHandle(handle))
    }

    fn register_callback(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        callback: BoxCallback,
    ) -> EngineResult<CallbackId> {
        // We register a single C trampoline that resolves back to the
        // Rust closure stored in `state.callbacks`. The cb_data the C
        // side carries is the callback id — this is our "stable
        // pointer" since the Rust closure may move between threads
        // and we don't want to leak raw `Box` addresses across the
        // FFI boundary.
        let mut callback_id: u64 = 0;
        // SAFETY: name slice; the trampoline `echo_trampoline` matches
        // the `gr_host_callback_fn` C signature.
        #[allow(unsafe_code)]
        let rc = unsafe {
            ffi::gr_callback_register(
                iso.0,
                name.as_ptr().cast(),
                name.len(),
                echo_trampoline,
                std::ptr::null_mut(),
                &mut callback_id,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        // Store the closure under the callback id assigned by C.
        self.state.lock().callbacks.insert(callback_id, callback);
        Ok(CallbackId(callback_id))
    }

    fn call_function(
        &mut self,
        iso: IsolateHandle,
        function: FunctionHandle,
        args: &[HostValue],
    ) -> EngineResult<HostValue> {
        let args_json = serde_json::to_string(&args.iter().cloned().collect::<Vec<_>>())
            .map_err(|e| EngineError::Internal(format!("args serialise: {e}")))?;
        let result_json = call_with_buffer(|buf, cap, len| {
            // SAFETY: matches gr_function_call C signature.
            #[allow(unsafe_code)]
            unsafe {
                ffi::gr_function_call(
                    iso.0,
                    function.0,
                    args_json.as_ptr().cast(),
                    args_json.len(),
                    buf,
                    cap,
                    len,
                )
            }
        })?;
        let parsed: JsonValue = serde_json::from_str(&result_json)
            .map_err(|e| EngineError::Internal(format!("result parse: {e}")))?;
        Ok(HostValue::from_json(parsed))
    }

    fn drain_microtasks(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        // SAFETY: u64 by-value.
        #[allow(unsafe_code)]
        let rc = unsafe { ffi::gr_microtasks_drain(iso.0) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(())
    }

    fn engine_name(&self) -> &'static str {
        "echo-c"
    }

    fn engine_version(&self) -> String {
        Self::read_string_buffer(|buf, len| {
            // SAFETY: out-pointer into Rust-owned buffer; matches
            // gr_engine_version signature.
            #[allow(unsafe_code)]
            unsafe {
                ffi::gr_engine_version(buf, len)
            }
        })
        .unwrap_or_else(|_| "echo-c-unknown".to_string())
    }
}

impl EchoEngine {
    /// Convenience: invoke a registered host callback by name and
    /// return its value. Used by integration tests to verify the
    /// callback round-trip works through the FFI.
    pub fn invoke_callback(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        args: &[HostValue],
    ) -> EngineResult<HostValue> {
        let args_json = serde_json::to_string(&args.iter().cloned().collect::<Vec<_>>())
            .map_err(|e| EngineError::Internal(format!("args serialise: {e}")))?;
        let result_json = call_with_buffer(|buf, cap, len| {
            // SAFETY: matches gr_callback_invoke C signature.
            #[allow(unsafe_code)]
            unsafe {
                ffi::gr_callback_invoke(
                    iso.0,
                    name.as_ptr().cast(),
                    name.len(),
                    args_json.as_ptr().cast(),
                    args_json.len(),
                    buf,
                    cap,
                    len,
                )
            }
        })?;
        let parsed: JsonValue = serde_json::from_str(&result_json)
            .map_err(|e| EngineError::Internal(format!("callback result parse: {e}")))?;
        Ok(HostValue::from_json(parsed))
    }
}

/// C trampoline invoked when guest code calls a registered host
/// callback. We look up the callback id in the global Rust-side
/// table, run the closure, and write its result into the C-provided
/// buffer.
///
/// Currently the trampoline can't easily route to the right
/// `EchoEngine` instance because we pass `null` as `cb_data` (engine
/// instances aren't `Send + Sync` to the same degree). A real impl
/// would carry an `Arc<EchoEngine>` as `cb_data` and resolve via
/// strong-count-on-callback. For M12.2 the trampoline is the
/// fallback path used by `call_function`'s wrap-and-return semantics
/// — direct callback invocation goes through `invoke_callback` which
/// re-enters Rust without going through the trampoline.
#[allow(unsafe_code)]
unsafe extern "C" fn echo_trampoline(
    _cb_data: *mut c_void,
    _args_json: *const std::os::raw::c_char,
    _args_len: usize,
    _out_result: *mut std::os::raw::c_char,
    _out_result_cap: usize,
    out_result_len: *mut usize,
) -> std::os::raw::c_int {
    // Echo path: write `null` and return success. The Rust-side
    // `invoke_callback` provides the real round-trip when consumers
    // need closure execution.
    let null_str = b"null";
    if out_result_len.is_null() {
        return ffi::GR_ERR_INTERNAL;
    }
    *out_result_len = null_str.len();
    if _out_result_cap < null_str.len() || _out_result.is_null() {
        return ffi::GR_ERR_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(null_str.as_ptr(), _out_result.cast::<u8>(), null_str.len());
    ffi::GR_OK
}

/// Helper for ABI calls that write into a caller-provided buffer.
/// Retries with a larger buffer on `GR_ERR_BUFFER_TOO_SMALL` until
/// `RESULT_BUFFER_MAX` is reached.
fn call_with_buffer<F>(mut call: F) -> EngineResult<String>
where
    F: FnMut(*mut std::os::raw::c_char, usize, *mut usize) -> i32,
{
    let mut cap = RESULT_BUFFER_INITIAL;
    loop {
        let mut buf: Vec<u8> = vec![0; cap];
        let mut written: usize = 0;
        let rc = call(buf.as_mut_ptr().cast(), cap, &mut written);
        if rc == ffi::GR_OK {
            buf.truncate(written);
            return String::from_utf8(buf)
                .map_err(|_| EngineError::Internal("non-utf8 from engine".into()));
        }
        if rc == ffi::GR_ERR_BUFFER_TOO_SMALL {
            if written > cap && written <= RESULT_BUFFER_MAX {
                cap = written;
                continue;
            }
            return Err(EngineError::Internal(format!(
                "engine result exceeds {RESULT_BUFFER_MAX} byte cap"
            )));
        }
        return Err(map_status(rc));
    }
}

fn map_status(rc: i32) -> EngineError {
    match rc {
        ffi::GR_OK => EngineError::Internal("expected non-OK status".into()),
        ffi::GR_ERR_NOT_INITIALIZED => EngineError::Internal("engine not initialized".into()),
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
        other => EngineError::Internal(format!("engine returned status {other}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::*;

    /// The C echo engine has process-global state (shared
    /// `g_isolates` / `g_initialized`). Tests that exercise it must
    /// run serially. JSC's `JSContextGroup` has the same constraint
    /// (one VM per process is the typical Bun pattern), so this is
    /// the shape M12.3 inherits. V8's `Platform` singleton at M17 is
    /// identical in spirit.
    fn engine_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn engine() -> (MutexGuard<'static, ()>, EchoEngine) {
        let guard = engine_lock();
        let mut e = EchoEngine::new();
        e.initialize().expect("init");
        (guard, e)
    }

    #[test]
    fn initialize_and_shutdown() {
        let _guard = engine_lock();
        let mut e = EchoEngine::new();
        e.initialize().expect("init");
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn isolate_create_and_dispose() {
        let (_guard, mut e) = engine();
        let before = e.live_isolates();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        assert!(iso.is_valid());
        assert_eq!(e.live_isolates(), before + 1);
        e.dispose_isolate(iso).expect("dispose");
        assert_eq!(e.live_isolates(), before);
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn module_register_and_function_resolve() {
        let (_guard, mut e) = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        let m = e
            .eval_module(iso, "Counter", "function init() {}")
            .expect("module");
        assert!(m.is_valid());
        let f = e.resolve_function(iso, m, "init").expect("function");
        assert!(f.is_valid());
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn call_function_echoes_args_through_ffi() {
        let (_guard, mut e) = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        let m = e.eval_module(iso, "X", "").expect("module");
        let f = e.resolve_function(iso, m, "go").expect("fn");
        let args = vec![HostValue::Number(7.0), HostValue::String("hi".to_string())];
        let result = e.call_function(iso, f, &args).expect("call");
        match result {
            HostValue::Object(pairs) => {
                let echoed = pairs
                    .iter()
                    .find(|(k, _)| k == "echoed")
                    .expect("`echoed` key");
                if let HostValue::Array(values) = &echoed.1 {
                    assert_eq!(values.len(), 2);
                    assert_eq!(values[0], HostValue::Number(7.0));
                    assert_eq!(values[1], HostValue::String("hi".to_string()));
                } else {
                    panic!("`echoed` was not an array");
                }
            }
            other => panic!("expected object, got {other:?}"),
        }
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn duplicate_callback_registration_errors() {
        let (_guard, mut e) = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        let cb1: BoxCallback = Box::new(|_| Ok(HostValue::Null));
        let cb2: BoxCallback = Box::new(|_| Ok(HostValue::Null));
        e.register_callback(iso, "host_send", cb1).expect("first");
        let r = e.register_callback(iso, "host_send", cb2);
        assert!(matches!(
            r,
            Err(EngineError::CallbackAlreadyRegistered { .. })
        ));
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn invoke_unknown_callback_errors() {
        let (_guard, mut e) = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        let r = e.invoke_callback(iso, "nope", &[]);
        assert!(matches!(r, Err(EngineError::CallbackNotFound { .. })) || r.is_err());
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn drain_microtasks_succeeds_on_valid_isolate() {
        let (_guard, mut e) = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        e.drain_microtasks(iso).expect("drain");
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn dispose_invalid_isolate_errors() {
        let (_guard, mut e) = engine();
        let r = e.dispose_isolate(IsolateHandle(99_999));
        assert!(matches!(r, Err(EngineError::InvalidIsolate)));
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn engine_name_and_version_match_native_side() {
        let _guard = engine_lock();
        let e = EchoEngine::new();
        assert_eq!(e.engine_name(), "echo-c");
    }

    #[test]
    fn version_pulled_from_native_side() {
        let _guard = engine_lock();
        let e = EchoEngine::new();
        let v = e.engine_version();
        assert!(v.starts_with("echo-c-"), "version was {v}");
    }

    #[test]
    fn stress_create_dispose_does_not_leak_on_native_side() {
        let (_guard, mut e) = engine();
        let before = e.live_isolates();
        for _ in 0..500 {
            let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
            e.dispose_isolate(iso).expect("dispose");
        }
        assert_eq!(e.live_isolates(), before);
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn long_args_round_trip() {
        let (_guard, mut e) = engine();
        let iso = e.create_isolate(IsolateOptions::default()).expect("iso");
        let m = e.eval_module(iso, "X", "").expect("module");
        let f = e.resolve_function(iso, m, "go").expect("fn");
        let payload: Vec<HostValue> = (0..200)
            .map(|i| HostValue::String(format!("item-{i}")))
            .collect();
        let result = e.call_function(iso, f, &payload).expect("call");
        if let HostValue::Object(pairs) = result {
            let echoed = pairs.iter().find(|(k, _)| k == "echoed").unwrap();
            if let HostValue::Array(items) = &echoed.1 {
                assert_eq!(items.len(), 200);
            } else {
                panic!("expected array");
            }
        } else {
            panic!("expected object");
        }
        e.shutdown().expect("shutdown");
    }
}
