/* echo.c — reference implementation of the engine-bridge ABI in C.
 *
 * The C echo engine validates the Rust↔native FFI surface end-to-end
 * before any real engine (V8 / CPython / JSC) is linked. It maintains
 * an in-memory isolate table with module / function / callback maps;
 * `gr_function_call` returns the args-JSON wrapped in a small
 * envelope (so the caller can verify the marshalling round-trip).
 *
 * The Zig sister implementation at `zig/bridge/echo.zig` exports
 * identical symbols; swapping which library is linked produces the
 * same observable behaviour.
 *
 * Concurrency: this implementation is *not* thread-safe — the
 * GraphReact runtime drives engines from a single thread (the
 * scheduler tick). Adding a `pthread_mutex_t` here would be trivial
 * but is deferred until a real concurrent consumer demands it.
 */

#include "graphreact_echo.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* ------- Generic dynamic-array helpers ----------------------------- */

#define INITIAL_CAP 4

typedef struct {
    uint64_t module_handle;
    char *name;
    size_t name_len;
} function_entry;

typedef struct {
    uint64_t handle;
    char *name;
    size_t name_len;
    char *source;
    size_t source_len;
} module_entry;

typedef struct {
    uint64_t id;
    char *name;
    size_t name_len;
    gr_host_callback_fn fn;
    void *data;
} callback_entry;

typedef struct {
    uint64_t handle;

    module_entry *modules;
    size_t modules_len;
    size_t modules_cap;

    function_entry *functions;
    size_t functions_len;
    size_t functions_cap;

    callback_entry *callbacks;
    size_t callbacks_len;
    size_t callbacks_cap;
} isolate_entry;

typedef struct {
    isolate_entry **slots;
    size_t cap;
    size_t live;
} isolate_table;

/* ------- Engine globals ------------------------------------------- */

static int g_initialized = 0;
static uint64_t g_next_handle = 1;
static isolate_table g_isolates = {0};

static const char *ENGINE_NAME = "echo-c";
static const char *ENGINE_VERSION = "echo-c-0.1.0";

static uint64_t alloc_handle(void) {
    return g_next_handle++;
}

static int copy_str(const char *src, size_t src_len, char **out, size_t *out_len) {
    if (src_len == 0) {
        *out = NULL;
        *out_len = 0;
        return GR_OK;
    }
    char *buf = (char*)malloc(src_len);
    if (!buf) return GR_ERR_INTERNAL;
    memcpy(buf, src, src_len);
    *out = buf;
    *out_len = src_len;
    return GR_OK;
}

static int write_to_buffer(
    const char *src, size_t src_len,
    char *out_buf, size_t *out_len
) {
    size_t cap = *out_len;
    *out_len = src_len;
    if (src_len > cap) return GR_ERR_BUFFER_TOO_SMALL;
    if (out_buf && src_len) memcpy(out_buf, src, src_len);
    return GR_OK;
}

/* ------- Isolate table ops ---------------------------------------- */

static int isolate_table_grow(isolate_table *t, size_t needed) {
    if (t->cap >= needed) return GR_OK;
    size_t new_cap = t->cap == 0 ? INITIAL_CAP : t->cap;
    while (new_cap < needed) new_cap *= 2;
    isolate_entry **slots = (isolate_entry**)realloc(t->slots, new_cap * sizeof(*slots));
    if (!slots) return GR_ERR_INTERNAL;
    for (size_t i = t->cap; i < new_cap; i++) slots[i] = NULL;
    t->slots = slots;
    t->cap = new_cap;
    return GR_OK;
}

static isolate_entry *find_isolate(uint64_t handle) {
    if (handle == 0) return NULL;
    for (size_t i = 0; i < g_isolates.cap; i++) {
        if (g_isolates.slots[i] && g_isolates.slots[i]->handle == handle) {
            return g_isolates.slots[i];
        }
    }
    return NULL;
}

static int isolate_grow_modules(isolate_entry *iso) {
    size_t new_cap = iso->modules_cap == 0 ? INITIAL_CAP : iso->modules_cap * 2;
    module_entry *m = (module_entry*)realloc(iso->modules, new_cap * sizeof(*m));
    if (!m) return GR_ERR_INTERNAL;
    iso->modules = m;
    iso->modules_cap = new_cap;
    return GR_OK;
}

