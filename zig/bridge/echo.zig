// echo.zig — Zig parity implementation of the engine-bridge ABI
// declared in `crates/graphreact-engine-echo/c/graphreact_echo.h`.
//
// This file is the M12.3 path: when Zig is installed and the V8
// embedding work begins, this Zig source replaces the C reference at
// `crates/graphreact-engine-echo/c/echo.c`. The exported symbol set
// matches byte-for-byte so the Rust crate's `extern "C"` block stays
// unchanged.
//
// Build: see `zig/bridge/build.zig`. Driven by `cargo xtask zig-build`
// which detects whether `zig` is on `$PATH` and shells out, otherwise
// prints install instructions.
//
// Status: source code committed; build wiring in place; not actually
// compiled in CI yet because Zig isn't a workspace test-time
// dependency. M12.3 lifts that.

const std = @import("std");

const GR_OK: i32 = 0;
const GR_ERR_NOT_INITIALIZED: i32 = -1;
const GR_ERR_INVALID_ISOLATE: i32 = -2;
const GR_ERR_INVALID_MODULE: i32 = -3;
const GR_ERR_INVALID_FUNCTION: i32 = -4;
const GR_ERR_CALLBACK_DUPLICATE: i32 = -5;
const GR_ERR_CALLBACK_NOT_FOUND: i32 = -6;
const GR_ERR_BUFFER_TOO_SMALL: i32 = -7;
const GR_ERR_INTERNAL: i32 = -99;

const HostCallbackFn = *const fn (
    cb_data: ?*anyopaque,
    args_json: [*c]const u8,
    args_len: usize,
    out_result: [*c]u8,
    out_result_cap: usize,
    out_result_len: *usize,
) callconv(.C) i32;

const Module = struct {
    handle: u64,
    name: []u8,
    source: []u8,
};

const Function = struct {
    module_handle: u64,
    name: []u8,
};

const Callback = struct {
    id: u64,
    name: []u8,
    fn_ptr: HostCallbackFn,
    data: ?*anyopaque,
};

const Isolate = struct {
    handle: u64,
    modules: std.array_list.Managed(Module),
    functions: std.array_list.Managed(Function),
    callbacks: std.array_list.Managed(Callback),

    fn deinit(self: *Isolate, allocator: std.mem.Allocator) void {
        for (self.modules.items) |m| {
            allocator.free(m.name);
            allocator.free(m.source);
        }
        self.modules.deinit();
        for (self.functions.items) |f| {
            allocator.free(f.name);
        }
        self.functions.deinit();
        for (self.callbacks.items) |c| {
            allocator.free(c.name);
        }
        self.callbacks.deinit();
    }
};

var gpa: std.heap.GeneralPurposeAllocator(.{}) = .{};
var allocator: std.mem.Allocator = undefined;
var initialized: bool = false;
var next_handle: u64 = 1;
var isolates: std.array_list.Managed(?*Isolate) = undefined;
var live_count: u64 = 0;

const ENGINE_NAME = "echo-zig";
const ENGINE_VERSION = "echo-zig-0.1.0";

fn ensureAllocator() void {
    if (!initialized) return;
}

fn allocHandle() u64 {
    const h = next_handle;
    next_handle += 1;
    return h;
}

fn copyString(src: [*c]const u8, src_len: usize) ![]u8 {
    if (src_len == 0) return &[_]u8{};
    const buf = try allocator.alloc(u8, src_len);
    @memcpy(buf, src[0..src_len]);
    return buf;
}

fn writeBuffer(src: []const u8, out_buf: [*c]u8, out_len: *usize) i32 {
    const cap = out_len.*;
    out_len.* = src.len;
    if (src.len > cap) return GR_ERR_BUFFER_TOO_SMALL;
    if (src.len > 0) @memcpy(out_buf[0..src.len], src);
    return GR_OK;
}

fn findIsolate(handle: u64) ?*Isolate {
    if (handle == 0) return null;
    for (isolates.items) |maybe| {
        if (maybe) |iso| {
            if (iso.handle == handle) return iso;
        }
    }
    return null;
}

fn findCallback(iso: *Isolate, name: []const u8) ?usize {
    for (iso.callbacks.items, 0..) |c, i| {
        if (std.mem.eql(u8, c.name, name)) return i;
    }
    return null;
}

export fn gr_engine_initialize() i32 {
    allocator = gpa.allocator();
    isolates = std.array_list.Managed(?*Isolate).init(allocator);
    initialized = true;
    return GR_OK;
}

export fn gr_engine_shutdown() i32 {
    for (isolates.items) |maybe| {
        if (maybe) |iso| {
            iso.deinit(allocator);
            allocator.destroy(iso);
        }
    }
    isolates.deinit();
    initialized = false;
    live_count = 0;
    _ = gpa.deinit();
    return GR_OK;
}

export fn gr_engine_name(out_buf: [*c]u8, out_len: *usize) i32 {
    return writeBuffer(ENGINE_NAME, out_buf, out_len);
}

export fn gr_engine_version(out_buf: [*c]u8, out_len: *usize) i32 {
    return writeBuffer(ENGINE_VERSION, out_buf, out_len);
}

