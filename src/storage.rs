use crate::process::Cancellation;
use anyhow::{Context, Result, ensure};
use std::{fs, path::PathBuf};
use tempfile::TempDir;

pub fn publish(staging: TempDir, name: &str, cancel: &Cancellation) -> Result<PathBuf> {
    cancel.check()?;
    let path = staging.path();
    let parent = path.parent().context("Temporary directory has no parent")?;
    let filename = path
        .file_name()
        .context("Temporary directory has no name")?
        .to_string_lossy();
    let (_, suffix) = filename
        .rsplit_once('-')
        .context("Temporary directory has no unique suffix")?;
    let destination = parent.join(format!("{name}-{suffix}"));
    ensure!(
        !destination.exists(),
        "Destination already exists; please retry"
    );
    fs::rename(path, &destination).context("Cannot publish completed directory")?;
    Ok(destination)
}
