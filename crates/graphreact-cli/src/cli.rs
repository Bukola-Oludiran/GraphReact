//! `grx` CLI argument parsing.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::debug::DebugFlag;

#[derive(Debug, Parser)]
#[command(
    name = "grx",
    version,
    about = "Standalone CLI for the GraphReact runtime",
    long_about = "Standalone CLI for the GraphReact runtime.\n\n\
                  Parses and runs `.grx` system descriptions. Engine\n\
                  selection (JSC / null / future) controlled via\n\
                  `--runtime`. Subcommands cover end-to-end execution\n\
                  (`run`), validation (`check`), introspection\n\
                  (`parse`, `fmt`), and version reporting."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a `.grx` file end-to-end.
    Run {
        /// Path to the `.grx` file (root of any multi-file imports).
        path: PathBuf,
        /// Engine to use. Default `null` until JSC lands in M12.3.
        #[arg(long, default_value_t = RuntimeArg::Null)]
        runtime: RuntimeArg,
        /// **M15 Phase 9.** Watch the `.grx` source plus its imports
        /// for changes and auto-reload via the hot-reload pipeline
        /// (file watcher → diff classifier → engine adapter →
        /// orchestrator). Compatible body changes preserve `useState`;
        /// incompatible changes drain plus remount; rollback fires on
        /// first-tick failure of new code.
        ///
        /// Requires `--runtime jsc` or `--runtime cpython` — the null
        /// and echo engines have no body to swap.
        #[arg(long, default_value_t = false)]
        watch: bool,
        /// **M16-Backend P8.** Open a per-body source-debug listener
        /// in the engine matching `<engine>` and bind on
        /// `127.0.0.1:<port>`. P8 supports `cpython:<port>` (debugpy);
        /// P9 will add `jsc:<port>` (Chrome DevTools / Safari Web
        /// Inspector). The engine in `--debug` MUST match `--runtime`;
        /// mismatched values are rejected at dispatch time.
        ///
        /// By default the daemon waits for the IDE to attach before
        /// running the first batch (so breakpoints set in `def
        /// init(ctx)` actually fire). Pass `--debug-no-wait` to skip
        /// the wait and attach later via `debugpy.connect()`.
        ///
        /// Default ports: `cpython` → 5678, `jsc` → 9229.
        #[arg(long, value_parser = clap::value_parser!(DebugFlag))]
        debug: Option<DebugFlag>,
        /// **M16-Backend P8.** Skip `debugpy.wait_for_client()` so
        /// the daemon starts running before the IDE attaches.
        /// Useful for "attach later" workflows; default behavior
        /// blocks until the IDE attaches so breakpoints in `def
        /// init(ctx)` fire on first run.
        #[arg(long, default_value_t = false, requires = "debug")]
        debug_no_wait: bool,
    },
    /// Parse + validate a `.grx` file without executing anything.
    /// Reports parse and import errors. TS-CLI parity for `grx check`.
    Check {
        path: PathBuf,
    },
    /// Print the parser AST or built IR.
    ///
    /// Replaces TS's `grx build` (which transpiled `.grx` → `.ts` for
    /// the TS runtime); the Rust runtime executes `.grx` directly via
    /// the engine, so the equivalent debug command is to dump the IR.
    Parse {
        path: PathBuf,
        /// Output JSON (default: pretty text summary).
        #[arg(long)]
        json: bool,
        /// Dump the AST instead of the IR.
        #[arg(long)]
        ast: bool,
    },
    /// Pretty-print a `.grx` file in canonical form. Idempotent.
    Fmt {
        path: PathBuf,
        /// Print to stdout instead of writing the file in place.
        #[arg(long)]
        check: bool,
    },
    /// Print engine + parser + CLI versions.
    Version,
    /// Generate shell completion script for the chosen shell.
    ///
    /// Output goes to stdout; redirect to your shell's completion
    /// directory or source it directly. Example:
    /// `grx completions zsh > ~/.zsh/completions/_grx`
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },
    /// Scaffold a new `.grx` project.
    ///
    /// Creates `<path>/` containing a hello-world `.grx` file plus a
    /// short README. Non-interactive: pick a template via `--template`.
    Init {
        /// Directory to create. Must not already exist.
        path: PathBuf,
        /// Template to scaffold. Defaults to `hello`.
        #[arg(long, default_value = "hello")]
        template: InitTemplate,
    },
    /// **M15 Phase 9 stub.** Reload a `.grx` file against a running
    /// daemon over the management API.
    ///
    /// Status: the remote-daemon variant is gated on M16-Backend's
    /// management API plus auth surface — see `docs/backend/m15-hot-reload.md`.
    /// For the in-process variant, use `grx run --watch <path>`,
    /// which integrates the same hot-reload pipeline (file watcher
    /// → diff classifier → engine adapter → orchestrator) and
    /// auto-reloads on every save.
    ///
    /// Today this subcommand prints the redirect message and exits
    /// nonzero so scripts that expect remote-reload semantics fail
    /// loudly rather than silently no-op.
    Reload {
        /// Path to the `.grx` file the user wanted to reload remotely.
        path: PathBuf,
    },
    /// Cluster operations — distributed runtime entry points (M14.8).
    ///
    /// Subcommands are namespaced under `grx cluster …` so they don't
    /// collide with single-process commands like `run` / `check`. The
    /// payload is boxed because [`ClusterOp`] carries large clap-
    /// derived option blocks; without the box `clippy::large_enum_variant`
    /// would force every other [`Command`] variant to allocate the
    /// same footprint.
    Cluster {
        #[command(subcommand)]
        op: Box<ClusterOp>,
    },
    /// **M16-Backend P5.** List the nodes registered in a running
    /// daemon. Hits `GET /v1/nodes` on the management API.
    Ps {
        #[command(flatten)]
        target: ObserverTarget,
        /// Emit one JSON object per line for `jq`/script consumption.
        /// Default emits a fixed-width text table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// **M16-Backend P5.** Print a one-shot ASCII supervisor tree
    /// for a running daemon. Hits `GET /v1/supervisor/tree`.
    Tree {
        #[command(flatten)]
        target: ObserverTarget,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// **M16-Backend P5.** htop-shaped live refresh of node + queue
    /// state. Polls the management API on a 1Hz cadence; clears
    /// stdout each frame via ANSI control sequences. Press Ctrl-C
    /// to exit.
    Top {
        #[command(flatten)]
        target: ObserverTarget,
        /// Refresh interval in seconds (clamped to 1..30).
        #[arg(long, default_value_t = 1)]
        interval: u64,
    },
    /// **M16-Backend P5.** NDJSON-formatted line-mode follower tailing
    /// the recent-messages ring of one node. Polls the management
    /// API on a configurable cadence; emits new entries since the
    /// last poll. Pipe through `jq` for filtering.
    Trace {
        #[command(flatten)]
        target: ObserverTarget,
        /// Node ID to tail.
        node_id: String,
        /// Initial limit (max ring snapshot to fetch). Default 64
        /// matches the daemon's ring capacity.
        #[arg(long, default_value_t = 64)]
        limit: usize,
        /// Poll cadence in milliseconds.
        #[arg(long, default_value_t = 500)]
        poll_ms: u64,
    },
    /// **M16-Backend P6.** htop/k9s-class live introspection terminal
    /// UI. Polls the management API on a 10 Hz cadence and renders
    /// the supervisor tree, per-node message rates, queue depths,
    /// and per-node trace. Pane state machine: Overview / Tree /
    /// Nodes / Node detail / Trace.
    ///
    /// Tab cycles panes; h/j/k/l navigates; Enter drills into the
    /// selected node; Esc returns; q quits. On daemon restart
    /// (`process_start_id` change), the tape clears and a
    /// `[target restarted]` divider appears.
    ///
    /// Polling-only fallback ships in P6; WebSocket subscription
    /// upgrade lands in P7. Reconnect logic (exponential backoff
    /// up to 30s, identity-change detection) carries forward.
    Observer {
        #[command(flatten)]
        target: TlsObserverTarget,
    },
    /// **M-Prod sub-track #3 (Phase A).** Operator-facing secret
    /// store. Backs developer + ops workflows that need to set /
    /// retrieve / list / remove credentials without putting them in
    /// `.grx` files or environment-variable scrollback.
    ///
    /// Two backends ship in Phase A: `keyring` (default; macOS
    /// Keychain / Linux Secret Service / Windows Credential Manager
    /// via the `keyring` crate) and `env` (reads `GRX_SECRET_<NAME>`
    /// environment variables; the documented production path for
    /// headless containers / k8s where no OS keyring agent is
    /// running). The Phase B runtime accessor (`ctx.secret(name)`
    /// inside `.grx` bodies) is deferred — see
    /// `docs/backend/m-prod-secrets.md`.
    Secrets {
        #[command(subcommand)]
        op: SecretsOp,
    },
    /// **M-Prod sub-track #1 Phase A.** Inspect the hash-chained
    /// audit log produced by `--observer-audit-log`. Phase A ships
    /// one operation:
    ///
    /// - `grx audit verify <PATH>` — walks every NDJSON line in
    ///   the file, recomputes each record's SHA-256, verifies the
    ///   chain links, and prints either `audit chain OK: <N>
    ///   records` (exit 0) or a structured tamper / mismatch
    ///   message (exit 1). Operators wire this into CI / audit
    ///   review workflows alongside `journalctl -u grx --grep
    ///   'graphreact::observer::audit'`.
    Audit {
        #[command(subcommand)]
        op: AuditOp,
    },
    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Operator
    /// admin surface for a running daemon. Today's surface:
    /// - `grx admin reload-tokens` — fire SIGHUP at a target daemon
    ///   PID so it re-reads the on-disk token-registry file
    ///   (`--observer-tokens`) and atomic-swaps the in-memory
    ///   [`graphreact_observer::RotatableRegistry`]. The same
    ///   SIGHUP also fires the audit-log rotation + the TLS cert
    ///   reload — operators get one signal, three reload targets,
    ///   fail-isolated.
    Admin {
        #[command(subcommand)]
        op: AdminOp,
    },
}

/// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Sub-subcommand
/// block under `grx admin <op>`.
#[derive(Debug, clap::Subcommand)]
pub enum AdminOp {
    /// Send SIGHUP to a running daemon PID so it reloads the
    /// on-disk token registry file (and rotates the audit log +
    /// reloads TLS certs in the same fan-out). The CLI is a thin
    /// signal wrapper — exit 0 on successful `kill(2)`, exit 1
    /// otherwise. Operators wait for a follow-up audit row of
    /// shape `token_registry_reloaded` / `token_registry_reload_failed`
    /// to know the in-process outcome.
    ReloadTokens {
        /// PID of the daemon to signal. Mutually exclusive with
        /// `--pid-file`.
        #[arg(long, conflicts_with = "pid_file")]
        pid: Option<u32>,
        /// File containing the daemon PID (one integer, optional
        /// trailing newline). Mutually exclusive with `--pid`.
        #[arg(long, conflicts_with = "pid")]
        pid_file: Option<PathBuf>,
    },
}

/// **M-Prod sub-track #1 Phase A.** Sub-subcommand block under
/// `grx audit <op>`. Today's surface is the chain verifier; Phase B
/// adds replay / inspect / report operations.
#[derive(Debug, clap::Subcommand)]
pub enum AuditOp {
    /// Verify the hash chain in the NDJSON file at `<PATH>`.
    /// Prints `audit chain OK: <N> records` on success (exit 0)
    /// or a structured error message on the first violation
    /// (exit 1). The file is opened read-only — never mutated.
    Verify {
        /// Path to the NDJSON audit log to verify.
        path: PathBuf,
    },
}

/// **M-Prod sub-track #3.** Sub-subcommand block under
/// `grx secrets <op>`. Each leaf carries the secret name plus a
/// shared [`SecretsBackend`] flag picking the backend (defaults to
/// `keyring`). The `Set` variant reads the plaintext from a
/// zeroized stdin prompt — clients MUST NOT pass the value as a
/// CLI argument because `ps`/shell-history would leak it.
#[derive(Debug, clap::Subcommand)]
pub enum SecretsOp {
    /// Write a secret. Reads the plaintext from stdin (TTY-detected;
    /// a piped stdin works too but emits a stderr warning that the
    /// caller is responsible for not having logged the value).
    /// Existing entries are overwritten (upsert).
    Set {
        /// Secret name. Must match `[A-Z][A-Z0-9_]*` so the entry
        /// round-trips identically across the keyring + env
        /// backends. Validated in `graphreact-secrets::validate_secret_name`.
        name: String,
        #[arg(long, default_value_t = SecretsBackend::Keyring)]
        backend: SecretsBackend,
    },
    /// Read a secret. Writes the raw bytes to stdout WITHOUT a
    /// trailing newline so callers can compose with
    /// `--token-file <(grx secrets get FOO)`. Exit code 1 if the
    /// secret is absent (matches Unix `cat`-style ergonomics for
    /// missing files).
    Get {
        /// Secret name (see `Set` for validation rules).
        name: String,
        #[arg(long, default_value_t = SecretsBackend::Keyring)]
        backend: SecretsBackend,
    },
    /// List the secret names visible to this backend. Names ONLY —
    /// never values. Sorted ascending. The keyring backend returns
    /// an empty vec on Phase A (the `keyring` crate's enumeration
    /// surface isn't uniform across platforms); switch to
    /// `--backend env` to enumerate.
    List {
        #[arg(long, default_value_t = SecretsBackend::Keyring)]
        backend: SecretsBackend,
    },
    /// Remove a secret. Idempotent — removing an absent entry
    /// succeeds with exit code 0 so `grx secrets remove FOO` is
    /// safe to re-run.
    Remove {
        /// Secret name (see `Set` for validation rules).
        name: String,
        #[arg(long, default_value_t = SecretsBackend::Keyring)]
        backend: SecretsBackend,
    },
}

/// **M-Prod sub-track #3.** Which backend `grx secrets` should
/// dispatch through. Defaults to `keyring` because that's the
/// developer-workstation path; containers / k8s deploys override
/// to `env`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum SecretsBackend {
    /// OS keyring (Keychain / Secret Service / Credential Manager).
    Keyring,
    /// `GRX_SECRET_<NAME>` env vars (process-local).
    Env,
}

