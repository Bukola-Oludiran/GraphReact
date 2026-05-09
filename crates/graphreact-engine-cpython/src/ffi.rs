//! Raw `extern "C"` declarations matching the symbols exported by
//! `zig/bridge/cpython.zig`. Symbol prefix is `gr_cpython_*` so all
//! three engine bridges (echo `gr_*`, jsc `gr_jsc_*`, cpython
//! `gr_cpython_*`) can link into the same binary without colliding.
//!
//! **M13-phase2.5 surface — all real.** Lifecycle
//! (`engine_initialize` / `shutdown` / `name` / `version` /
//! `live_isolates`) + isolate create/dispose + `module_register` /
//! `function_resolve` / `function_call` / `microtasks_drain` (no-op
//! — CPython has no embedder-level microtask queue) +
//! `callback_register` (wraps a Rust closure as a `PyCFunction` whose
//! `m_self` is a `PyCapsule` carrying the per-callback identity;
//! installs on the active sub-interpreter's builtins) +
//! `callback_invoke` (looks up a Python callable on the active
//! sub-interpreter's builtins by name + calls it via `PyObject_Call`).

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
    pub fn gr_cpython_engine_initialize() -> c_int;
    pub fn gr_cpython_engine_shutdown() -> c_int;
    pub fn gr_cpython_engine_name(out_buf: *mut c_char, out_len: *mut usize) -> c_int;
    pub fn gr_cpython_engine_version(out_buf: *mut c_char, out_len: *mut usize) -> c_int;
    pub fn gr_cpython_engine_live_isolates() -> u64;

    pub fn gr_cpython_isolate_create(out_handle: *mut u64) -> c_int;
    pub fn gr_cpython_isolate_dispose(handle: u64) -> c_int;

    /// **M13-phase2 — REAL.** Evaluates `source` via `PyRun_String`
    /// against a fresh module dict; the resulting module object is
    /// stored under the returned handle.
    pub fn gr_cpython_module_register(
        isolate: u64,
        module_name: *const c_char,
        module_name_len: usize,
        source: *const c_char,
        source_len: usize,
        out_module: *mut u64,
    ) -> c_int;

    /// **M13-phase2 — REAL.** `PyObject_GetAttrString` against the
    /// stored module object + `PyCallable_Check`.
    pub fn gr_cpython_function_resolve(
        isolate: u64,
        module: u64,
        name: *const c_char,
        name_len: usize,
        out_function: *mut u64,
    ) -> c_int;

    /// **M13-phase2.5 — REAL.** Allocates a per-callback heap entry,
    /// boxes its identity into a `PyCapsule`, wraps that capsule in a
    /// `PyCFunction` (using a process-shared `PyMethodDef` whose
    /// `ml_meth` is a Zig trampoline that re-enters Rust via `cb_fn`),
    /// and installs the callable on the active sub-interpreter's
    /// builtins so any module can call `<name>(...)` without import.
    /// The capsule destructor frees the entry deterministically when
    /// the sub-interpreter is torn down. `cb_data` is reserved for
    /// future engine-identity routing and intentionally unused — the
    /// Zig side stores the registry id in the entry's `host_data` slot
    /// and passes it back to the Rust trampoline as `cb_data`.
    pub fn gr_cpython_callback_register(
        isolate: u64,
        name: *const c_char,
        name_len: usize,
        cb_fn: gr_host_callback_fn,
        cb_data: *mut c_void,
        out_callback_id: *mut u64,
    ) -> c_int;

    /// **M13-phase2 — REAL.** `PyObject_Call` with JSON marshalling
    /// (stdlib `json.loads` on args, `json.dumps` on return).
    pub fn gr_cpython_function_call(
        isolate: u64,
        function: u64,
        args_json: *const c_char,
        args_len: usize,
        out_result: *mut c_char,
        out_result_cap: usize,
        out_result_len: *mut usize,
    ) -> c_int;

    /// CPython doesn't have an event loop / microtask queue analogue
    /// at the embedder level; asyncio integration lands at a later
    /// phase (likely a sibling of M12.3b-phase1 — JS Promise → Rust
    /// Future bridge). Phase 1 returns `GR_OK` no-op so the trait
    /// surface can be exercised cleanly.
    pub fn gr_cpython_microtasks_drain(isolate: u64) -> c_int;

    /// **M13-phase2.5 — REAL.** Symmetric peer of `callback_register`:
    /// looks up a Python callable on the active sub-interpreter's
    /// builtins by name (`PyEval_GetBuiltins` + `PyDict_GetItemString`)
    /// and calls it via `PyObject_Call` with JSON-marshalled args
    /// (stdlib `json.loads` on args, `json.dumps` on return).
    pub fn gr_cpython_callback_invoke(
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
