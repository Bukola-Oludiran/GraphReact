// Minimal Go c-shared artifact for the graphreact-engine-go
// Engine-conformance test suite.
//
// **NOT load-bearing for production.** This file exists only so the
// engine-go crate's `cargo test` can validate the loader + Engine
// trait round-trip against a real `go build -buildmode=c-shared`
// artifact. Real Go body bodies are emitted by
// `graphreact-codegen-go` (M17 phase 3b) — not by this hand-written
// stub.
//
// Status: M17 phase 1+2 acceptance fixture. Stays in lockstep with
// `crates/graphreact-engine-go/src/ffi.rs::GRX_GO_ABI_VERSION` and
// the symbol names enumerated in `crates/graphreact-engine-go/src/loader.rs`.

package main

/*
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// Mirror of crate::ffi::gr_host_callback_fn.
typedef int (*gr_host_callback_fn)(
    void *cb_data,
    const char *args_json,
    size_t args_len,
    char *out_result,
    size_t out_result_cap,
    size_t *out_result_len
);

// Inline wrapper so Go can call the function-pointer cleanly via
// cgo. Returns the C int the trampoline produced.
static int grx_invoke_host_callback(
    gr_host_callback_fn cb,
    void *cb_data,
    const char *args_json,
    size_t args_len,
    char *out_result,
    size_t out_result_cap,
    size_t *out_result_len
) {
    return cb(cb_data, args_json, args_len, out_result, out_result_cap, out_result_len);
}
*/
import "C"

import (
	"encoding/json"
	"sync"
	"sync/atomic"
	"unsafe"
)

// ---------------------------------------------------------------------
// Status codes — share values with crates/graphreact-engine-go/src/ffi.rs.
// ---------------------------------------------------------------------

const (
	GR_OK                     = 0
	GR_ERR_NOT_INITIALIZED    = -1
	GR_ERR_INVALID_ISOLATE    = -2
	GR_ERR_INVALID_MODULE     = -3
	GR_ERR_INVALID_FUNCTION   = -4
	GR_ERR_CALLBACK_DUPLICATE = -5
	GR_ERR_CALLBACK_NOT_FOUND = -6
	GR_ERR_BUFFER_TOO_SMALL   = -7
	GR_ERR_INTERNAL           = -99
)

const GRX_GO_ABI_VERSION uint32 = 1

// ---------------------------------------------------------------------
// Per-isolate state — minimal "calculator" module + InvokeHostDouble
// host-callback round-trip fixture. Real codegen-emitted artifacts
// host the user's component body code here; the test stub hardcodes
// two functions.
// ---------------------------------------------------------------------

type callbackSlot struct {
	cbFn   C.gr_host_callback_fn
	cbData unsafe.Pointer
}

type isolate struct {
	mu          sync.Mutex
	modules     map[uint64]string // handle → module name
	functions   map[uint64]string // handle → "<module>.<fn>"
	callbacks   map[uint64]callbackSlot
	callbacksByName map[string]uint64
	nextModule  uint64
	nextFunc    uint64
}

type state struct {
	mu       sync.Mutex
	live     int64 // atomic
	isolates map[uint64]*isolate
	nextIso  uint64
	initted  bool
}

var st = state{isolates: map[uint64]*isolate{}}

// Process-global next callback id — matches the Rust-side CALLBACK_REGISTRY
// which keys on a u64 the Go side allocates.
var nextCallbackId uint64

//export GrxAbiVersion
func GrxAbiVersion() C.uint32_t {
	return C.uint32_t(GRX_GO_ABI_VERSION)
}

//export GrxEngineInitialize
func GrxEngineInitialize() C.int {
	st.mu.Lock()
	defer st.mu.Unlock()
	st.initted = true
	return GR_OK
}

//export GrxEngineShutdown
func GrxEngineShutdown() C.int {
	st.mu.Lock()
	defer st.mu.Unlock()
	st.initted = false
	// Drop all isolates; matches CPython's Py_Finalize semantics.
	for k := range st.isolates {
		delete(st.isolates, k)
	}
	atomic.StoreInt64(&st.live, 0)
	return GR_OK
}