impl std::fmt::Display for SecretsBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keyring => write!(f, "keyring"),
            Self::Env => write!(f, "env"),
        }
    }
}

/// Reusable target + auth flag block for `grx ps/tree/top/trace`.
#[derive(Debug, clap::Args)]
pub struct ObserverTarget {
    /// Daemon's management API target (`host:port` or `https://host:port`).
    /// Must point at the same address `--observer-bind` uses.
    #[arg(long, default_value = "127.0.0.1:9090")]
    pub target: String,
    /// Read the bearer token from this env var.
    #[arg(long, conflicts_with = "token_file")]
    pub token_env: Option<String>,
    /// Read the bearer token from this file (mode 0600 enforced
    /// on Unix). Mutually exclusive with `--token-env`.
    #[arg(long, conflicts_with = "token_env")]
    pub token_file: Option<PathBuf>,
}

/// `grx observer` target block. Extends [`ObserverTarget`] with the
/// `--insecure-no-tls` dev-only opt-out for skipping cert verification
/// when scraping a daemon that's bound on a self-signed cert. **DO
/// NOT** use in production deploys.
#[derive(Debug, clap::Args)]
pub struct TlsObserverTarget {
    /// Daemon's management API target (`host:port` or `https://host:port`).
    /// `https://` selects the rustls TLS path; plain `host:port` uses HTTP.
    #[arg(long, default_value = "127.0.0.1:9090")]
    pub target: String,
    /// Read the bearer token from this env var.
    #[arg(long, conflicts_with = "token_file")]
    pub token_env: Option<String>,
    /// Read the bearer token from this file (mode 0600 enforced
    /// on Unix). Mutually exclusive with `--token-env`.
    #[arg(long, conflicts_with = "token_env")]
    pub token_file: Option<PathBuf>,
    /// **DEV ONLY.** Skip TLS peer certificate verification. Useful
    /// when scraping a daemon bound on a self-signed cert; refused
    /// in production deploys (must use a real CA-signed cert).
    #[arg(long, default_value_t = false)]
    pub insecure_no_tls: bool,
}

