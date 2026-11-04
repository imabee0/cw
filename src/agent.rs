use std::io;
use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::config::AgentConfig;

/// Prefix of the lock reason `launch` writes into a worktree's git metadata.
/// `clean::remove_one` parses the pid back out to tell a live session from
/// a lock a crashed/killed `cw` left behind.
pub const LOCK_REASON_PREFIX: &str = "cw: agent session active (pid ";

/// Holds a worktree lock for the agent's lifetime, released on drop. Only
/// releases a lock this process actually took — a second `cw` launching into
/// an already-locked worktree leaves the first session's lock alone.
struct SessionLock(Option<git2::Worktree>);

impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Some(wt) = &self.0 {
            let _ = wt.unlock();
        }
    }
}

fn acquire_lock(worktree: &Path) -> SessionLock {
    let wt = git2::Repository::open(worktree)
        .ok()
        .filter(|repo| repo.is_worktree())
        .and_then(|repo| git2::Worktree::open_from_repository(&repo).ok())
        .filter(|wt| matches!(wt.is_locked(), Ok(git2::WorktreeLockStatus::Unlocked)))
        .filter(|wt| {
            wt.lock(Some(&format!(
                "{LOCK_REASON_PREFIX}{})",
                std::process::id()
            )))
            .is_ok()
        });
    SessionLock(wt)
}

/// Launches the configured agent CLI inside `worktree` with inherited
/// stdio, so the agent gets a real interactive terminal (§5i). While it
/// runs, the worktree is git-locked so `cw finish`/`cw clean` can't remove
/// it out from under the session.
///
/// - the binary can't be started (not on `PATH`, …) → `Err`;
/// - it ran → `Ok(Some(note))` when it exited nonzero (Ctrl-C out of an
///   agent, a shell whose last command failed — routine, not a launch
///   failure, so callers show it as a note, never an error), `Ok(None)` on a
///   clean exit. The worktree is preserved either way.
pub fn launch(agent: &AgentConfig, worktree: &Path) -> Result<Option<String>> {
    let _lock = acquire_lock(worktree);

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

    Ok((!status.success()).then(|| {
        format!(
            "{} exited with {status} — worktree kept at {}",
            agent.cmd,
            worktree.display()
        )
    }))
}

/// `launch` for the plain-terminal fast paths: prints the nonzero-exit note
/// to stderr instead of returning it.
pub fn launch_reporting(agent: &AgentConfig, worktree: &Path) -> Result<()> {
    if let Some(note) = launch(agent, worktree)? {
        eprintln!("{note}");
    }
    Ok(())
}
