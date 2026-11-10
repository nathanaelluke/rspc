pub mod benchmark;
pub mod licenses;
pub mod pairing;
pub mod privacy;
pub mod release;
pub mod soloist;

use anyhow::{Context, Result, ensure};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

pub fn run(command: &mut Command) -> Result<()> {
    ensure!(
        command.status().context("Cannot start command")?.success(),
        "Command failed: {}",
        command.get_program().to_string_lossy()
    );
    Ok(())
}

/// Read bounded stdout, without including a failed command's output in errors.
pub fn capture(command: &mut Command, limit: usize) -> Result<Vec<u8>> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut bytes = Vec::new();
    let result = child
        .stdout
        .take()
        .context("Missing command output")?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes);
    if result.is_err() || bytes.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        result?;
        anyhow::bail!("Command output exceeded its limit");
    }
    ensure!(
        child.wait()?.success(),
        "Command failed: {}",
        command.get_program().to_string_lossy()
    );
    Ok(bytes)
}
