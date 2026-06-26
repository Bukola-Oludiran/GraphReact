//! **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Dispatcher
//! for the `grx admin {reload-tokens}` CLI surface.
//!
//! `kill(2)` is FFI — this module deliberately enables
//! `unsafe_code` for the one syscall. See [`send_sighup`] for the
//! safety justification.
#![cfg_attr(unix, allow(unsafe_code))]

//!
//! Today's surface is one operation: send SIGHUP to a running
//! daemon PID so it reloads the on-disk token registry file (and
//! rotates the audit log + reloads TLS certs in the same fan-out).
//! The CLI is a thin signal wrapper — exit 0 on successful
//! `kill(2)`, exit 1 otherwise. Operators wait for a follow-up
//! audit row of shape `token_registry_reloaded` /
//! `token_registry_reload_failed` to know the in-process outcome.
//!
//! Phase B Part 2 (Group 1cd) will add JWT-OIDC reload + remote
//! HTTP-driven admin endpoints; today's surface is SIGHUP-only.

use std::io::{self, Write};
use std::path::Path;

use crate::cli::AdminOp;
use crate::commands::{CommandError, CommandResult};

/// Top-level dispatcher invoked from `bin/grx.rs` for `Command::Admin`.
pub fn run_admin(op: AdminOp) -> CommandResult<()> {
    match op {
        AdminOp::ReloadTokens { pid, pid_file } => run_reload_tokens(pid, pid_file.as_deref()),
    }
}

/// `grx admin reload-tokens [--pid <N>|--pid-file <PATH>]` — send
/// SIGHUP to the daemon at `pid`.
fn run_reload_tokens(pid: Option<u32>, pid_file: Option<&Path>) -> CommandResult<()> {
    let pid = resolve_pid(pid, pid_file)?;
    send_sighup(pid)?;
    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "grx admin reload-tokens: SIGHUP sent to PID {pid}; \
         daemon will emit `token_registry_reloaded` or \
         `token_registry_reload_failed` audit row asynchronously"
    )?;
    stdout.flush()?;
    Ok(())
}

/// Read the daemon PID — precedence: explicit `--pid` > file path.
/// Refuses to proceed when neither was supplied.
fn resolve_pid(pid: Option<u32>, pid_file: Option<&Path>) -> CommandResult<u32> {
    if let Some(p) = pid {
        return Ok(p);
    }
    if let Some(path) = pid_file {
        let text = std::fs::read_to_string(path).map_err(|err| {
            CommandError::ClusterArg(format!(
                "grx admin reload-tokens: read --pid-file {}: {err}",
                path.display()
            ))
        })?;
        return text.trim().parse::<u32>().map_err(|err| {
            CommandError::ClusterArg(format!(
                "grx admin reload-tokens: --pid-file {} did not contain a u32 PID: {err}",
                path.display()
            ))
        });
    }
    Err(CommandError::ClusterArg(
        "grx admin reload-tokens: pass --pid <N> or --pid-file <PATH>".to_string(),
    ))
}

/// Send SIGHUP via the libc `kill(2)` syscall. Unix-only — Windows
/// has no SIGHUP analogue; the binary returns an explanatory error
/// on non-Unix platforms.
///
/// We deliberately do NOT pull `libc` into `[dependencies]` for
/// this one syscall (per the "no new workspace deps" constraint).
/// Instead, declare the FFI signature inline — `kill(pid_t, c_int)
/// -> c_int` is a stable POSIX surface and the symbol is exposed
/// by every libc the toolchain links.
#[cfg(unix)]
fn send_sighup(pid: u32) -> CommandResult<()> {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SIGHUP is signal 1 across every POSIX platform (Linux + the
    // BSDs + macOS). Hard-coding it sidesteps the `libc::SIGHUP`
    // import.
    const SIGHUP_NUMBER: i32 = 1;
    // POSIX guarantees pid_t is a signed integer at least 16 bits
    // wide. On every modern Unix it's `i32`. Casting via TryFrom
    // gates against pathological PIDs that don't fit.
    let signed_pid: i32 = i32::try_from(pid).map_err(|_| {
        CommandError::ClusterArg(format!(
            "grx admin reload-tokens: PID {pid} doesn't fit pid_t (i32)"
        ))
    })?;
    // SAFETY: `kill` with a valid pid + SIGHUP is a standard POSIX
    // operation. The target's signal handler is installed by
    // `drive_handle_until`; absent that, SIGHUP defaults to
    // terminating the process — the operator already knew the
    // target's pid AND chose to invoke this command so that's
    // their intent.
    let rc = unsafe { kill(signed_pid, SIGHUP_NUMBER) };
    if rc != 0 {
        let errno = io::Error::last_os_error();
        return Err(CommandError::ClusterArg(format!(
            "grx admin reload-tokens: kill(pid={pid}, SIGHUP) failed: {errno}"
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn send_sighup(_pid: u32) -> CommandResult<()> {
    Err(CommandError::ClusterArg(
        "grx admin reload-tokens is unix-only; SIGHUP has no Windows analogue".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_pid_prefers_explicit_pid() {
        let p = resolve_pid(Some(42), None).expect("resolve");
        assert_eq!(p, 42);
    }

    #[test]
    fn resolve_pid_reads_from_file() {
        let dir = std::env::temp_dir().join(format!(
            "grx-admin-pid-test-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.pid");
        std::fs::write(&path, "1234\n").unwrap();
        let p = resolve_pid(None, Some(&path)).expect("resolve");
        assert_eq!(p, 1234);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_pid_refuses_neither() {
        let err = resolve_pid(None, None).unwrap_err();
        assert!(matches!(err, CommandError::ClusterArg(_)));
    }
}
