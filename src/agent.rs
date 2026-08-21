use std::io;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use tracing::warn;

use crate::config::AgentConfig;

/// Launches the configured agent CLI inside `worktree` with inherited
/// stdio, so the agent gets a real interactive terminal (§5i). Distinguishes
/// three outcomes, which matters for resume trustworthiness — a prior draft
/// treated `Command::status()`'s `Ok(ExitStatus)` as "session happened"
/// regardless of exit code:
///
/// - the binary isn't on `PATH` at all → a clear, actionable `Err`;
/// - it ran and exited nonzero → logged as a warning, `Ok(())` (worktree is
///   preserved either way; a crash shouldn't block getting back into it);
/// - it ran and exited cleanly → `Ok(())`, silently.
pub fn launch(agent: &AgentConfig, worktree: &Path) -> Result<()> {
    let status = Command::new(&agent.cmd)
        .args(&agent.args)
        .current_dir(worktree)
        .status()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "launching '{}' — is it on PATH? check [agents] in config.toml",
                    agent.cmd
                )
            } else {
                anyhow::Error::new(e).context(format!("launching '{}'", agent.cmd))
            }
        })?;

    if !status.success() {
        warn!(
            "{} exited with {status} — worktree preserved at {}",
            agent.cmd,
            worktree.display()
        );
    }

    Ok(())
}
