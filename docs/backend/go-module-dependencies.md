# Go module dependencies

GraphReact Go bodies may import public, private, or local Go packages.
The application owns ordinary Go sidecars; GraphReact discovers and
consumes them without rewriting them.

## Module selection

For a Go or mixed-runtime mount, GraphReact walks upward from the entry
`.grx` file and selects the nearest `go.mod`. Every `.grx` file and
external Go body containing a `lang="go"` definition must resolve to
that same module root. A system cannot span multiple Go modules.

When no `go.mod` is found, GraphReact preserves the dependency-free
behavior and emits its minimal generated module. When a sidecar module
is found, generated bodies live in `.grx-go-build-<entry>/` below the
module root, so they can import packages from the application module.

Builds always use:

```text
GOWORK=off
go mod verify
go build -mod=readonly -buildmode=c-shared
```

GraphReact refuses ambiguous `vendor/` trees and never runs `go get`,
`go mod tidy`, or any command that edits `go.mod` or `go.sum`. Update
and commit those files with standard Go tooling before starting `grx`.

## Public modules and offline operation

Public modules use the standard Go proxy and checksum database. Pin
every direct dependency in `go.mod` and commit `go.sum`.

Use `--go-module-cache <PATH>` to select a deployment cache. Add
`--go-offline` to disable all proxy and VCS access; a missing cache
entry then fails startup rather than falling back to the network.
Both flags are accepted by `grx run` and `grx cluster daemon` when the
runtime is `go` or `mixed`.

## Private modules

Private credentials are read from the same rotatable GraphReact secret
backend used by `ctx.Secret`, but build authorization is independent
of a node's `needs_secret` list. Put only the mapping policy—not secret
values—in `go.auth.json` beside `go.mod`:

```json
{
  "version": 1,
  "privatePatterns": ["github.com/acme/*"],
  "https": [
    {
      "host": "github.com",
      "username": "x-access-token",
      "passwordSecret": "GITHUB_TOKEN"
    }
  ],
  "proxy": {
    "url": "https://go.example.com",
    "usernameSecret": "GO_PROXY_USER",
    "passwordSecret": "GO_PROXY_TOKEN"
  }
}
```

For the default environment backend, provide
`GRX_SECRET_GITHUB_TOKEN`, `GRX_SECRET_GO_PROXY_USER`, and
`GRX_SECRET_GO_PROXY_TOKEN`. The keyring backend uses the same logical
secret identifiers.

Only credential-free HTTPS URLs are accepted in policy. GraphReact
materializes short-lived mode-0600 Git configuration under a mode-0700
temporary home, sanitizes the Go subprocess environment, redacts plain
and URL-encoded secret values from toolchain errors, and deletes the
temporary directory after the build. SSH private-key authentication is
not supported in this version.

## Cache and supply-chain artifacts

The build fingerprint covers generated sources, `go.mod`, `go.sum`,
`go.auth.json` without resolved values, Go version, target, and build
mode. A cached shared library is reused only when that fingerprint and
the library SHA-256 both match. Credential rotation does not invalidate
the binary.

Each successful build emits beside the library:

- `grx-go-build.json` — fingerprint and verified artifact hash;
- `grx-go-dependencies.json` — deterministic module inventory;
- `grx-go.cdx.json` — CycloneDX 1.4 dependency evidence.

These files never contain resolved credentials. Go hot reload remains
unsupported, and cluster reload packets remain IR-only. Every cluster
node must therefore receive identical module sidecars and caches before
initial startup.

See [`../../examples/openai-go-agent`](../../examples/openai-go-agent)
for a pinned third-party SDK example whose API call is triggered only
by a message.
