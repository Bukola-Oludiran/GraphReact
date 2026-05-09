//! Raw `extern "C"` declarations that match `c/graphreact_echo.h`.
//!
//! This module is the only place in the workspace where `unsafe` lives
//! for the bridge surface. Everything else handles values via the
//! safe wrappers in [`super::lib`].

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]

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
    pub fn gr_engine_initialize() -> c_int;
    pub fn gr_engine_shutdown() -> c_int;
    /// Reserved — Rust returns the name from a hardcoded `&'static str`
    /// for now to avoid an extra FFI hop. Kept on the C surface so the
    /// Zig parity at `zig/bridge/echo.zig` exports the same symbol set.
    #[allow(dead_code)]
    pub fn gr_engine_name(out_buf: *mut c_char, out_len: *mut usize) -> c_int;
    pub fn gr_engine_version(out_buf: *mut c_char, out_len: *mut usize) -> c_int;
    pub fn gr_engine_live_isolates() -> u64;

    pub fn gr_isolate_create(out_handle: *mut u64) -> c_int;
    pub fn gr_isolate_dispose(handle: u64) -> c_int;

    pub fn gr_module_register(
        isolate: u64,
        module_name: *const c_char,
        module_name_len: usize,
        source: *const c_char,
        source_len: usize,
        out_module: *mut u64,
    ) -> c_int;

    pub fn gr_function_resolve(
        isolate: u64,
        module: u64,
        name: *const c_char,
        name_len: usize,
        out_function: *mut u64,
    ) -> c_int;

    pub fn gr_callback_register(
        isolate: u64,
        name: *const c_char,
        name_len: usize,
        cb_fn: gr_host_callback_fn,
        cb_data: *mut c_void,
        out_callback_id: *mut u64,
    ) -> c_int;

    pub fn gr_function_call(
        isolate: u64,
        function: u64,
        args_json: *const c_char,
        args_len: usize,
        out_result: *mut c_char,
        out_result_cap: usize,
        out_result_len: *mut usize,
    ) -> c_int;

    pub fn gr_microtasks_drain(isolate: u64) -> c_int;

    pub fn gr_callback_invoke(
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