static int isolate_grow_functions(isolate_entry *iso) {
    size_t new_cap = iso->functions_cap == 0 ? INITIAL_CAP : iso->functions_cap * 2;
    function_entry *f = (function_entry*)realloc(iso->functions, new_cap * sizeof(*f));
    if (!f) return GR_ERR_INTERNAL;
    iso->functions = f;
    iso->functions_cap = new_cap;
    return GR_OK;
}

static int isolate_grow_callbacks(isolate_entry *iso) {
    size_t new_cap = iso->callbacks_cap == 0 ? INITIAL_CAP : iso->callbacks_cap * 2;
    callback_entry *c = (callback_entry*)realloc(iso->callbacks, new_cap * sizeof(*c));
    if (!c) return GR_ERR_INTERNAL;
    iso->callbacks = c;
    iso->callbacks_cap = new_cap;
    return GR_OK;
}

static module_entry *find_module(isolate_entry *iso, uint64_t handle) {
    for (size_t i = 0; i < iso->modules_len; i++) {
        if (iso->modules[i].handle == handle) return &iso->modules[i];
    }
    return NULL;
}

static int find_callback_index(
    isolate_entry *iso, const char *name, size_t name_len
) {
    for (size_t i = 0; i < iso->callbacks_len; i++) {
        if (iso->callbacks[i].name_len == name_len
            && memcmp(iso->callbacks[i].name, name, name_len) == 0) {
            return (int)i;
        }
    }
    return -1;
}

static void isolate_free(isolate_entry *iso) {
    for (size_t i = 0; i < iso->modules_len; i++) {
        free(iso->modules[i].name);
        free(iso->modules[i].source);
    }
    free(iso->modules);
    for (size_t i = 0; i < iso->functions_len; i++) {
        free(iso->functions[i].name);
    }
    free(iso->functions);
    for (size_t i = 0; i < iso->callbacks_len; i++) {
        free(iso->callbacks[i].name);
    }
    free(iso->callbacks);
    free(iso);
}

/* ------- Public API ------------------------------------------------ */

int32_t gr_engine_initialize(void) {
    g_initialized = 1;
    return GR_OK;
}

int32_t gr_engine_shutdown(void) {
    for (size_t i = 0; i < g_isolates.cap; i++) {
        if (g_isolates.slots[i]) {
            isolate_free(g_isolates.slots[i]);
            g_isolates.slots[i] = NULL;
        }
    }
    free(g_isolates.slots);
    g_isolates.slots = NULL;
    g_isolates.cap = 0;
    g_isolates.live = 0;
    g_initialized = 0;
    return GR_OK;
}

int32_t gr_engine_name(char *out_buf, size_t *out_len) {
    return write_to_buffer(ENGINE_NAME, strlen(ENGINE_NAME), out_buf, out_len);
}

int32_t gr_engine_version(char *out_buf, size_t *out_len) {
    return write_to_buffer(ENGINE_VERSION, strlen(ENGINE_VERSION), out_buf, out_len);
}

uint64_t gr_engine_live_isolates(void) {
    return (uint64_t)g_isolates.live;
}

int32_t gr_isolate_create(uint64_t *out_handle) {
    if (!g_initialized) return GR_ERR_NOT_INITIALIZED;
    if (!out_handle) return GR_ERR_INTERNAL;
    /* Find a free slot (or grow). */
    size_t slot = g_isolates.cap;
    for (size_t i = 0; i < g_isolates.cap; i++) {
        if (!g_isolates.slots[i]) { slot = i; break; }
    }
    if (slot == g_isolates.cap) {
        int rc = isolate_table_grow(&g_isolates, g_isolates.cap + 1);
        if (rc != GR_OK) return rc;
    }
    isolate_entry *iso = (isolate_entry*)calloc(1, sizeof(*iso));
    if (!iso) return GR_ERR_INTERNAL;
    iso->handle = alloc_handle();
    g_isolates.slots[slot] = iso;
    g_isolates.live++;
    *out_handle = iso->handle;
    return GR_OK;
}

int32_t gr_isolate_dispose(uint64_t handle) {
    for (size_t i = 0; i < g_isolates.cap; i++) {
        if (g_isolates.slots[i] && g_isolates.slots[i]->handle == handle) {
            isolate_free(g_isolates.slots[i]);
            g_isolates.slots[i] = NULL;
            g_isolates.live--;
            return GR_OK;
        }
    }
    return GR_ERR_INVALID_ISOLATE;
}