//export GrxEngineLiveIsolates
func GrxEngineLiveIsolates() C.uint64_t {
	return C.uint64_t(atomic.LoadInt64(&st.live))
}

//export GrxIsolateCreate
func GrxIsolateCreate(outHandle *C.uint64_t) C.int {
	st.mu.Lock()
	defer st.mu.Unlock()
	if !st.initted {
		return GR_ERR_NOT_INITIALIZED
	}
	st.nextIso++
	id := st.nextIso
	st.isolates[id] = &isolate{
		modules:         map[uint64]string{},
		functions:       map[uint64]string{},
		callbacks:       map[uint64]callbackSlot{},
		callbacksByName: map[string]uint64{},
	}
	atomic.AddInt64(&st.live, 1)
	*outHandle = C.uint64_t(id)
	return GR_OK
}

//export GrxIsolateDispose
func GrxIsolateDispose(handle C.uint64_t) C.int {
	st.mu.Lock()
	defer st.mu.Unlock()
	id := uint64(handle)
	if _, ok := st.isolates[id]; !ok {
		return GR_ERR_INVALID_ISOLATE
	}
	delete(st.isolates, id)
	atomic.AddInt64(&st.live, -1)
	return GR_OK
}

func isoLookup(handle uint64) *isolate {
	st.mu.Lock()
	defer st.mu.Unlock()
	return st.isolates[handle]
}

//export GrxModuleRegister
func GrxModuleRegister(
	iso C.uint64_t,
	moduleName *C.char, moduleNameLen C.size_t,
	source *C.char, sourceLen C.size_t,
	outModule *C.uint64_t,
) C.int {
	is := isoLookup(uint64(iso))
	if is == nil {
		return GR_ERR_INVALID_ISOLATE
	}
	name := C.GoStringN(moduleName, C.int(moduleNameLen))
	// `source` is ignored in the test fixture — bodies are
	// pre-compiled hardcoded functions. Real codegen-emitted
	// artifacts may stash source by name here.
	_ = C.GoStringN(source, C.int(sourceLen))
	is.mu.Lock()
	defer is.mu.Unlock()
	is.nextModule++
	id := is.nextModule
	is.modules[id] = name
	*outModule = C.uint64_t(id)
	return GR_OK
}

//export GrxFunctionResolve
func GrxFunctionResolve(
	iso C.uint64_t,
	module C.uint64_t,
	name *C.char, nameLen C.size_t,
	outFunction *C.uint64_t,
) C.int {
	is := isoLookup(uint64(iso))
	if is == nil {
		return GR_ERR_INVALID_ISOLATE
	}
	is.mu.Lock()
	defer is.mu.Unlock()
	moduleName, ok := is.modules[uint64(module)]
	if !ok {
		return GR_ERR_INVALID_MODULE
	}
	fnName := C.GoStringN(name, C.int(nameLen))
	// Only specific module+function pairs are recognised by the
	// test fixture.
	key := moduleName + "." + fnName
	switch key {
	case "calculator.Add",
		"callbacks.InvokeHostDouble",
		"callbacks.InvokeLargeEcho",
		"payloads.LargeJson":
		is.nextFunc++
		id := is.nextFunc
		is.functions[id] = key
		*outFunction = C.uint64_t(id)
		return GR_OK
	}
	return GR_ERR_INVALID_FUNCTION
}

