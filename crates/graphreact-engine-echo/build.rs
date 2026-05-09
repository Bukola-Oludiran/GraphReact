//! Compile the C echo engine (`c/echo.c`) into a static library that
//! the Rust crate links against. Mirrors the `build.zig` over at
//! `zig/bridge/build.zig`, which produces an equivalent library
//! once Zig is installed.

fn main() {
    println!("cargo:rerun-if-changed=c/echo.c");
    println!("cargo:rerun-if-changed=c/graphreact_echo.h");

    cc::Build::new()
        .file("c/echo.c")
        .include("c")
        .flag_if_supported("-std=c11")
        .flag_if_supported("-fPIC")
        .warnings(true)
        .extra_warnings(true)
        .compile("graphreact_echo");
}
