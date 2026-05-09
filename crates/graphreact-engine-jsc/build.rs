//! Link the Zig-built JSC bridge static lib + the macOS system
//! `JavaScriptCore` framework, plus (M12.3b-phase1.5) compile and
//! link the C++ drain shim that calls
//! `JSC::VM::drainMicrotasks()` against the framework via the
//! `JSContextGroupRef → JSC::VM*` layout-compat cast.
//!
//! The Zig static lib at
//! `zig/bridge/zig-out/lib/libgraphreact_jsc_zig.a` is produced by
//! `cargo xtask zig-build` (which shells out to `zig build`). If the
//! lib is missing, build.rs prints a clear installation/rebuild hint
//! and fails — there is no fallback today (M12.3b-phase4 will add a
//! vendored-WebKit option for non-macOS).

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../../zig/bridge/jsc.zig");
    println!("cargo:rerun-if-changed=../../zig/bridge/build.zig");
    println!("cargo:rerun-if-changed=src/drain_shim.cpp");

    // Walk up from CARGO_MANIFEST_DIR (this crate) to the workspace
    // root, then into zig/bridge/zig-out/lib/.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .parent() // crates/
        .and_then(std::path::Path::parent) // workspace root
        .expect("manifest dir has parent")
        .to_path_buf();
    let lib_dir = workspace_root.join("zig/bridge/zig-out/lib");
    let lib_path = lib_dir.join("libgraphreact_jsc_zig.a");

    assert!(
        lib_path.exists(),
        "graphreact-engine-jsc: missing static lib at {}.\n\
         \n\
         The JSC bridge is produced by zig from `zig/bridge/jsc.zig`.\n\
         Build it via:\n\
         \n\
         \tcargo xtask zig-build --release\n\
         \n\
         Requires zig 0.16+ on $PATH. Install with `brew install zig` (macOS),\n\
         or see https://ziglang.org/download/.\n",
        lib_path.display()
    );

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=graphreact_jsc_zig");

    // Link the macOS system JavaScriptCore framework. Linux/Windows
    // support (M12.3b-phase4) will swap this for `cargo:rustc-link-lib=static=JavaScriptCore`
    // against a vendored build.
    if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=framework=JavaScriptCore");

        // M12.3b-phase1.5: compile the C++ drain shim. The shim
        // forward-declares `JSC::VM::drainMicrotasks()`; the C++
        // compiler mangles to `_ZN3JSC2VM15drainMicrotasksEv` which
        // the macOS framework binary exports. The linker resolves
        // the symbol against `-framework JavaScriptCore` declared
        // above. `-fno-rtti -fno-exceptions` keep the object file
        // tiny and ABI-stable; `-std=c++17` is the floor for the
        // `<JavaScriptCore/JavaScriptCore.h>` headers shipped on
        // macOS 12+.
        cc::Build::new()
            .cpp(true)
            .file("src/drain_shim.cpp")
            .flag_if_supported("-std=c++17")
            .flag_if_supported("-fno-rtti")
            .flag_if_supported("-fno-exceptions")
            .compile("graphreact_jsc_drain_shim");
    } else {
        panic!(
            "graphreact-engine-jsc: only macOS is supported in M12.3a/M12.3b-phase1.5. \
             Linux/Windows via vendored WebKit is the M12.3b-phase4 deliverable."
        );
    }
}
