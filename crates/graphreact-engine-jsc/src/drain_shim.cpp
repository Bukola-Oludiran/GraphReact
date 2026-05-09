// drain_shim.cpp — macOS-only thin C++ shim that calls
// `JSC::VM::drainMicrotasks()` against the system JavaScriptCore
// framework via the public C API's `JSContextGroupRef → JSC::VM*`
// layout-compatible cast.
//
// **M12.3b-phase1.5.** Phase 1 left `gr_jsc_microtasks_drain` as a
// documented no-op because JSC's public C API on macOS does not
// expose a drain entry point. This shim closes that gap WITHOUT
// vendoring WebKit:
//
//   - The mangled symbol `_ZN3JSC2VM15drainMicrotasksEv`
//     (`JSC::VM::drainMicrotasks()`) is exported from
//     `/System/Library/Frameworks/JavaScriptCore.framework/JavaScriptCore`
//     on macOS (verified via `nm` on the SDK's `JavaScriptCore.tbd`).
//   - Forward-declaring `JSC::VM` as a class with a single
//     `drainMicrotasks()` method lets the C++ compiler emit the call
//     with the matching mangled name; the linker resolves it against
//     the framework binary at final-binary link time.
//   - `OpaqueJSContextGroup` (the underlying type of the public
//     `JSContextGroupRef`) is layout-identical to `JSC::VM` per
//     WebKit's internal `JavaScriptCore/API/APICast.h`
//     (`inline JSC::VM* toJS(JSContextGroupRef)` is a plain
//     `reinterpret_cast`). The cast below mirrors that contract.
//
// Linux/Windows do NOT compile this file; M12.3b-phase4 will replace
// it with a vendored-WebKit C++ shim that has full access to JSC's
// internal headers.

#include <JavaScriptCore/JavaScriptCore.h>

namespace JSC {
// Minimal forward declaration: the only method we call. The C++
// compiler mangles `drainMicrotasks()` to
// `_ZN3VM15drainMicrotasksEv` inside `namespace JSC` →
// `_ZN3JSC2VM15drainMicrotasksEv`, which matches the framework's
// exported symbol. No member layout is read; only the mangled call
// site matters for linkage.
class VM {
public:
    void drainMicrotasks();
};
} // namespace JSC

extern "C" void gr_jsc_drain_microtasks_via_ctx(JSContextRef ctx) {
    if (ctx == nullptr) {
        return;
    }
    JSContextGroupRef group = JSContextGetGroup(ctx);
    if (group == nullptr) {
        return;
    }
    JSC::VM* vm = reinterpret_cast<JSC::VM*>(
        const_cast<OpaqueJSContextGroup*>(group));
    vm->drainMicrotasks();
}
