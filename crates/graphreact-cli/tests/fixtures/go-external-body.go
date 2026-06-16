// M17-phase3a external Go body referenced from go-external-body.grx
// via <Counter src="./go-external-body.go" lang="go"/>. Validates the
// end-to-end mount path: parser AST → multi-file resolver reads this
// file → IR carries the body → codegen-go composes it with the
// emitted prelude → go build -buildmode=c-shared produces the
// loadable artifact → engine-go LoadedRuntime opens it.
//
// **M17-phase3a-v1**: the body is a stub; full Use* surface lands in
// phase 3c-h.
//
// func ExternalCounterBody(ctx Ctx) Manifest {
//     return Manifest{
//         Accepts: []string{"increment"},
//         Emits:   []string{"count-updated"},
//         Extras:  map[string]any{"loaded_from": "external_go_file"},
//     }
// }
