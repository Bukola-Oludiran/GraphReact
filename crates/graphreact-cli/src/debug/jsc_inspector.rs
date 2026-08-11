//! **M16-Backend Phase 9.** JSC RemoteInspector install via the Zig
//! bridge added in [`graphreact_engine_jsc::JscEngine::enable_debugger`].
//!
//! ## Native attach (macOS lockdownd discovery)
//!
//! `JSGlobalContextSetRemoteInspectionEnabled(true)` + `JSRemoteInspectorStart()`
//! mark the isolate's JSGlobalContext as remotely inspectable and
//! start the system RemoteInspector singleton. Apple's lockdownd
//! surfaces inspectable JSContexts in `Safari → Develop → [Hostname]
//! → JSContext`; the operator opens Web Inspector and steps through
//! the `.grx` JS body source. Same shape as P8's CPython story
//! (debugpy + VS Code Python extension / PyCharm) — engine-native
//! debug protocol consumed by the engine-native IDE.
//!
//! ## CDP-over-TCP attach (deferred)
//!
//! VS Code's JS debugger / Chrome DevTools / WebStorm all speak CDP
//! (Chrome DevTools Protocol). JSC's native protocol is WebKit's
//! RemoteInspector, which is CDP-shaped but not byte-identical;
//! attaching CDP-shaped clients requires a vendored-WebKit
//! `RemoteInspectorServer` plus a CDP shim. That's M-Prod scope and
//! aligns with V8's eventual return — V8 is soft-demoted per
//! [decision 0001]; CDP-shaped clients defer with V8.
//!
//! See [feedback_axis_b_no_browser.md] for the symmetric Axis B
//! framing — Python → debugpy + VS Code/PyCharm, JSC →
//! RemoteInspector + Safari, JVM → JDWP + IntelliJ. No browsers in
//! the protocol path.
//!
//! ## Per-process model
//!
//! `JSRemoteInspectorStart` is singleton-init at the framework
//! level; subsequent calls are no-ops. The JscEngine layers an
//! `AtomicBool` guard over this for cheaper short-circuit on repeat
//! invocations from the same engine instance (e.g., when the watch
//! loop remounts after a reload).
//!
//! ## `port` parameter
//!
//! Plumbed through for forward-compat with M-Prod's TCP-bound
//! listener and for symmetry with `--debug=cpython:<port>` (which
//! IS port-bound via debugpy). Today the JSC bridge ignores it on
//! macOS lockdownd discovery — Safari finds the JSContext by
//! process traversal, not by TCP probe.
//!
//! [decision 0001]: ../../../../docs/decisions/0001-v8-engine-question.md

use graphreact_engine_jsc::JscEngine;
use graphreact_engine_zig::handles::IsolateHandle;
use graphreact_engine_zig::EngineError;

/// Install the JSC RemoteInspector listener on `iso`. Idempotent
/// across calls (the JscEngine layers an `AtomicBool` guard on top
/// of the framework-level singleton-init).
///
/// `port` is informational on macOS lockdownd discovery (see module
/// docs). Plan-locked at `9229` for forward-compat with M-Prod's
/// vendored-WebKit RemoteInspectorServer (TCP-bound).
pub fn install_jsc_inspector(
    engine: &mut JscEngine,
    iso: IsolateHandle,
    port: u16,
) -> Result<(), EngineError> {
    engine.enable_debugger(iso, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the install signature against drift. The CLI dispatch arms
    // in commands.rs::build_scheduler_host + commands.rs::run_run +
    // watch_mode.rs::run_jsc_watch_blocking all call this helper with
    // (&mut JscEngine, IsolateHandle, u16). Renaming a parameter or
    // adding a required argument needs all three callsites updated;
    // this test fails the build before it reaches CI.
    #[test]
    fn install_jsc_inspector_signature_pinned() {
        // Compile-time check via function pointer coercion — the
        // signature is what makes the function pointer typecheck.
        let _: fn(&mut JscEngine, IsolateHandle, u16) -> Result<(), EngineError> =
            install_jsc_inspector;
    }
}
