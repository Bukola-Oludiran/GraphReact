// M17-phase3b-real external Go body referenced from go-external-body.grx
// via <Counter src="./go-external-body.go" lang="go"/>. Validates the
// end-to-end mount path: parser AST -> multi-file resolver reads this
// file -> IR carries the body -> codegen-go composes it with the
// emitted prelude -> go build -buildmode=c-shared produces the
// loadable artifact -> engine-go LoadedRuntime opens it -> the
// codegen-emitted dispatch.go registers CounterBody so GrxFunctionCall
// can route (Counter, "body") into the registry.

func CounterBody(ctx Ctx) Manifest {
    return Manifest{
        ID:      ctx.ID,
        Accepts: []string{"increment"},
        Emits:   []string{"count-updated"},
    }
}