int32_t gr_module_register(
    uint64_t isolate,
    const char *module_name, size_t module_name_len,
    const char *source, size_t source_len,
    uint64_t *out_module
) {
    isolate_entry *iso = find_isolate(isolate);
    if (!iso) return GR_ERR_INVALID_ISOLATE;
    if (iso->modules_len + 1 > iso->modules_cap) {
        int rc = isolate_grow_modules(iso);
        if (rc != GR_OK) return rc;
    }
    module_entry *m = &iso->modules[iso->modules_len++];
    m->handle = alloc_handle();
    if (copy_str(module_name, module_name_len, &m->name, &m->name_len) != GR_OK
        || copy_str(source, source_len, &m->source, &m->source_len) != GR_OK) {
        return GR_ERR_INTERNAL;
    }
    *out_module = m->handle;
    return GR_OK;
}

int32_t gr_function_resolve(
    uint64_t isolate,
    uint64_t module,
    const char *name, size_t name_len,
    uint64_t *out_function
) {
    isolate_entry *iso = find_isolate(isolate);
    if (!iso) return GR_ERR_INVALID_ISOLATE;
    if (!find_module(iso, module)) return GR_ERR_INVALID_MODULE;
    if (iso->functions_len + 1 > iso->functions_cap) {
        int rc = isolate_grow_functions(iso);
        if (rc != GR_OK) return rc;
    }
    function_entry *f = &iso->functions[iso->functions_len++];
    f->module_handle = module;
    if (copy_str(name, name_len, &f->name, &f->name_len) != GR_OK) {
        return GR_ERR_INTERNAL;
    }
    *out_function = alloc_handle();
    /* Echo engine doesn't need a separate handle table for functions —
     * it just bumps the global counter so handles are unique. The
     * Rust side validates by re-resolving on each call when V8 lands. */
    return GR_OK;
}

int32_t gr_callback_register(
    uint64_t isolate,
    const char *name, size_t name_len,
    gr_host_callback_fn cb_fn,
    void *cb_data,
    uint64_t *out_callback_id
) {
    isolate_entry *iso = find_isolate(isolate);
    if (!iso) return GR_ERR_INVALID_ISOLATE;
    if (find_callback_index(iso, name, name_len) >= 0) {
        return GR_ERR_CALLBACK_DUPLICATE;
    }
    if (iso->callbacks_len + 1 > iso->callbacks_cap) {
        int rc = isolate_grow_callbacks(iso);
        if (rc != GR_OK) return rc;
    }
    callback_entry *c = &iso->callbacks[iso->callbacks_len++];
    c->id = alloc_handle();
    c->fn = cb_fn;
    c->data = cb_data;
    if (copy_str(name, name_len, &c->name, &c->name_len) != GR_OK) {
        return GR_ERR_INTERNAL;
    }
    *out_callback_id = c->id;
    return GR_OK;
}

int32_t gr_function_call(
    uint64_t isolate,
    uint64_t function,
    const char *args_json, size_t args_len,
    char *out_result, size_t out_result_cap, size_t *out_result_len
) {
    isolate_entry *iso = find_isolate(isolate);
    if (!iso) return GR_ERR_INVALID_ISOLATE;
    (void)function;
    /* Echo policy: wrap args as { "echoed": <args> }. Lets the Rust
     * test layer prove arguments crossed the boundary intact. */
    static const char *prefix = "{\"echoed\":";
    static const char *suffix = "}";
    size_t prefix_len = strlen(prefix);
    size_t suffix_len = strlen(suffix);
    size_t total = prefix_len + args_len + suffix_len;
    *out_result_len = total;
    if (total > out_result_cap) return GR_ERR_BUFFER_TOO_SMALL;
    memcpy(out_result, prefix, prefix_len);
    if (args_len) memcpy(out_result + prefix_len, args_json, args_len);
    memcpy(out_result + prefix_len + args_len, suffix, suffix_len);
    return GR_OK;
}

int32_t gr_microtasks_drain(uint64_t isolate) {
    return find_isolate(isolate) ? GR_OK : GR_ERR_INVALID_ISOLATE;
}

int32_t gr_callback_invoke(
    uint64_t isolate,
    const char *name, size_t name_len,
    const char *args_json, size_t args_len,
    char *out_result, size_t out_result_cap, size_t *out_result_len
) {
    isolate_entry *iso = find_isolate(isolate);
    if (!iso) return GR_ERR_INVALID_ISOLATE;
    int idx = find_callback_index(iso, name, name_len);
    if (idx < 0) return GR_ERR_CALLBACK_NOT_FOUND;
    callback_entry *c = &iso->callbacks[idx];
    return c->fn(c->data, args_json, args_len, out_result, out_result_cap, out_result_len);
}
