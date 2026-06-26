//! **M-Prod sub-track #1 Phase A.** Dispatcher for the
//! `grx audit {verify}` CLI surface.
//!
//! Today's surface is one operation: walk the NDJSON audit log
//! written by `--observer-audit-log`, recompute every record's
//! SHA-256 hash from its canonical bytes, and verify the chain
//! links match. Operators wire this into CI / SOC review workflows
//! alongside the pre-existing
//! `target="graphreact::observer::audit"` JSON-log stream.
//!
//! Phase B will add replay (`grx audit replay`), inspect
//! (`grx audit inspect <SEQ>`), and the strongly-typed RBAC verdict
//! filters that ride the same on-disk schema.

use std::io::{self, Write};

use graphreact_observer::{replay_audit_chain, AuditChainError};

use crate::cli::AuditOp;
use crate::commands::{CommandError, CommandResult};

/// Top-level dispatcher invoked from `bin/grx.rs` for `Command::Audit`.
pub fn run_audit(op: AuditOp) -> CommandResult<()> {
    match op {
        AuditOp::Verify { path } => run_verify(&path),
    }
}

/// `grx audit verify <PATH>` — open the file read-only, walk every
/// NDJSON line, and verify the chain. Prints either
/// `audit chain OK: <N> records` on success (exit 0) or a
/// structured failure message on the first violation (exit 1).
fn run_verify(path: &std::path::Path) -> CommandResult<()> {
    match replay_audit_chain(path) {
        Ok(records) => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "audit chain OK: {} records", records.len())?;
            stdout.flush()?;
            Ok(())
        }
        Err(err) => {
            // Print the structured-shape message to STDERR so a
            // CI pipeline scraping STDOUT for `audit chain OK`
            // never sees a partial / ambiguous result; the
            // top-level error handler also re-formats the error
            // for the operator-facing message. We pre-emit here
            // so operators get the structured fields up-front
            // (`seq=...`, `expected_hash=...`, etc.) without the
            // wrapping `audit verify failure:` prefix the
            // top-level handler adds — the structured fields are
            // the load-bearing operator signal.
            let mut stderr = io::stderr().lock();
            match &err {
                AuditChainError::TamperDetected {
                    seq,
                    expected_hash,
                    found_hash,
                } => {
                    writeln!(
                        stderr,
                        "tamper detected at seq={seq}: expected_hash={expected_hash} found_hash={found_hash}"
                    )?;
                }
                AuditChainError::PrevHashMismatch {
                    seq,
                    expected_prev_hash,
                    found_prev_hash,
                } => {
                    writeln!(
                        stderr,
                        "prev-hash mismatch at seq={seq}: expected_prev_hash={expected_prev_hash} found_prev_hash={found_prev_hash}"
                    )?;
                }
                AuditChainError::SequenceGap {
                    expected_seq,
                    found_seq,
                } => {
                    writeln!(
                        stderr,
                        "sequence gap: expected_seq={expected_seq} found_seq={found_seq}"
                    )?;
                }
                AuditChainError::Parse { line_index, detail } => {
                    writeln!(stderr, "parse failure at line {line_index}: {detail}")?;
                }
                AuditChainError::Io { path, source } => {
                    writeln!(stderr, "audit chain I/O at {}: {}", path.display(), source)?;
                }
                AuditChainError::Runtime { detail } => {
                    writeln!(stderr, "audit chain runtime requirement: {detail}")?;
                }
            }
            stderr.flush()?;
            Err(CommandError::AuditVerify(err))
        }
    }
}
