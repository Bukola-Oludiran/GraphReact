/* graphreact_echo.h — C ABI for the engine-bridge surface.
 *
 * This is the contract between the Rust runtime and any concrete
 * engine implementation (C echo / Zig echo / Zig V8 / Zig CPython).
 * Both `c/echo.c` (this crate's reference implementation) and
 * `zig/bridge/echo.zig` (the eventual Zig implementation) export
 * matching symbols.
 *
 * Conventions:
 *   - Handles are `uint64_t`. `0` means "invalid".
 *   - All functions return an `int32_t` status code (`GR_OK` on
 *     success, negative on error). Out-parameters are written via
 *     pointer.
 *   - Strings cross the FFI as `(const char*, size_t)` pairs (NOT
 *     null-terminated by contract; native side must respect length).
 *   - Returned strings are written to caller-provided buffers; the
 *     callee never allocates Rust-visible memory.
 */

#ifndef GRAPHREACT_ECHO_H
#define GRAPHREACT_ECHO_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Status codes returned by every API. */
#define GR_OK                       0
#define GR_ERR_NOT_INITIALIZED     -1
#define GR_ERR_INVALID_ISOLATE     -2
#define GR_ERR_INVALID_MODULE      -3
#define GR_ERR_INVALID_FUNCTION    -4
#define GR_ERR_CALLBACK_DUPLICATE  -5
#define GR_ERR_CALLBACK_NOT_FOUND  -6
#define GR_ERR_BUFFER_TOO_SMALL    -7
#define GR_ERR_INTERNAL            -99

/* Engine-global lifecycle. */
int32_t gr_engine_initialize(void);
int32_t gr_engine_shutdown(void);

/* Returns the engine name (e.g. "echo-c" / "echo-zig" / "v8") into
 * `out_buf`. `*out_len` is in/out: callers pass the buffer capacity,
 * the callee writes the actual length. */
int32_t gr_engine_name(char *out_buf, size_t *out_len);

/* Returns the engine version into `out_buf`. Same in/out semantics. */
int32_t gr_engine_version(char *out_buf, size_t *out_len);

/* Isolate lifecycle. `*out_handle` receives the new isolate's id. */
int32_t gr_isolate_create(uint64_t *out_handle);
int32_t gr_isolate_dispose(uint64_t handle);

/* Number of currently-live isolates. Used for leak tests. */
uint64_t gr_engine_live_isolates(void);

/* Compile + register a module on an isolate. `module_name` and
 * `source` are length-counted (NOT null-terminated). On success
 * `*out_module` receives the module's handle. (Maps to the
 * Rust trait's `eval_module` method.) */
int32_t gr_module_register(
    uint64_t isolate,
    const char *module_name, size_t module_name_len,
    const char *source, size_t source_len,
    uint64_t *out_module
);

/* Resolve a function by name from a module. */
int32_t gr_function_resolve(
    uint64_t isolate,
    uint64_t module,
    const char *name, size_t name_len,
    uint64_t *out_function
);

/* Register a host callback by name. `*out_callback_id` receives the
 * callback's id; the bridge stores `cb_data` and passes it to
 * `cb_fn` on each invocation.
 *
 * Callback ABI:
 *   `cb_fn(cb_data, args_json, args_len, out_result, out_result_cap, out_result_len) -> status`.
 * `args_json` is the marshalled arguments as a JSON array (UTF-8,
 * not null-terminated). The callback writes its return value as
 * JSON into `out_result` (capacity `out_result_cap`, actual length
 * written to `*out_result_len`). */
typedef int32_t (*gr_host_callback_fn)(
    void *cb_data,
    const char *args_json, size_t args_len,
    char *out_result, size_t out_result_cap, size_t *out_result_len
);

int32_t gr_callback_register(
    uint64_t isolate,
    const char *name, size_t name_len,
    gr_host_callback_fn cb_fn,
    void *cb_data,
    uint64_t *out_callback_id
);

/* Invoke a function with args (JSON array). The result (JSON value)
 * is written to `out_result` like the callback path. */
int32_t gr_function_call(
    uint64_t isolate,
    uint64_t function,
    const char *args_json, size_t args_len,
    char *out_result, size_t out_result_cap, size_t *out_result_len
);

/* Drain pending microtasks. Echo bridge has no queue → always GR_OK. */
int32_t gr_microtasks_drain(uint64_t isolate);

/* Convenience: invoke a registered callback by name from Rust. The
 * echo engine routes this through the same callback table that
 * `gr_callback_register` populated, validating the round-trip. */
int32_t gr_callback_invoke(
    uint64_t isolate,
    const char *name, size_t name_len,
    const char *args_json, size_t args_len,
    char *out_result, size_t out_result_cap, size_t *out_result_len
);

#ifdef __cplusplus
}
#endif

#endif /* GRAPHREACT_ECHO_H */