export fn gr_engine_live_isolates() u64 {
    return live_count;
}

export fn gr_isolate_create(out_handle: *u64) i32 {
    if (!initialized) return GR_ERR_NOT_INITIALIZED;
    const iso = allocator.create(Isolate) catch return GR_ERR_INTERNAL;
    iso.* = .{
        .handle = allocHandle(),
        .modules = std.array_list.Managed(Module).init(allocator),
        .functions = std.array_list.Managed(Function).init(allocator),
        .callbacks = std.array_list.Managed(Callback).init(allocator),
    };
    isolates.append(iso) catch {
        allocator.destroy(iso);
        return GR_ERR_INTERNAL;
    };
    live_count += 1;
    out_handle.* = iso.handle;
    return GR_OK;
}

export fn gr_isolate_dispose(handle: u64) i32 {
    for (isolates.items, 0..) |maybe, idx| {
        if (maybe) |iso| {
            if (iso.handle == handle) {
                iso.deinit(allocator);
                allocator.destroy(iso);
                isolates.items[idx] = null;
                live_count -= 1;
                return GR_OK;
            }
        }
    }
    return GR_ERR_INVALID_ISOLATE;
}

export fn gr_module_register(
    isolate: u64,
    module_name: [*c]const u8,
    module_name_len: usize,
    source: [*c]const u8,
    source_len: usize,
    out_module: *u64,
) i32 {
    const iso = findIsolate(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const name_buf = copyString(module_name, module_name_len) catch return GR_ERR_INTERNAL;
    const src_buf = copyString(source, source_len) catch {
        allocator.free(name_buf);
        return GR_ERR_INTERNAL;
    };
    const handle = allocHandle();
    iso.modules.append(.{ .handle = handle, .name = name_buf, .source = src_buf }) catch {
        allocator.free(name_buf);
        allocator.free(src_buf);
        return GR_ERR_INTERNAL;
    };
    out_module.* = handle;
    return GR_OK;
}

export fn gr_function_resolve(
    isolate: u64,
    module: u64,
    name: [*c]const u8,
    name_len: usize,
    out_function: *u64,
) i32 {
    const iso = findIsolate(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    var found = false;
    for (iso.modules.items) |m| {
        if (m.handle == module) {
            found = true;
            break;
        }
    }
    if (!found) return GR_ERR_INVALID_MODULE;
    const name_buf = copyString(name, name_len) catch return GR_ERR_INTERNAL;
    iso.functions.append(.{ .module_handle = module, .name = name_buf }) catch {
        allocator.free(name_buf);
        return GR_ERR_INTERNAL;
    };
    out_function.* = allocHandle();
    return GR_OK;
}

export fn gr_callback_register(
    isolate: u64,
    name: [*c]const u8,
    name_len: usize,
    cb_fn: HostCallbackFn,
    cb_data: ?*anyopaque,
    out_callback_id: *u64,
) i32 {
    const iso = findIsolate(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const name_slice = name[0..name_len];
    if (findCallback(iso, name_slice) != null) return GR_ERR_CALLBACK_DUPLICATE;
    const name_buf = copyString(name, name_len) catch return GR_ERR_INTERNAL;
    const id = allocHandle();
    iso.callbacks.append(.{
        .id = id,
        .name = name_buf,
        .fn_ptr = cb_fn,
        .data = cb_data,
    }) catch {
        allocator.free(name_buf);
        return GR_ERR_INTERNAL;
    };
    out_callback_id.* = id;
    return GR_OK;
}

export fn gr_function_call(
    isolate: u64,
    function: u64,
    args_json: [*c]const u8,
    args_len: usize,
    out_result: [*c]u8,
    out_result_cap: usize,
    out_result_len: *usize,
) i32 {
    _ = function;
    if (findIsolate(isolate) == null) return GR_ERR_INVALID_ISOLATE;
    const prefix = "{\"echoed\":";
    const suffix = "}";
    const total = prefix.len + args_len + suffix.len;
    out_result_len.* = total;
    if (total > out_result_cap) return GR_ERR_BUFFER_TOO_SMALL;
    @memcpy(out_result[0..prefix.len], prefix);
    if (args_len > 0) {
        @memcpy(out_result[prefix.len .. prefix.len + args_len], args_json[0..args_len]);
    }
    @memcpy(
        out_result[prefix.len + args_len .. total],
        suffix,
    );
    return GR_OK;
}

export fn gr_microtasks_drain(isolate: u64) i32 {
    return if (findIsolate(isolate) != null) GR_OK else GR_ERR_INVALID_ISOLATE;
}

export fn gr_callback_invoke(
    isolate: u64,
    name: [*c]const u8,
    name_len: usize,
    args_json: [*c]const u8,
    args_len: usize,
    out_result: [*c]u8,
    out_result_cap: usize,
    out_result_len: *usize,
) i32 {
    const iso = findIsolate(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const idx = findCallback(iso, name[0..name_len]) orelse return GR_ERR_CALLBACK_NOT_FOUND;
    const cb = iso.callbacks.items[idx];
    return cb.fn_ptr(cb.data, args_json, args_len, out_result, out_result_cap, out_result_len);
}