//export GrxFunctionCall
func GrxFunctionCall(
	iso C.uint64_t,
	function C.uint64_t,
	argsJson *C.char, argsLen C.size_t,
	outResult *C.char, outResultCap C.size_t,
	outResultLen *C.size_t,
) C.int {
	is := isoLookup(uint64(iso))
	if is == nil {
		return GR_ERR_INVALID_ISOLATE
	}
	is.mu.Lock()
	fnKey, ok := is.functions[uint64(function)]
	is.mu.Unlock()
	if !ok {
		return GR_ERR_INVALID_FUNCTION
	}

	argsBytes := C.GoBytes(unsafe.Pointer(argsJson), C.int(argsLen))
	var args []interface{}
	if len(argsBytes) > 0 {
		if err := json.Unmarshal(argsBytes, &args); err != nil {
			return GR_ERR_INTERNAL
		}
	}

	var resultJson []byte

	switch fnKey {
	case "calculator.Add":
		if len(args) != 2 {
			return GR_ERR_INTERNAL
		}
		a, ok1 := args[0].(float64)
		b, ok2 := args[1].(float64)
		if !ok1 || !ok2 {
			return GR_ERR_INTERNAL
		}
		out, err := json.Marshal(a + b)
		if err != nil {
			return GR_ERR_INTERNAL
		}
		resultJson = out

	case "callbacks.InvokeHostDouble":
		// Look up the "double" callback registered against this
		// isolate, invoke it with the first arg, and return what
		// it returned.
		is.mu.Lock()
		cbId, hasCb := is.callbacksByName["double"]
		var slot callbackSlot
		if hasCb {
			slot = is.callbacks[cbId]
		}
		is.mu.Unlock()
		if !hasCb {
			return GR_ERR_CALLBACK_NOT_FOUND
		}

		// Re-serialize the args slice (the host gets the exact
		// shape the Go body would pass).
		argsForHost, err := json.Marshal(args)
		if err != nil {
			return GR_ERR_INTERNAL
		}
		argsCStr := C.CString(string(argsForHost))
		defer C.free(unsafe.Pointer(argsCStr))

		// Allocate a 4KB result buffer; the trampoline writes
		// the JSON-result into it. If too small, grow once.
		// NB: defer a closure (NOT `defer C.free(buf)`) so we free
		// whatever `buf` points at when the function returns —
		// `defer C.free(buf)` would capture the original pointer
		// and double-free it after the grow-and-retry reassigns
		// `buf`.
		cap := C.size_t(4096)
		buf := C.malloc(cap)
		defer func() { C.free(buf) }()
		var written C.size_t

		rc := C.grx_invoke_host_callback(
			slot.cbFn,
			slot.cbData,
			argsCStr, C.size_t(len(argsForHost)),
			(*C.char)(buf), cap, &written,
		)
		if rc == GR_ERR_BUFFER_TOO_SMALL {
			cap = written
			C.free(buf)
			buf = C.malloc(cap)
			rc = C.grx_invoke_host_callback(
				slot.cbFn,
				slot.cbData,
				argsCStr, C.size_t(len(argsForHost)),
				(*C.char)(buf), cap, &written,
			)
		}
		if rc != GR_OK {
			return rc
		}

		resultJson = C.GoBytes(buf, C.int(written))

	case "callbacks.InvokeLargeEcho":
		// Look up "large_echo" callback, pass the args verbatim,
		// and return its result. The host-side closure returns a
		// JSON string > 4 KiB to force the Rust trampoline's
		// GR_ERR_BUFFER_TOO_SMALL branch + the Go-side grow-once
		// path.
		is.mu.Lock()
		cbId, hasCb := is.callbacksByName["large_echo"]
		var slot callbackSlot
		if hasCb {
			slot = is.callbacks[cbId]
		}
		is.mu.Unlock()
		if !hasCb {
			return GR_ERR_CALLBACK_NOT_FOUND
		}
		argsForHost, err := json.Marshal(args)
		if err != nil {
			return GR_ERR_INTERNAL
		}
		argsCStr := C.CString(string(argsForHost))
		defer C.free(unsafe.Pointer(argsCStr))
		// Same defer-closure pattern as InvokeHostDouble — see the
		// comment there explaining why a literal `defer C.free(buf)`
		// would double-free on the grow-and-retry path.
		bufCap := C.size_t(4096)
		buf := C.malloc(bufCap)
		defer func() { C.free(buf) }()
		var written C.size_t
		rc := C.grx_invoke_host_callback(
			slot.cbFn, slot.cbData,
			argsCStr, C.size_t(len(argsForHost)),
			(*C.char)(buf), bufCap, &written,
		)
		if rc == GR_ERR_BUFFER_TOO_SMALL {
			bufCap = written
			C.free(buf)
			buf = C.malloc(bufCap)
			rc = C.grx_invoke_host_callback(
				slot.cbFn, slot.cbData,
				argsCStr, C.size_t(len(argsForHost)),
				(*C.char)(buf), bufCap, &written,
			)
		}
		if rc != GR_OK {
			return rc
		}
		resultJson = C.GoBytes(buf, C.int(written))

	case "payloads.LargeJson":
		// Return a JSON array of `n` 100-byte strings. Drives the
		// Rust-side call_with_buffer grow-and-retry loop AND its
		// 1-MiB-cap exit when n is large enough.
		if len(args) != 1 {
			return GR_ERR_INTERNAL
		}
		nf, ok := args[0].(float64)
		if !ok {
			return GR_ERR_INTERNAL
		}
		n := int(nf)
		if n < 0 {
			return GR_ERR_INTERNAL
		}
		chunk := make([]byte, 100)
		for i := range chunk {
			chunk[i] = 'x'
		}
		arr := make([]string, n)
		for i := 0; i < n; i++ {
			arr[i] = string(chunk)
		}
		out, err := json.Marshal(arr)
		if err != nil {
			return GR_ERR_INTERNAL
		}
		resultJson = out

	default:
		return GR_ERR_INVALID_FUNCTION
	}

	// Write resultJson into outResult using the bridge length
	// convention.
	resultLen := C.size_t(len(resultJson))
	*outResultLen = resultLen
	if resultLen > outResultCap {
		return GR_ERR_BUFFER_TOO_SMALL
	}
	if resultLen > 0 {
		C.memcpy(unsafe.Pointer(outResult), unsafe.Pointer(&resultJson[0]), resultLen)
	}
	return GR_OK
}

