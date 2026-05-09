// build.zig — Zig build orchestration for the engine bridges.
//
// Produces:
//   - `libgraphreact_jsc_zig.a` from `jsc.zig` (M12.3a/M12.3b) —
//     consumed by `graphreact-engine-jsc`.
//   - `libgraphreact_cpython_zig.a` from `cpython.zig` (M13) —
//     consumed by `graphreact-engine-cpython`.
//
// The `graphreact-engine-echo` crate's C reference at
// `crates/graphreact-engine-echo/c/echo.c` is built separately by
// that crate's own `build.rs` (cc-rs); the `zig/bridge/echo.zig`
// parity source predates Zig 0.16's API churn and is currently
// disabled — can be re-enabled once ported.
//
// Invoked via `cargo xtask zig-build` (see `xtask/src/main.rs`).
//
// **M13-phase3f (2026-05-05) — Python build options.** Two new build
// options for the cpython bridge let xtask zig-build (or any direct
// `zig build` invocation) target a specific Python install:
//
//   - `-Dpython_include=<path>` overrides the include directory
//     (default: macOS Command Line Tools 3.9 framework headers).
//   - `-Dpython_3_12=true` enables the PEP 684 per-interpreter GIL
//     path inside `cpython.zig` (uses `Py_NewInterpreterFromConfig`
//     with `gil = OWN_GIL` instead of legacy `Py_NewInterpreter` and
//     a real `Py_FinalizeEx` on engine shutdown).
//
// xtask zig-build derives both options from a pkg-config probe
// mirroring `crates/graphreact-engine-cpython/build.rs`'s resolver,
// so a developer just runs `cargo xtask zig-build --release` and the
// right Python is picked automatically.

const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    // ----- jsc bridge (M12.3a / M12.3b-phase1.5) -----
    //
    // Zig 0.16+: libraries are built from Modules created via
    // `createModule`, then passed to `addLibrary` with explicit
    // linkage. Older 0.13 API used `addStaticLibrary` directly.
    const jsc_module = b.createModule(.{
        .root_source_file = b.path("jsc.zig"),
        .target = target,
        .optimize = optimize,
    });
    // JSC's C API headers live behind the `JavaScriptCore` framework
    // on macOS. Link libC and the framework so `@cImport` resolves.
    jsc_module.link_libc = true;
    jsc_module.linkFramework("JavaScriptCore", .{});

    const jsc_lib = b.addLibrary(.{
        .name = "graphreact_jsc_zig",
        .root_module = jsc_module,
        .linkage = .static,
    });
    b.installArtifact(jsc_lib);

    // ----- cpython bridge (M13) -----
    //
    // Phase 1+2 hardcoded the macOS Command Line Tools 3.9 framework
    // headers; phase 3f makes the include path + the PEP 684 cfg
    // flag user-overridable so a Homebrew Python 3.13 (or a Linux
    // python3-dev install, or a Windows python3.lib) can be selected
    // without editing build.zig.
    //
    // `python_include` defaults to the Apple CLT 3.9 path so a stock
    // macOS without Homebrew still builds. Linux/Windows users (or
    // macOS-with-Homebrew users) override via xtask zig-build's
    // pkg-config-derived `-Dpython_include=...` invocation.
    //
    // `python_3_12` defaults to false — the legacy single-GIL
    // sub-interpreter path stays the safe fallback. xtask zig-build
    // flips this to true when pkg-config reveals a Python ≥3.12.
    const python_include = b.option(
        []const u8,
        "python_include",
        "Python include directory containing Python.h (default: macOS Command Line Tools 3.9 framework headers)",
    ) orelse "/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/Headers";

    const python_3_12 = b.option(
        bool,
        "python_3_12",
        "Build the cpython bridge against Python 3.12+ (enables PEP 684 per-interpreter GIL path)",
    ) orelse false;

    // M13-phase3h. Set when the resolved Python is a PEP 703
    // free-threaded build (`python3.Xt`). Today this only flips the
    // engine version string's annotation — the `cpython.zig` runtime
    // path is uniform across GIL'd and free-threaded builds because
    // PEP 703 doesn't change any C-API surface we use, only the
    // semantics. Future slices that want concurrent isolate execution
    // will branch on `build_opts.python_free_threaded` to relax
    // Zig-side state-mutation serialisation.
    const python_free_threaded = b.option(
        bool,
        "python_free_threaded",
        "Build the cpython bridge against a PEP 703 free-threaded Python (`python3.Xt`)",
    ) orelse false;

    const cpython_module = b.createModule(.{
        .root_source_file = b.path("cpython.zig"),
        .target = target,
        .optimize = optimize,
    });
    cpython_module.link_libc = true;
    cpython_module.addIncludePath(.{ .cwd_relative = python_include });

    // Surface the build options as a comptime-importable module so
    // `cpython.zig` can branch on `build_opts.python_3_12` /
    // `build_opts.python_free_threaded` at comptime — the legacy
    // path needs to compile against Python 3.9 headers that don't
    // have `Py_NewInterpreterFromConfig` / `PyInterpreterConfig`,
    // so the 3.12+ branch must be elided when the flag is false.
    const cpython_opts = b.addOptions();
    cpython_opts.addOption(bool, "python_3_12", python_3_12);
    cpython_opts.addOption(bool, "python_free_threaded", python_free_threaded);
    cpython_module.addOptions("build_opts", cpython_opts);

    const cpython_lib = b.addLibrary(.{
        .name = "graphreact_cpython_zig",
        .root_module = cpython_module,
        .linkage = .static,
    });
    b.installArtifact(cpython_lib);

    // ----- echo bridge (legacy, disabled under Zig 0.16) -----
    //
    // The original echo.zig was written against Zig 0.13 APIs that
    // changed substantially in 0.16. The C reference at `c/echo.c`
    // proves the same FFI surface, so re-porting echo.zig is
    // deferred. Re-enable here when porting lands.
}
