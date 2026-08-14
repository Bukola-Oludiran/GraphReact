use std::io::{self, Write as _};
use std::path::Path;

use graphreact_persistence::{LocalConfig, LocalPersistence, PersistenceEngine, PersistenceStatus};

use crate::cli::PersistenceOp;
use crate::commands::CommandResult;

pub async fn run_persistence(op: PersistenceOp) -> CommandResult<()> {
    match op {
        PersistenceOp::Verify { directory } => {
            let store = open_existing(&directory)?;
            let status = store.verify().await?;
            print_status("persistence verified", &status)
        }
        PersistenceOp::Status { directory } => {
            let store = open_existing(&directory)?;
            let status = store.status().await?;
            print_status("persistence status", &status)
        }
    }
}

fn open_existing(directory: &Path) -> graphreact_persistence::Result<LocalPersistence> {
    LocalPersistence::open(LocalConfig::discover(directory)?)
}

fn print_status(label: &str, status: &PersistenceStatus) -> CommandResult<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{label}:")?;
    writeln!(stdout, "  deployment_id: {}", status.deployment_id)?;
    writeln!(
        stdout,
        "  deployment_digest: {}",
        status.deployment_digest.hex()
    )?;
    writeln!(stdout, "  fencing_token: {}", status.fencing_token)?;
    writeln!(
        stdout,
        "  head_sequence: {}",
        optional_number(status.head_sequence)
    )?;
    writeln!(
        stdout,
        "  snapshot_sequence: {}",
        optional_number(status.snapshot_sequence)
    )?;
    writeln!(stdout, "  durable_bytes: {}", status.durable_bytes)?;
    writeln!(stdout, "  artifact_count: {}", status.artifact_count)?;
    writeln!(
        stdout,
        "  lease_owner: {}",
        status.lease_owner.as_deref().unwrap_or("none")
    )?;
    stdout.flush()?;
    Ok(())
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}
