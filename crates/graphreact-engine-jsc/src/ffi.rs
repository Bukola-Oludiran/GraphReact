//! Raw `extern "C"` declarations matching the symbols exported by
//! `zig/bridge/jsc.zig`. Symbol prefix is `gr_jsc_*` (rather than the
//! shared `gr_*` used by the echo bridge) so both engines can link
//! into the same binary without colliding.

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
#![allow(dead_code)]

use std::os::raw::{c_char, c_int, c_void};

pub const GR_OK: i32 = 0;
pub const GR_ERR_NOT_INITIALIZED: i32 = -1;
pub const GR_ERR_INVALID_ISOLATE: i32 = -2;
pub const GR_ERR_INVALID_MODULE: i32 = -3;
pub const GR_ERR_INVALID_FUNCTION: i32 = -4;
pub const GR_ERR_CALLBACK_DUPLICATE: i32 = -5;
pub const GR_ERR_CALLBACK_NOT_FOUND: i32 = -6;
pub const GR_ERR_BUFFER_TOO_SMALL: i32 = -7;
pub const GR_ERR_INTERNAL: i32 = -99;

pub type gr_host_callback_fn = unsafe extern "C" fn(
    cb_data: *mut c_void,
    args_json: *const c_char,
    args_len: usize,
    out_result: *mut c_char,
    out_result_cap: usize,
    out_result_len: *mut usize,
) -> c_int;

extern "C" {
    pub fn gr_jsc_engine_initialize() -> c_int;
    pub fn gr_jsc_engine_shutdown() -> c_int;
    pub fn gr_jsc_engine_name(out_buf: *mut c_char, out_len: *mut usize) -> c_int;
    pub fn gr_jsc_engine_version(out_buf: *mut c_char, out_len: *mut usize) -> c_int;
    pub fn gr_jsc_engine_live_isolates() -> u64;

    pub fn gr_jsc_isolate_create(out_handle: *mut u64) -> c_int;
    pub fn gr_jsc_isolate_dispose(handle: u64) -> c_int;

    pub fn gr_jsc_module_register(
        isolate: u64,
        module_name: *const c_char,
        module_name_len: usize,
        source: *const c_char,
        source_len: usize,
        out_module: *mut u64,
    ) -> c_int;

    /// Register a component body as a re-callable function (NOT
    /// invoked at register time, unlike `gr_jsc_module_register` which
    /// IIFE-wraps and invokes once). Wraps the body as
    /// `(function(ctx){ <body> })`, evaluates to the function value,
    /// stores it in the per-isolate functions table — caller invokes
    /// per render via `gr_jsc_function_call(iso, handle, "[ctx_json]")`.
    pub fn gr_jsc_component_register(
        isolate: u64,
        component_name: *const c_char,
        component_name_len: usize,
        source: *const c_char,
        source_len: usize,
        out_function: *mut u64,
    ) -> c_int;

    pub fn gr_jsc_function_resolve(
        isolate: u64,
        module: u64,
        name: *const c_char,
        name_len: usize,
        out_function: *mut u64,
    ) -> c_int;

    pub fn gr_jsc_callback_register(
        isolate: u64,
        name: *const c_char,
        name_len: usize,
        cb_fn: gr_host_callback_fn,
        cb_data: *mut c_void,
        out_callback_id: *mut u64,
    ) -> c_int;

    pub fn gr_jsc_function_call(
        isolate: u64,
        function: u64,
        args_json: *const c_char,
        args_len: usize,
        out_result: *mut c_char,
        out_result_cap: usize,
        out_result_len: *mut usize,
    ) -> c_int;

    pub fn gr_jsc_microtasks_drain(isolate: u64) -> c_int;

    /// **M16-Backend P9.** Mark the isolate's JSGlobalContext as
    /// remotely inspectable + start the system RemoteInspector
    /// listener. macOS-only; lockdownd discovery surfaces the
    /// JSContext in `Safari → Develop → [Hostname] → JSContext`.
    /// `port` is informational (lockdownd doesn't bind a TCP listener).
    pub fn gr_jsc_isolate_enable_debugger(handle: u64, port: u16) -> c_int;

    /// **M16-Backend P9.** Verification round-trip for the headless
    /// smoke test. Returns 1 if `JSGlobalContextGetRemoteInspectionEnabled`
    /// reads back `true`, 0 if `false`, negative on error.
    pub fn gr_jsc_isolate_inspection_enabled(handle: u64) -> c_int;

    #[allow(dead_code)]
    pub fn gr_jsc_callback_invoke(
        isolate: u64,
        name: *const c_char,
        name_len: usize,
        args_json: *const c_char,
        args_len: usize,
        out_result: *mut c_char,
        out_result_cap: usize,
        out_result_len: *mut usize,
    ) -> c_int;
}
