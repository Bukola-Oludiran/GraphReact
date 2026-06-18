# syntax=docker/dockerfile:1.7
#
# **M-Prod sub-track #8 — packaging Phase A.** Multi-stage OCI build
# for the `grx` CLI binary.
#
# Stage layout:
#
#   zig-builder  →  rust-builder  →  python-donor  →  final (distroless cc)
#       │              │                  │              │
#       │              │                  │              └─ runtime image, ~120MB
#       │              │                  └─ source of libpython{X}.so.1.0 + stdlib
#       │              └─ produces /work/target/release/grx, dynamically links libpython
#       └─ produces zig/bridge/zig-out/lib/libgraphreact_*_zig.a (frozen .a files)
#
# Why distroless final + python donor (and not python:slim base):
#
#   The cpython engine HARD-PANICS in `engine-cpython/build.rs` if
#   `libgraphreact_cpython_zig.a` is missing AND HARD-LINKS against
#   pkg-config python-3.X-embed (so libpython3.X.so.1.0 must be resolvable
#   at runtime at the absolute LC_RPATH that `crates/graphreact-cli/build.rs`
#   bakes — the pkg-config libdir, typically `/usr/lib/x86_64-linux-gnu`).
#   Distroless/cc-debian12 has ca-certificates (rustls-native-certs needs
#   `/etc/ssl/certs/ca-certificates.crt`) and `libc` + `libgcc_s` — but
#   NEITHER `libpython3.X.so.1.0` NOR `/usr/lib/python3.X/` stdlib. We COPY
#   both from a `python:3.X-slim-bookworm` donor stage so the final image
#   preserves the M-Prod attack-surface posture without rebuilding CPython
#   from source.
#
# Per-CPython-minor ABI lock:
#
#   Re-build with `--build-arg GRX_PYTHON_MINOR=3.12` / `3.13` / `3.14` to
#   produce per-Python-minor image slices (grx-py-3.12 / grx-py-3.13 /
#   grx-py-3.14). The default below pins 3.13 to match the test-go cell's
#   default in `.github/workflows/ci.yml`. 3.14t (PEP 703 free-threaded)
#   waits until `actions/setup-python` ships free-threaded support.
#
# Reproducibility:
#
#   The `rust-builder` stage mirrors the `reproducible-build` CI cell in
#   `.github/workflows/ci.yml`: RUSTFLAGS=`--remap-path-prefix=...`
#   + SOURCE_DATE_EPOCH propagated from the build context. Phase A scope:
#   same-builder reproducibility only. Cross-host reproducibility is
#   Phase B (see `docs/backend/m-prod-supply-chain.md`).
#
# Multi-arch:
#
#   `docker buildx build --platform=linux/amd64,linux/arm64` works under
#   QEMU emulation for Phase A. Native arm64 cross-builds need an xtask
#   `-Dtarget` knob (Phase B); see `docs/backend/m-prod-packaging.md`.

# ---- Args (top-level so every stage can FROM ... ARG) ----
#
# NOTE on coupling with the FROM-line digest pins below:
#   Phase B Part 1 pins each base-image FROM at `@sha256:<digest>`
#   alongside the LITERAL tag (e.g. `rust:1.83-bookworm@sha256:...`).
#   The ARG declarations below are STILL consumed by the per-stage
#   apt-get install steps (`python${GRX_PYTHON_MINOR}-dev`) and by
#   ENV exports, so removing them would break the cpython link path.
#   They are NOT interpolated into the FROM lines anymore — bumping
#   `RUST_VERSION` / `GRX_PYTHON_MINOR` / `DEBIAN_RELEASE` REQUIRES a
#   coupled update to the literal tag strings at the FROM lines AND
#   a re-resolution of the `@sha256:<digest>` (see top-of-file
#   "Base-image digest pins" block below for the re-pin command).
#
# Base-image digest pins (resolved 2026-06-18). Re-pin via:
#   for img in rust:1.83-bookworm \
#              python:3.13-slim-bookworm \
#              gcr.io/distroless/cc-debian12:nonroot; do
#     echo -n "$img: "; \
#       docker buildx imagetools inspect "$img" --format '{{.Manifest.Digest}}'
#   done
# Bumping RUST_VERSION / GRX_PYTHON_MINOR / DEBIAN_RELEASE ARGs requires
# coupled bumps to the literal tag strings at the FROM lines below.

ARG GRX_PYTHON_MINOR=3.13
ARG RUST_VERSION=1.83
ARG ZIG_VERSION=0.16.0
ARG DEBIAN_RELEASE=bookworm

# ============================================================================
# Stage 1 — zig-builder: produce the static .a bridge libs
# ============================================================================
#
# `engine-cpython/build.rs` hard-panics if
# `zig/bridge/zig-out/lib/libgraphreact_cpython_zig.a` is missing
# (see lines 94-106). `cargo xtask zig-build --release` is the only
# supported builder. We run it here to produce frozen .a files that the
# `rust-builder` stage consumes.

