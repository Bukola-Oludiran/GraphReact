# Homebrew formula scaffold for `grx`.
#
# **M-Prod sub-track #8 Phase A scope:** this file is a build-instructions
# REFERENCE — it is NOT yet submitted to homebrew-core. Phase A operators
# building from source on macOS use this file as the canonical recipe
# (`brew install --build-from-source path/to/grx.rb`); Phase B submits
# a polished version to a `graphreact/homebrew-tap` repo + later to
# homebrew-core.
#
# Why bottle:unneeded for Phase A:
#   - We have NOT yet stood up a `brew bottle` pipeline against macOS
#     {13,14,15} runners. Until then, bottles would be locally-built
#     and unverified.
#   - `bottle :unneeded` (rather than `bottle :none`) tells Homebrew the
#     formula is reproducible from source and a bottle is not required;
#     this is the canonical Rust-formula posture during the early-tap
#     phase (mirrors what `bat`, `ripgrep`, `eza` shipped before
#     homebrew-core ingest).
#
# Cross-references:
#   - https://docs.brew.sh/Formula-Cookbook
#   - docs/backend/m-prod-packaging.md — Phase A→B→C plan for distribution
#   - docs/backend/m-prod-runbook.md — `Verify a release` operator flow

class Grx < Formula
  desc "GraphReact backend runtime — multi-engine .grx server"
  homepage "https://github.com/graphreact/graphreact"
  # `url` MUST point at the per-tag source tarball — bump alongside
  # `version` on every release. Phase B's release.yml will inject the
  # right URL via `gh release create` + a downstream bump script.
  url "https://github.com/graphreact/graphreact/archive/refs/tags/v0.1.0.tar.gz"
  # `sha256` MUST be updated on every release. Compute via
  # `shasum -a 256 <tarball>` (matches the sha emitted by the
  # `reproducible-build` CI cell).
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"
  version "0.1.0"

  # Phase A: no bottle pipeline yet. Homebrew removed `bottle :unneeded`
  # and `bottle :none` in brew 3.6.0 (Sep 2022); modern formulae simply
  # omit the `bottle` block until a `bottle do ... end` is wired up.
  # See `Open work in #8` in docs/backend/m-prod-packaging.md.

  # Build-time deps:
  #
  #   - rust  — workspace targets MSRV 1.83 (set in workspace Cargo.toml
  #             via rust-version = "1.83" and rust-toolchain.toml).
  #             Homebrew's `rust` formula tracks stable.
  #   - zig   — `cargo xtask zig-build --release` produces the static
  #             .a bridge libs (per `xtask/src/main.rs` lines 132+).
  #             zig 0.16+ is required (xtask probes `zig version`).
  #   - pkg-config — required by both the zig-build's pkg-config probe
  #             AND `engine-cpython/build.rs`.
  depends_on "rust" => :build
  depends_on "zig" => :build
  depends_on "pkg-config" => :build

  # Runtime deps:
  #
  #   - python@3.13 — `engine-cpython/build.rs` HARD-LINKS against
  #             pkg-config python-3.X-embed (Python 3.12+ floor enforced
  #             at link time). Homebrew's python@3.13 ships the .pc file
  #             at $(brew --prefix python@3.13)/lib/pkgconfig.
  #             Per-CPython-minor ABI: bumping the Python pin is a
  #             deliberate formula-version bump (mirrors the per-Python-
  #             minor image slice the helm chart documents).
  depends_on "python@3.13"

  def install
    # Point pkg-config at Homebrew's python@3.13 ahead of the system
    # python. Without this the build.rs probe finds the macOS Command
    # Line Tools python (which lacks an `-embed.pc`) and emits the
    # "Python 3.12+ floor" panic.
    py_prefix = Formula["python@3.13"].opt_prefix
    ENV.prepend_path "PKG_CONFIG_PATH", "#{py_prefix}/lib/pkgconfig"
    ENV["GRAPHREACT_PYTHON_PKGCONFIG"] = "python-3.13-embed"
    ENV["GRAPHREACT_PYTHON_VERSION"] = "3.13"

    # Mirror the CI repro cell's path-remap + SOURCE_DATE_EPOCH so a
    # `brew install --build-from-source` produces the same audit hash
    # as the GitHub Release's `reproducible-build-sha256-<tag>` artifact.
    ENV["SOURCE_DATE_EPOCH"] = `git -C #{cached_download} log -1 --pretty=%ct`.strip rescue nil
    ENV["RUSTFLAGS"] = "--remap-path-prefix=#{Dir.pwd}=. --remap-path-prefix=#{Dir.home}/.cargo=/cargo"

    # Stage 1: zig bridge static libs. xtask checks `zig version` and
    # writes .a files to zig/bridge/zig-out/lib/.
    system "cargo", "xtask", "zig-build", "--release"

    # Stage 2: Rust binary build. `--bin grx` produces target/release/grx.
    system "cargo", "build", "--release", "--bin", "grx"

    # Install into the Homebrew Cellar.
    bin.install "target/release/grx"

    # Shell completions are NOT yet emitted by the binary (Phase B will
    # ship `grx completions {bash,zsh,fish}`). When that lands, install
    # via:
    #
    #   bash_completion.install "completions/grx.bash"
    #   zsh_completion.install "completions/_grx"
    #   fish_completion.install "completions/grx.fish"
  end

  test do
    # Smoke test: `grx version` exits 0 and prints the version we shipped.
    assert_match version.to_s, shell_output("#{bin}/grx version")
  end
end
