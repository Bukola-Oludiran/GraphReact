//! `grx` binary entry point. Dispatches into
//! [`graphreact_cli::commands`].

use std::path::Path;
use std::process::ExitCode;

use clap::Parser;

use graphreact_cli::cli::{Cli, ClusterOp, Command};
use graphreact_cli::commands::{
    drive_handle_until, format_parse_error, load_tls_from_paths, parse_peer_entry,
    resolve_cookie, run_check, run_completions, run_fmt, run_init, run_parse, run_reload,
    run_run, run_run_watch, run_version, start_daemon, validate_debug_against_runtime,
    CommandError, ConnectOptions, DaemonOptions, FmtOutcome,
};
use graphreact_dist::{NetSplitPolicy, NetTickConfig, NodeId};

fn main() -> ExitCode {
    // Install a tokio runtime for the duration of the CLI invocation
    // so source factories that call `tokio::spawn` inside `start()`
    // (timer / delay / http) find a context. notify-backed watchSource
    // doesn't need this — it spawns its own thread — but co-locating
    // the runtime keeps source semantics uniform.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _enter = runtime.enter();

    let cli = Cli::parse();
    let path_for_err = associated_path(&cli.command).map(Path::to_path_buf);
    match dispatch(&runtime, cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", format_top_level_error(&err, path_for_err.as_deref()));
            ExitCode::FAILURE
        }
    }
}

fn dispatch(runtime: &tokio::runtime::Runtime, command: Command) -> Result<(), CommandError> {
    match command {
        Command::Run {
            path,
            runtime: runtime_arg,
            watch,
            debug,
            debug_no_wait,
        } => {
            let debug = validate_debug_against_runtime(debug, runtime_arg)?;
            if watch {
                run_run_watch(&path, runtime_arg, debug, !debug_no_wait)?;
            } else {
                let out = run_run(&path, runtime_arg, debug, !debug_no_wait)?;
                print!("{out}");
            }
        }
        Command::Check { path } => {
            let out = run_check(&path)?;
            print!("{out}");
        }
        Command::Parse { path, json, ast } => {
            let out = run_parse(&path, json, ast)?;
            println!("{out}");
        }
        Command::Fmt { path, check } => match run_fmt(&path, check)? {
            FmtOutcome::AlreadyFormatted => {
                println!("{} is already formatted", path.display());
            }
            FmtOutcome::WouldChange { formatted } => {
                println!("{formatted}");
                return Err(CommandError::Io(std::io::Error::other(
                    "file would be reformatted (re-run without --check to write)",
                )));
            }
            FmtOutcome::Wrote { .. } => {
                println!("formatted {}", path.display());
            }
        },
        Command::Version => {
            print!("{}", run_version()?);
        }
        Command::Completions { shell } => {
            print!("{}", run_completions(shell)?);
        }
        Command::Init { path, template } => {
            print!("{}", run_init(&path, template)?);
        }
        Command::Reload { path } => {
            // run_reload always returns Err today (stub redirect to
            // `grx run --watch`). The match arm exists so `Reload`
            // is exhaustive; the actual error message goes through
            // the binary's top-level error handler.
            let _ = run_reload(&path)?;
        }
        Command::Cluster { op } => {
            dispatch_cluster(runtime, *op)?;
        }
        Command::Ps { target, json } => {
            runtime.block_on(graphreact_cli::observer_commands::run_ps(target, json))?;
        }
        Command::Tree { target, json } => {
            runtime.block_on(graphreact_cli::observer_commands::run_tree(target, json))?;
        }
        Command::Top { target, interval } => {
            runtime.block_on(graphreact_cli::observer_commands::run_top(target, interval))?;
        }
        Command::Trace {
            target,
            node_id,
            limit,
            poll_ms,
        } => {
            runtime.block_on(graphreact_cli::observer_commands::run_trace(
                target, node_id, limit, poll_ms,
            ))?;
        }
        Command::Observer { target } => {
            runtime.block_on(graphreact_cli::observer_tui::run(target))?;
        }
    }
    Ok(())
}