/// Cluster subcommands. M14.8.
///
/// `clippy::large_enum_variant` allowed because the `Daemon` variant
/// is genuinely the heaviest CLI command (M16-Backend P2/P4/P5/P6/P7/P8/P10
/// all extend it with optional flags) and boxing it would force every
/// `ClusterOp::Daemon { .. }` pattern-match site to add `..` indirection.
/// The enum is constructed once per CLI invocation and discarded; the
/// memory cost is negligible.
#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum ClusterOp {
    /// Start a clustered daemon. Mounts the optional `.grx` file
    /// against the chosen engine, boots a `Cluster` listener,
    /// optionally dials a list of peers, and runs the scheduler in
    /// dist-aware mode until the supplied duration elapses (or
    /// until SIGINT for the unset case).
    Daemon {
        /// `.grx` file to mount. Optional — when absent the daemon
        /// runs an empty cluster, useful as a relay or for tests.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Cluster identity, parsed as `name@host` (e.g.
        /// `alice@127.0.0.1`). Carried in the OTP-style handshake;
        /// peers reference this exact form.
        #[arg(long)]
        node_id: String,
        /// TCP socket the listener binds to. Use `127.0.0.1:0` in
        /// tests for an ephemeral port; use a fixed port in
        /// production.
        #[arg(long)]
        listen: SocketAddr,
        /// Read the cluster cookie from this environment variable.
        /// Mutually exclusive with `--cookie-file`.
        #[arg(long, conflicts_with = "cookie_file")]
        cookie_env: Option<String>,
        /// Read the cluster cookie from this file. The file must
        /// be mode 0600 (rw-------) on Unix; world- or
        /// group-readable files are refused at load. Mutually
        /// exclusive with `--cookie-env`.
        #[arg(long, conflicts_with = "cookie_env")]
        cookie_file: Option<PathBuf>,
        /// PEM-encoded leaf cert chain for this node's identity.
        #[arg(long)]
        cert: PathBuf,
        /// PEM-encoded private key matching `--cert`.
        #[arg(long)]
        key: PathBuf,
        /// PEM-encoded trust anchor (typically the cluster's CA cert).
        #[arg(long)]
        ca: PathBuf,
        /// Comma-separated peer list. Each entry parses as
        /// `name@host:port` — host:port doubles as the cluster
        /// identity (NodeId) AND the dial address (SocketAddr).
        #[arg(long, value_delimiter = ',')]
        peers: Vec<String>,
        /// Engine to mount the `.grx` file against. Defaults to
        /// `null` for engine-free relay daemons.
        #[arg(long, default_value_t = RuntimeArg::Null)]
        runtime: RuntimeArg,
        /// Exit after this many seconds. For tests + scripted
        /// shutdowns; absent = run until SIGINT.
        #[arg(long)]
        duration_secs: Option<u64>,
        /// **M16-Backend P2.** Bind the observer HTTP server
        /// (`/metrics` + `/healthz`) on this socket. Default unset:
        /// no observer port bound, zero per-event overhead. Pass
        /// `127.0.0.1:9090` for a Prometheus-compatible loopback
        /// scrape endpoint; any non-loopback address ALSO requires
        /// `--observer-token-{env|file}` AND `--observer-cert` AND
        /// `--observer-key` together.
        #[arg(long)]
        observer_bind: Option<SocketAddr>,
        /// Read the observer bearer token from this env var.
        /// Mutually exclusive with `--observer-token-file`. Required
        /// when `--observer-bind` names a non-loopback address.
        #[arg(long, conflicts_with = "observer_token_file")]
        observer_token_env: Option<String>,
        /// Read the observer bearer token from this file. Refuses
        /// to load the file if it's group-/world-readable on Unix
        /// (mode 0600 enforcement). Mutually exclusive with
        /// `--observer-token-env`.
        #[arg(long, conflicts_with = "observer_token_env")]
        observer_token_file: Option<PathBuf>,
        /// PEM-encoded leaf cert for observer TLS. Required for
        /// any non-loopback `--observer-bind`.
        #[arg(long)]
        observer_cert: Option<PathBuf>,
        /// PEM-encoded private key matching `--observer-cert`.
        #[arg(long)]
        observer_key: Option<PathBuf>,
        /// **M16-Backend P3.** Install the unified JSON-log
        /// subscriber so `tracing::*` events (P2 audit events,
        /// scheduler supervisor escalations, dist reload events)
        /// emit as single-line JSON to stderr. Off by default
        /// because consuming processes may already have their own
        /// `tracing-subscriber` setup; setting this flag would
        /// then conflict.
        #[arg(long, default_value_t = false)]
        observer_json_logs: bool,
        /// **M-Prod sub-track #1 Phase A.** Write a hash-chained
        /// tamper-evident audit log to this NDJSON file. One
        /// record per bind / accept-fail / auth-fail /
        /// TLS-rotation event with SHA-256 prev/this chain links.
        /// The pre-existing `target="graphreact::observer::audit"`
        /// tracing stream STILL fires bit-for-bit unchanged — this
        /// flag is strictly additive.
        ///
        /// The sink fsyncs (sync_data) after every append in Phase
        /// A, so high-volume `auth_fail` floods will pay a per-
        /// request disk-sync cost. Phase B switches to a worker-
        /// task + batched-fsync model. Operators rotate the file
        /// out-of-band by renaming + restarting the daemon
        /// (Phase A intentionally refuses SIGHUP; in-process
        /// rotation would break the chain).
        ///
        /// Verify the chain at any time via `grx audit verify
        /// <PATH>`. Mode 0600 on Unix is recommended on the
        /// containing directory; the file itself is opened with
        /// the process's umask.
        #[arg(long)]
        observer_audit_log: Option<PathBuf>,
        /// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Path
        /// to an on-disk multi-token RBAC registry (NDJSON one
        /// record per line; see ADR-0009 for the schema). When set,
        /// the daemon dispatches every protected request through
        /// [`graphreact_observer::RotatableRegistry`] + the per-role
        /// ACL flow shipped in Phase B Part 1, replacing the
        /// single-bearer `--observer-token-{env,file}` path.
        ///
        /// File mode 0o600 is enforced on Unix. SIGHUP
        /// (`kill -HUP <pid>` or `grx admin reload-tokens --pid
        /// <pid>`) re-reads the file + atomic-swaps the in-memory
        /// registry without restarting the daemon. On parse /
        /// validate failure the prior registry stays serving + an
        /// audit event of shape `token_registry_reload_failed` is
        /// emitted.
        #[arg(long)]
        observer_tokens: Option<PathBuf>,
        /// **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** Path
        /// to a JWKS JSON file (RFC 7517 §5) — the OIDC IdP's
        /// public signing keys for the JWT-OIDC bridge. Required
        /// alongside `--observer-jwt-config`. The two flags ship
        /// together; the daemon fails fast if only one is set.
        ///
        /// File mode is NOT enforced (JWKS keys are public by
        /// convention; ADR-0010 documents the deliberate
        /// deviation). SIGHUP re-reads the JWKS file + atomic-
        /// swaps the in-memory store without restarting the
        /// daemon. On parse / validate failure the prior store
        /// stays serving + a `jwks_reload_failed` audit row
        /// emits.
        #[arg(long)]
        observer_jwks: Option<PathBuf>,
        /// **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** Path
        /// to the JWT-OIDC bridge config JSON (acceptable issuers,
        /// audiences, algorithm whitelist, leeway, role mapping).
        /// File mode 0o600 IS enforced on Unix — the role mapping
        /// determines who becomes admin, so the file gets the
        /// same filesystem protection as the static-token
        /// registry.
        ///
        /// Config changes require a daemon restart; SIGHUP
        /// reloads the JWKS file only. When set, requires
        /// `--observer-jwks` — the JWT bridge needs both halves.
        ///
        /// Bridges external OIDC IdP-issued JWTs (Okta / Auth0 /
        /// Keycloak / AWS Cognito / Google etc.) into the multi-
        /// token RBAC dispatch by mapping JWT claims onto the
        /// existing Role::Admin / Role::ReadOnly / Role::Custom
        /// vocabulary. eyJ-prefixed bearers route through JWT
        /// verify FIRST; non-eyJ bearers dispatch through the
        /// existing static-token walk unchanged.
        #[arg(long, requires = "observer_jwks")]
        observer_jwt_config: Option<PathBuf>,
        /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Path
        /// to an audit-log forwarder spool file. When set, every
        /// fully-stamped audit record (the in-process audit log's
        /// AuditRecord shape) is ALSO written as NDJSON to the
        /// spool with mode 0o600 enforcement on Unix. Sidecars
        /// (fluentd / vector / promtail / fluent-bit /
        /// opentelemetry-collector / aws-for-fluent-bit /
        /// google-cloud-ops-agent) tail the spool and ship to S3 /
        /// Loki / GCS / OpenSearch / Splunk / Datadog without the
        /// daemon ever needing its own outbound HTTP path.
        ///
        /// SIGHUP rotates the spool atomically (renames to
        /// `<spool>.YYYY-MM-DDTHH-MM-SS-Z` + reopens a fresh file
        /// with 0o600). The chain markers ride via the in-process
        /// `audit_log_rotated` records that are forwarded through
        /// the normal fan-out.
        #[arg(long)]
        observer_audit_forwarder_spool: Option<PathBuf>,
        /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** When
        /// set, write every fully-stamped audit record as a JSON
        /// line to the daemon's stderr stream. Kubernetes-native:
        /// every node runs a log-collector DaemonSet (fluentd /
        /// fluent-bit / vector / opentelemetry-collector /
        /// aws-for-fluent-bit / google-cloud-ops-agent) that tails
        /// container stderr and ships to any backend (S3 / Loki /
        /// GCS / OpenSearch / Splunk / Datadog) — zero-config for
        /// the k8s-shaped operator path.
        #[arg(long, default_value_t = false)]
        observer_audit_forwarder_stderr: bool,
        /// **M16-Backend P10.** Export OpenTelemetry traces to the
        /// given OTLP/gRPC endpoint (e.g. `http://localhost:4317`
        /// for a local OTel collector). When set, the daemon
        /// constructs a `graphreact_observer::OtelSink` and
        /// installs it as both a `TelemetrySink` (for span
        /// emission) AND a `WireTracingHook` (for cross-node OTel
        /// span linkage via `wire::Message::trace_context`). The
        /// daemon advertises the `observer-otel-v1` capability in
        /// its handshake; peers with the matching capability
        /// participate in cross-cluster traces. Requires the
        /// `otel` Cargo feature on `graphreact-cli`; without it,
        /// the flag returns an error at dispatch.
        #[arg(long)]
        observer_otel_endpoint: Option<String>,
        /// **M16-Backend P10.** OTel resource attribute
        /// `service.name` for spans emitted via
        /// `--observer-otel-endpoint`. Defaults to `"graphreact"`.
        /// Per the OTel SDK convention, can also be set via the
        /// `OTEL_SERVICE_NAME` environment variable.
        #[arg(long, requires = "observer_otel_endpoint")]
        observer_otel_service_name: Option<String>,
        /// **M16-Backend P8.** Open a per-body source-debug listener
        /// in the engine matching `<engine>` (must match `--runtime`).
        /// P8 supports `cpython:<port>` (debugpy); P9 will add
        /// `jsc:<port>`. Default ports: `cpython` → 5678, `jsc` →
        /// 9229. See the `Run` variant docs for full semantics.
        #[arg(long, value_parser = clap::value_parser!(DebugFlag))]
        debug: Option<DebugFlag>,
        /// **M16-Backend P8.** Skip `debugpy.wait_for_client()` so
        /// the daemon starts running before the IDE attaches.
        #[arg(long, default_value_t = false, requires = "debug")]
        debug_no_wait: bool,
        /// **M-Prod sub-track #4 Phase α.** Outer SIGTERM drain
        /// deadline in seconds. On SIGTERM the daemon flips
        /// `/v1/readyz` to 503 `draining`, refuses new ingress
        /// POSTs with the same body, and waits up to this many
        /// seconds for in-flight requests to settle before
        /// running the existing OTel + cluster + engine teardown.
        ///
        /// Budget math vs k8s `terminationGracePeriodSeconds`:
        /// drain + `OBSERVER_SHUTDOWN_DEADLINE` + slack must be ≤
        /// `terminationGracePeriodSeconds`. Default 20s drain +
        /// default 5s OTel + 5s slack = 30s (k8s default).
        /// Operators raising either bound MUST raise
        /// `terminationGracePeriodSeconds` accordingly or risk
        /// SIGKILL-truncated audit logs.
        ///
        /// Falls through to the `GRX_DRAIN_DEADLINE_SECS` env var
        /// when absent; both ≤0 / unparseable fall back to 20s.
        /// SIGTERM is unix-only; on Windows the daemon falls back
        /// to ctrl_c-as-snap-shutdown (no drain phase).
        #[arg(long)]
        drain_deadline_secs: Option<u64>,
    },
    /// One-shot peer dial. Spins up a minimal cluster (no `.grx`
    /// mount), opens a TLS connection to the named peer, and
    /// stays alive for `--duration-secs` (or until SIGINT).
    Connect {
        /// Peer to dial, in `name@host:port` form. The host:port
        /// suffix doubles as the dial address; the full string is
        /// the NodeId the handshake validates against.
        peer: String,
        /// This connector's own cluster identity. Mirrors the
        /// daemon's `--node-id`.
        #[arg(long)]
        node_id: String,
        /// Listen address. Even though this is a one-shot dial,
        /// the cluster requires a listener for incoming
        /// reciprocal traffic. Use `127.0.0.1:0` for an ephemeral
        /// port in tests.
        #[arg(long)]
        listen: SocketAddr,
        #[arg(long, conflicts_with = "cookie_file")]
        cookie_env: Option<String>,
        #[arg(long, conflicts_with = "cookie_env")]
        cookie_file: Option<PathBuf>,
        #[arg(long)]
        cert: PathBuf,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        ca: PathBuf,
        /// Exit after this many seconds. Absent = run until
        /// SIGINT (symmetric with `daemon`); useful for hosting
        /// the connector as a long-running cluster member.
        /// Specify a value for tests + scripted runs.
        #[arg(long)]
        duration_secs: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum InitTemplate {
    /// Single-component "hello world" — one node that emits a message
    /// on mount and the runtime logs it. Smallest possible `.grx`.
    Hello,
    /// Two-node ping-pong example. Two components send messages to
    /// each other; mirrors the `ping-pong.grx` integration fixture.
    PingPong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum RuntimeArg {
    Null,
    /// Reference engine that crosses the Rust↔native FFI boundary
    /// (C today, Zig in M12.3) and validates marshalling.
    Echo,
    Jsc,
    /// CPython engine (M13-phase1+2). Real eval/resolve/call against
    /// embedded Python sub-interpreters; callbacks + hook semantics
    /// land at M13-phase2.5+.
    Cpython,
    /// Go engine (M17-phase1+2+3a+3b SHIPPED). Loads a cgo +
    /// c-shared artifact produced by `graphreact-codegen-go` +
    /// `go build -buildmode=c-shared`. The bare `--runtime=go` path
    /// constructs a `GoEngine` without an artifact wired; production
    /// use should route through `goruntime::GoRuntime::build_and_load`
    /// instead. Full hook surface (M6.6 parity) lands at M17 phase
    /// 3c-h per the later-milestone plan.
    Go,
    /// **M22 — Mixed-runtime daemon.** Brings up every engine
    /// (JSC + CPython + Go) whose `lang` appears in the `.grx`
    /// IR. Engines whose lang is absent are not initialized so
    /// pure-lang fixtures pay no cross-engine cost. Each engine
    /// owns its own scheduler; per-component dispatch is closure-
    /// based via the relaxed-mount path shipped in M22 phase 1.
    /// `--debug` is currently rejected under `--runtime=mixed`
    /// (debug attach picks one engine; multi-engine attach is
    /// deferred to M22 phase 5+).
    Mixed,
}

impl std::fmt::Display for RuntimeArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::Echo => write!(f, "echo"),
            Self::Jsc => write!(f, "jsc"),
            Self::Cpython => write!(f, "cpython"),
            Self::Go => write!(f, "go"),
            Self::Mixed => write!(f, "mixed"),
        }
    }
}