//export GrxCallbackRegister
func GrxCallbackRegister(
	iso C.uint64_t,
	name *C.char, nameLen C.size_t,
	cbFn C.gr_host_callback_fn,
	cbData unsafe.Pointer,
	outCallbackId *C.uint64_t,
) C.int {
	is := isoLookup(uint64(iso))
	if is == nil {
		return GR_ERR_INVALID_ISOLATE
	}
	cbName := C.GoStringN(name, C.int(nameLen))
	id := atomic.AddUint64(&nextCallbackId, 1)
	// Encode the id back as the cb_data pointer the trampoline
	// reads. Match the Rust side's `cb_data as usize as u64`
	// decoding.
	idAsPointer := unsafe.Pointer(uintptr(id))
	is.mu.Lock()
	defer is.mu.Unlock()
	if _, exists := is.callbacksByName[cbName]; exists {
		return GR_ERR_CALLBACK_DUPLICATE
	}
	is.callbacks[id] = callbackSlot{cbFn: cbFn, cbData: idAsPointer}
	is.callbacksByName[cbName] = id
	_ = cbData // We deliberately ignore the caller-supplied cb_data
	           // because the Rust side passes nullptr; the id-as-pointer
	           // round-trip is what the trampoline expects.
	*outCallbackId = C.uint64_t(id)
	return GR_OK
}

//export GrxMicrotasksDrain
func GrxMicrotasksDrain(iso C.uint64_t) C.int {
	is := isoLookup(uint64(iso))
	if is == nil {
		return GR_ERR_INVALID_ISOLATE
	}
	// M17 phase 1+2 — soft no-op. Real microtask semantics arrive
	// in M17 phase 3g (context.Context cancellation + WaitGroup).
	return GR_OK
}

// main is required by `go build -buildmode=c-shared`. Never called.
func main() {}