fn dispatch_cluster(
    runtime: &tokio::runtime::Runtime,
    op: ClusterOp,
) -> Result<(), CommandError> {
    match op {
        ClusterOp::Daemon {
            path,
            node_id,
            listen,
            cookie_env,
            cookie_file,
            cert,
            key,
            ca,
            peers,
            runtime: engine,
            duration_secs,
            observer_bind,
            observer_token_env,
            observer_token_file,
            observer_cert,
            observer_key,
            observer_json_logs,
            observer_otel_endpoint,
            observer_otel_service_name,
            debug,
            debug_no_wait,
        } => {
            let debug = validate_debug_against_runtime(debug, engine)?;
            // M16-Backend P3 — install the unified JSON-log
            // subscriber globally so the daemon's tracing::* events
            // (scheduler supervisor escalation, dist reload audit,
            // observer HTTP audit from `audit.rs`) emerge as
            // single-line JSON on stderr. set_global_default fails
            // silently if a subscriber is already present (test
            // fixtures, embedding process); that's intentional —
            // we never want to override a hosting subscriber.
            if observer_json_logs {
                let subscriber = graphreact_observer::install_json_logs_subscriber(
                    graphreact_observer::stderr_writer(),
                );
                let _ = tracing::subscriber::set_global_default(subscriber);
            }
            let node_id: NodeId = node_id.parse().map_err(|e| {
                CommandError::ClusterArg(format!("--node-id: {e}"))
            })?;
            let cookie = resolve_cookie(cookie_env.as_deref(), cookie_file.as_deref())?;
            let tls = load_tls_from_paths(&cert, &key, &ca)?;
            let peers: Vec<_> = peers
                .iter()
                .map(|p| parse_peer_entry(p))
                .collect::<Result<Vec<_>, _>>()?;
            // M16-Backend P2 — only build a BindConfig when the
            // operator actually passed `--observer-bind`. The
            // shape-rule that `BindConfig::from_flags` enforces is
            // about a remote socket needing token+cert+key; a
            // missing `--observer-bind` means no observer at all.
            let observer_bind = if observer_bind.is_some()
                || observer_token_env.is_some()
                || observer_token_file.is_some()
                || observer_cert.is_some()
                || observer_key.is_some()
            {
                let flags = graphreact_observer::BindFlags {
                    addr: observer_bind,
                    token_env: observer_token_env,
                    token_file: observer_token_file,
                    cert: observer_cert,
                    key: observer_key,
                };
                Some(
                    graphreact_observer::BindConfig::from_flags(flags)
                        .map_err(|e| CommandError::ClusterArg(format!("{e}")))?,
                )
            } else {
                None
            };
            let opts = DaemonOptions {
                path,
                node_id,
                listen,
                cookie,
                tls,
                peers,
                runtime: engine,
                net_tick: NetTickConfig::default(),
                net_split_policy: NetSplitPolicy::default(),
                duration_secs,
                observer_bind,
                observer_otel_endpoint,
                observer_otel_service_name,
                debug,
                debug_wait: !debug_no_wait,
            };
            // Inline the run-loop so we can print the bound
            // address BETWEEN start_daemon (which has finished
            // binding + handshaking) and drive_handle_until
            // (which blocks). The "grx-daemon-ready <addr>"
            // line is parsed by the subprocess CLI test (the
            // pre-existing M14.8 boundary said no subprocess
            // test existed; this readiness signal closes that).
            runtime.block_on::<_>(async move {
                let mut handle = start_daemon(opts).await?;
                println!("grx-daemon-ready {}", handle.cluster().local_addr());
                if let Some(addr) = handle.observer_addr() {
                    // Lets external scrape tooling discover the
                    // observer bind without parsing flags back out
                    // of `ps aux`.
                    println!("grx-observer-ready {addr}");
                }
                drive_handle_until(&mut handle, duration_secs).await?;
                handle.shutdown().await;
                println!(
                    "grx-daemon-exited{}",
                    duration_secs
                        .map(|s| format!(" after {s}s"))
                        .unwrap_or_else(|| " (signal)".to_string())
                );
                Ok::<(), CommandError>(())
            })?;
        }
        ClusterOp::Connect {
            peer,
            node_id,
            listen,
            cookie_env,
            cookie_file,
            cert,
            key,
            ca,
            duration_secs,
        } => {
            let (peer_id, peer_addr) = parse_peer_entry(&peer)?;
            let node_id: NodeId = node_id.parse().map_err(|e| {
                CommandError::ClusterArg(format!("--node-id: {e}"))
            })?;
            let cookie = resolve_cookie(cookie_env.as_deref(), cookie_file.as_deref())?;
            let tls = load_tls_from_paths(&cert, &key, &ca)?;
            let opts = ConnectOptions {
                peer: peer_id,
                peer_addr,
                node_id,
                listen,
                cookie,
                tls,
                net_tick: NetTickConfig::default(),
                net_split_policy: NetSplitPolicy::default(),
                duration_secs,
            };
            let out = runtime.block_on(graphreact_cli::commands::run_connect_blocking(opts))?;
            println!("{out}");
        }
    }
    Ok(())
}

/// Pull the `.grx` file path out of subcommands that operate on one,
/// so the top-level error handler can render parser-error snippets.
fn associated_path(command: &Command) -> Option<&Path> {
    match command {
        Command::Run { path, .. }
        | Command::Check { path }
        | Command::Parse { path, .. }
        | Command::Fmt { path, .. }
        | Command::Reload { path } => Some(path),
        Command::Version
        | Command::Completions { .. }
        | Command::Init { .. }
        | Command::Cluster { .. }
        | Command::Ps { .. }
        | Command::Tree { .. }
        | Command::Top { .. }
        | Command::Trace { .. }
        | Command::Observer { .. } => None,
    }
}

/// Render the user-facing error string. For parser errors with a known
/// source path, render the source-line snippet with a caret. Falls
/// back to plain Display otherwise.
fn format_top_level_error(err: &CommandError, path: Option<&Path>) -> String {
    if let (CommandError::Parser(parse_err), Some(p)) = (err, path) {
        return format_parse_error(parse_err, p);
    }
    if let (CommandError::Runtime(graphreact_cli::runtime::RuntimeError::Parser(parse_err)), Some(p)) =
        (err, path)
    {
        return format_parse_error(parse_err, p);
    }
    format!("error: {err}")
}
