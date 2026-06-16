//! Raw `extern "C"` types matching the symbols exported by the
//! Go-side c-shared artifact (`go build -buildmode=c-shared`).
//!
//! Symbol prefix is `gr_go_*` (rather than the shared `gr_*` used by
//! the echo bridge) so both engines can link into the same binary
//! without colliding.
//!
//! Unlike `graphreact-engine-jsc::ffi` and `graphreact-engine-cpython::ffi`
//! — which use `extern "C" { ... }` static linking against a Zig
//! bridge built into the binary — this module defines only the
//! **type aliases** for the symbols. The actual function pointers are
//! resolved at runtime via `libloading` (see [`crate::loader`]) because
//! the Go-built `libgrx_go.{so,dylib,dll}` is loaded dynamically per
//! `grx build` output.
//!
//! Status codes + the host-callback trampoline signature are
//! intentionally identical to the JSC + CPython bridges so the
//! callback trampoline body is engine-agnostic.

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
#![allow(dead_code)]

use std::os::raw::{c_char, c_int, c_void};

/// ABI version baked into the Go-side prelude. Bumped any time the
/// FFI wire format changes (e.g. a status code is added, a function
/// signature gains a parameter, BSATN replaces JSON at the
/// boundary). The loader calls [`gr_go_abi_version_fn`] FIRST after
/// `Library::new` and refuses to bind any other symbol on mismatch.
pub const GRX_GO_ABI_VERSION: u32 = 1;

// Status codes — share values with `graphreact-engine-jsc::ffi` and
// `graphreact-engine-cpython::ffi` so `map_status` can be copied
// verbatim across engines.
pub const GR_OK: i32 = 0;
pub const GR_ERR_NOT_INITIALIZED: i32 = -1;
pub const GR_ERR_INVALID_ISOLATE: i32 = -2;
pub const GR_ERR_INVALID_MODULE: i32 = -3;
pub const GR_ERR_INVALID_FUNCTION: i32 = -4;
pub const GR_ERR_CALLBACK_DUPLICATE: i32 = -5;
pub const GR_ERR_CALLBACK_NOT_FOUND: i32 = -6;
pub const GR_ERR_BUFFER_TOO_SMALL: i32 = -7;
pub const GR_ERR_SCRIPT: i32 = -8;
pub const GR_ERR_INTERNAL: i32 = -99;

/// Host-callback trampoline signature. Identical to the JSC + CPython
/// equivalents — the body of `host_callback_trampoline` is
/// engine-agnostic since args/result cross the FFI boundary as JSON.
///
/// - `cb_data`: opaque pointer the engine passes back; we encode the
///   callback id as the pointer value on `register_callback`.
/// - `args_json` / `args_len`: caller-owned JSON-array of args.
/// - `out_result` / `out_result_cap`: caller-owned write buffer.
/// - `out_result_len`: written by the callee; on
///   `GR_ERR_BUFFER_TOO_SMALL` the required size goes here so the
///   caller can grow + retry.
pub type gr_host_callback_fn = unsafe extern "C" fn(
    cb_data: *mut c_void,
    args_json: *const c_char,
    args_len: usize,
    out_result: *mut c_char,
    out_result_cap: usize,
    out_result_len: *mut usize,
) -> c_int;

// ---------------------------------------------------------------------
// Function-pointer type aliases for every Go-side exported symbol.
// `crate::loader::LoadedRuntime` stores one of each, resolved via
// `libloading::Library::get`. The Go side declares the matching
// `//export GrxXxx` functions in its prelude.
// ---------------------------------------------------------------------

pub type gr_go_abi_version_fn = unsafe extern "C" fn() -> u32;

pub type gr_go_engine_initialize_fn = unsafe extern "C" fn() -> c_int;
pub type gr_go_engine_shutdown_fn = unsafe extern "C" fn() -> c_int;
pub type gr_go_engine_live_isolates_fn = unsafe extern "C" fn() -> u64;

pub type gr_go_isolate_create_fn = unsafe extern "C" fn(out_handle: *mut u64) -> c_int;
pub type gr_go_isolate_dispose_fn = unsafe extern "C" fn(handle: u64) -> c_int;

pub type gr_go_module_register_fn = unsafe extern "C" fn(
    isolate: u64,
    module_name: *const c_char,
    module_name_len: usize,
    source: *const c_char,
    source_len: usize,
    out_module: *mut u64,
) -> c_int;

pub type gr_go_function_resolve_fn = unsafe extern "C" fn(
    isolate: u64,
    module: u64,
    name: *const c_char,
    name_len: usize,
    out_function: *mut u64,
) -> c_int;

pub type gr_go_function_call_fn = unsafe extern "C" fn(
    isolate: u64,
    function: u64,
    args_json: *const c_char,
    args_len: usize,
    out_result: *mut c_char,
    out_result_cap: usize,
    out_result_len: *mut usize,
) -> c_int;

pub type gr_go_callback_register_fn = unsafe extern "C" fn(
    isolate: u64,
    name: *const c_char,
    name_len: usize,
    cb_fn: gr_host_callback_fn,
    cb_data: *mut c_void,
    out_callback_id: *mut u64,
) -> c_int;

pub type gr_go_microtasks_drain_fn = unsafe extern "C" fn(isolate: u64) -> c_int;
