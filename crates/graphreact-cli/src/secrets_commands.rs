//! **M-Prod sub-track #3 (Phase A).** Dispatcher for the
//! `grx secrets {set,get,list,remove}` CLI surface.
//!
//! Reads its inputs from the parsed [`crate::cli::SecretsOp`] tree,
//! constructs a `Box<dyn graphreact_secrets::SecretStore>` against
//! the selected backend, and runs the operation. `set` consumes
//! the plaintext from stdin via `rpassword` (TTY-detected; no-echo
//! prompt on a terminal, line-buffered fallback for piped input)
//! so the secret never appears on the `ps`/shell-history surface.
//!
//! The Phase B runtime accessor (`ctx.secret(name)` inside `.grx`
//! bodies) is deferred — see `docs/backend/m-prod-secrets.md`.

use std::io::{self, BufRead, IsTerminal, Write};

use graphreact_secrets::{
    EnvStore, KeyringStore, SecretStore, SecretValue, SecretsError,
};

use crate::cli::{SecretsBackend, SecretsOp};
use crate::commands::{CommandError, CommandResult};

/// Top-level dispatcher invoked from `bin/grx.rs` for `Command::Secrets`.
pub fn run_secrets(op: SecretsOp) -> CommandResult<()> {
    match op {
        SecretsOp::Set { name, backend } => run_set(&name, backend),
        SecretsOp::Get { name, backend } => run_get(&name, backend),
        SecretsOp::List { backend } => run_list(backend),
        SecretsOp::Remove { name, backend } => run_remove(&name, backend),
    }
}

fn store_for(backend: SecretsBackend) -> Box<dyn SecretStore> {
    match backend {
        SecretsBackend::Keyring => Box::new(KeyringStore),
        SecretsBackend::Env => Box::new(EnvStore),
    }
}

/// `grx secrets set <NAME>` — read plaintext from stdin into a
/// zeroized buffer, hand it to the backend, then drop. The buffer
/// is the only path that touches the secret bytes; nothing is
/// logged at info/debug — only the structured audit line
/// (`graphreact::secrets::audit` target) carries the action +
/// backend + name.
fn run_set(name: &str, backend: SecretsBackend) -> CommandResult<()> {
    let plaintext = read_secret_from_stdin()?;
    // SecretValue::from_string takes ownership of `plaintext` and
    // zeroizes the local buffer; from this point only the
    // SecretValue holds the bytes.
    let value = SecretValue::from_string(plaintext).map_err(CommandError::Secrets)?;
    let store = store_for(backend);
    store.set(name, value).map_err(CommandError::Secrets)?;
    // Emit nothing to stdout on success — the CLI surface treats
    // a clean exit code as the success signal so callers can
    // pipeline `grx secrets set X && ...` without scraping output.
    Ok(())
}

/// `grx secrets get <NAME>` — write the raw bytes to stdout WITHOUT
/// a trailing newline. Callers compose via `--token-file <(grx secrets get X)`
/// or `KEY=$(grx secrets get X)`. Exit 1 on `SecretsError::NotFound`
/// (the typed error already prints the operator-friendly message).
fn run_get(name: &str, backend: SecretsBackend) -> CommandResult<()> {
    let store = store_for(backend);
    let value = store.get(name).map_err(CommandError::Secrets)?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(value.expose_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// `grx secrets list` — names only, sorted ascending, one per line.
/// Empty output is a legitimate result: the keyring backend's
/// Phase-A `list` returns `Ok(vec![])` because the `keyring` crate's
/// enumeration surface isn't uniform across platforms (see
/// `KeyringStore::list` docstring). The env backend always returns
/// the full set.
fn run_list(backend: SecretsBackend) -> CommandResult<()> {
    let store = store_for(backend);
    let names = store.list().map_err(CommandError::Secrets)?;
    let mut stdout = io::stdout().lock();
    for name in names {
        writeln!(stdout, "{name}")?;
    }
    stdout.flush()?;
    Ok(())
}

/// `grx secrets remove <NAME>` — idempotent. Returns exit code 0
/// whether or not the entry was present, so scripts that "ensure
/// absent" don't need to special-case the `NotFound` path.
fn run_remove(name: &str, backend: SecretsBackend) -> CommandResult<()> {
    let store = store_for(backend);
    store.remove(name).map_err(CommandError::Secrets)?;
    Ok(())
}

/// Read one secret value from stdin.
///
/// - TTY input: `rpassword::prompt_password` disables terminal ECHO
///   for the duration of the read (POSIX termios bit-twiddle on
///   Unix, Win32 console-mode toggle on Windows).
/// - Non-TTY (piped) input: read a single line WITHOUT disabling
///   echo (no terminal to disable) and emit a stderr warning that
///   the caller is responsible for not having logged the value.
///
/// The returned `String` is allocated once and handed to
/// `SecretValue::from_string`, which transfers ownership and
/// zeroizes the original allocation.
fn read_secret_from_stdin() -> Result<String, CommandError> {
    if io::stdin().is_terminal() {
        // Interactive: rpassword writes the prompt to stderr,
        // disables ECHO, reads a line, restores ECHO, and returns
        // the line without the trailing newline. On error the
        // termios state is still restored via the crate's internal
        // Drop guard.
        let plaintext = rpassword::prompt_password("Enter secret value (input hidden): ")
            .map_err(|e| io::Error::other(format!("rpassword prompt failed: {e}")))?;
        if plaintext.is_empty() {
            return Err(CommandError::Secrets(SecretsError::Empty));
        }
        Ok(plaintext)
    } else {
        // Piped stdin: read one line. Warn on stderr so an
        // operator scripting `echo "$pw" | grx secrets set X` sees
        // that the value just hit shell history.
        let mut stderr = io::stderr().lock();
        writeln!(
            stderr,
            "warning: reading secret from piped stdin; shell history / process listings may contain it"
        )?;
        stderr.flush()?;
        read_line_from_stdin()
    }
}

/// Read one line from stdin, stripping a trailing `\n` or `\r\n`.
/// Used for piped input only — interactive input goes through
/// `rpassword::prompt_password` directly.
fn read_line_from_stdin() -> Result<String, CommandError> {
    let mut buf = String::new();
    let stdin_handle = io::stdin();
    let mut locked = stdin_handle.lock();
    locked.read_line(&mut buf)?;
    if buf.ends_with('\n') {
        buf.pop();
    }
    if buf.ends_with('\r') {
        buf.pop();
    }
    if buf.is_empty() {
        return Err(CommandError::Secrets(SecretsError::Empty));
    }
    Ok(buf)
}