# Base: rust:1.83-bookworm (resolved 2026-06-18 via
#   docker buildx imagetools inspect rust:1.83-bookworm
# Multi-arch index digest — buildx selects the per-arch manifest via
# the `--platform` flag at build time. Re-pin via the same command
# when bumping RUST_VERSION + DEBIAN_RELEASE in tandem.
FROM rust:1.83-bookworm@sha256:a45bf1f5d9af0a23b26703b3500d70af1abff7f984a7abef5a104b42c02a292b AS zig-builder

ARG GRX_PYTHON_MINOR
ARG ZIG_VERSION

# - python3.X-dev: provides `pkg-config python-3.X-embed` for the zig
#   bridge's `-Dpython_include=<path>` probe.
# - pkg-config: required by `cargo xtask zig-build`'s
#   `resolve_python_for_zig_build()` step.
# - xz-utils + ca-certificates: needed to fetch zig and verify TLS.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        xz-utils \
        pkg-config \
        python${GRX_PYTHON_MINOR} \
        python${GRX_PYTHON_MINOR}-dev \
    && rm -rf /var/lib/apt/lists/*

# Install zig at the pinned version. xtask/src/main.rs requires zig 0.16+.
# Mirror the `.github/workflows/ci.yml` `mlugg/setup-zig@v2` pin so the
# CI repro cell and the Dockerfile produce identical .a outputs.
RUN curl -fSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-$(uname -m)-${ZIG_VERSION}.tar.xz" \
        -o /tmp/zig.tar.xz \
    && mkdir -p /opt/zig \
    && tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1 \
    && rm /tmp/zig.tar.xz \
    && /opt/zig/zig version
ENV PATH="/opt/zig:${PATH}"

WORKDIR /work
COPY . .

# Pin pkg-config so the cpython engine's build.rs resolves the right
# Python minor. `GRAPHREACT_PYTHON_PKGCONFIG` mirrors the CI matrix.
ENV GRAPHREACT_PYTHON_PKGCONFIG="python-${GRX_PYTHON_MINOR}-embed"
ENV GRAPHREACT_PYTHON_VERSION="${GRX_PYTHON_MINOR}"

# Produces zig/bridge/zig-out/lib/{libgraphreact_jsc_zig.a,
# libgraphreact_cpython_zig.a, libgraphreact_echo_zig.a}. The .a files
# are byte-identical inputs to both the rust-builder stage AND the CI
# repro cell — keep both invocations in lockstep when bumping zig.
RUN cargo xtask zig-build --release

# ============================================================================
# Stage 2 — rust-builder: produce target/release/grx
# ============================================================================

# Base: rust:1.83-bookworm (resolved 2026-06-18 via
#   docker buildx imagetools inspect rust:1.83-bookworm
# Same digest as zig-builder by construction — both stages depend on
# the same toolchain image. Re-pin both stages together when bumping.
FROM rust:1.83-bookworm@sha256:a45bf1f5d9af0a23b26703b3500d70af1abff7f984a7abef5a104b42c02a292b AS rust-builder

ARG GRX_PYTHON_MINOR

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        python${GRX_PYTHON_MINOR} \
        python${GRX_PYTHON_MINOR}-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
COPY . .

# Re-use the static .a files from stage 1.
COPY --from=zig-builder /work/zig/bridge/zig-out/ zig/bridge/zig-out/

# Pin the same Python minor that produced the .a files. ABI mismatch
# between zig-builder and rust-builder would surface as a link-time error.
ENV GRAPHREACT_PYTHON_PKGCONFIG="python-${GRX_PYTHON_MINOR}-embed"
ENV GRAPHREACT_PYTHON_VERSION="${GRX_PYTHON_MINOR}"

# Mirror the `reproducible-build` CI cell in `.github/workflows/ci.yml`
# (lines 367, 383): SOURCE_DATE_EPOCH (passed as a build-arg from the
# release pipeline so both the CI repro cell and `docker build` see the
# same value) + path-remap so the runner's homedir doesn't leak into
# debuginfo.
ARG SOURCE_DATE_EPOCH
ENV SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}"

# `GRX_SIGNATURE_URL` (sub-track #7) is baked into the binary via
# `crates/graphreact-cli/build.rs`'s `cargo:rustc-env`. Release pipeline
# (`.github/workflows/release.yml`) sets this via build-arg.
ARG GRX_SIGNATURE_URL=""
ENV GRX_SIGNATURE_URL="${GRX_SIGNATURE_URL}"

ENV RUSTFLAGS="--remap-path-prefix=/work=. --remap-path-prefix=/usr/local/cargo=/cargo"

# `--release` (NOT `--profile release-small`) — the repro cell at
# `.github/workflows/ci.yml` line 386 builds plain `--release` and the
# emitted SHA is part of the supply-chain audit trail.
RUN cargo build --release --bin grx

# ============================================================================
# Stage 3 — python-donor: source of libpython{X}.so.1.0 + stdlib
# ============================================================================
#
# python:slim ships a curated, security-patched CPython tree we can COPY
# into the distroless final stage. Building CPython from source here is
# possible but wastes 5-7min per build for no security gain (Docker
# Hub's python:slim images are kept patched ahead of debian:slim itself).

# Base: python:3.13-slim-bookworm (resolved 2026-06-18 via
#   docker buildx imagetools inspect python:3.13-slim-bookworm
# Multi-arch index digest. Bumping GRX_PYTHON_MINOR or DEBIAN_RELEASE
# requires a coupled re-resolution of this digest.
FROM python:3.13-slim-bookworm@sha256:05b95397cac02b060ff1251afaa78087d92d7034369afbc8eb765631cada8257 AS python-donor

ARG GRX_PYTHON_MINOR

# Sanity check: the donor stage MUST ship the libpython.so.1.0 the
# rust-builder linked against. A version skew here breaks the runtime
# image silently (the binary loads but Py_InitializeEx fails).
RUN test -f "/usr/local/lib/libpython${GRX_PYTHON_MINOR}.so.1.0" \
        || { echo "libpython${GRX_PYTHON_MINOR}.so.1.0 missing in donor stage" >&2; exit 1; } \
    && test -d "/usr/local/lib/python${GRX_PYTHON_MINOR}" \
        || { echo "stdlib /usr/local/lib/python${GRX_PYTHON_MINOR} missing in donor stage" >&2; exit 1; }

# ============================================================================
# Stage 4 — final: distroless/cc-debian12 + grx + libpython + stdlib
# ============================================================================
#
# distroless/cc-debian12 ships:
#   - glibc, libgcc_s (cpython embed needs both)
#   - /etc/ssl/certs/ca-certificates.crt (rustls-native-certs)
#   - /etc/passwd with a `nonroot` (UID 65532) account
#
# It does NOT ship a shell, package manager, or any python runtime.
# We COPY the binary + libpython + stdlib from earlier stages and run
# as the `nonroot` user.

# Base: gcr.io/distroless/cc-debian12:nonroot (resolved 2026-06-18 via
#   docker buildx imagetools inspect gcr.io/distroless/cc-debian12:nonroot
# Multi-arch index digest. The `:nonroot` tag is rolled upstream without
# notice; the pinned digest captures a specific image at resolution
# time. Re-pin via the same command when bumping the runtime base.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:b0ae8e989418b458e0f25489bc3be523718938a2b70864cc0f6a00af1ddbd985 AS final

ARG GRX_PYTHON_MINOR

# rust-builder's binary. Single file, statically linked against the
# zig bridges; libpython is loaded dynamically via the rpath bake in
# `crates/graphreact-cli/build.rs`.
COPY --from=rust-builder /work/target/release/grx /usr/local/bin/grx

# Donor's libpython + stdlib. The rpath baked into the grx binary
# points at the pkg-config libdir from the rust-builder stage — which
# in python:slim is `/usr/local/lib`. Layout MUST match between the
# donor and the rpath bake. If you swap python-donor for `debian-slim
# + python3.X`, the libdir becomes `/usr/lib/x86_64-linux-gnu` and the
# rpath bake on the binary stops resolving.
COPY --from=python-donor /usr/local/lib/libpython${GRX_PYTHON_MINOR}.so.1.0 /usr/local/lib/
COPY --from=python-donor /usr/local/lib/python${GRX_PYTHON_MINOR} /usr/local/lib/python${GRX_PYTHON_MINOR}

# OCI image labels. `org.opencontainers.image.*` is the canonical
# label namespace consumers (ghcr.io, Docker Hub) render in their UI.
LABEL org.opencontainers.image.title="grx" \
      org.opencontainers.image.description="GraphReact backend runtime (`grx` CLI) — multi-engine .grx server." \
      org.opencontainers.image.source="https://github.com/graphreact/graphreact" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.documentation="https://github.com/graphreact/graphreact/blob/main/docs/backend/README.md"

# Run as the distroless `nonroot` UID (65532). The helm chart layers
# the same posture via `securityContext: { runAsNonRoot: true,
# readOnlyRootFilesystem: true }`.
USER nonroot:nonroot

# `grx --help` is the safest default — operators MUST override at
# `docker run` time (`docker run grx run /path/to/system.grx` or via
# the helm chart's `runtime.args`). We deliberately do NOT default
# to `grx cluster daemon` because that would require operators to
# accept the implicit observer bind / TLS / drain budgets without
# opt-in. The helm chart wires those via `args:`.
ENTRYPOINT ["grx"]
CMD ["--help"]

# No EXPOSE — operators map `--observer-bind` themselves via the
# helm chart's `observer.bind` value or the daemon's CLI flag.
