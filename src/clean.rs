use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::Cli;
use crate::config::Config;
use crate::dashboard::{self, Entry};
use crate::worktree;

/// `cw clean`: opens the dashboard scoped to every worktree across every
/// repo, in delete-mode, letting the user multi-select which to remove
/// (idle/dirty already annotated, per `tui::model::WorktreeRow`). The
/// dashboard itself owns the scan/select/confirm/remove flow
/// (`DashboardModel::confirm_delete`) — this is now a thin entry point, not
/// the flow's owner.
pub fn run_clean(cli: &Cli, config: &Config, root: &Path, force: bool) -> Result<()> {
    dashboard::run(Entry::Clean { force }, cli, config, root)
}

/// Removes one worktree: opens its repo, prunes the worktree checkout, and
/// deletes its branch. Shared by `DashboardModel::confirm_delete` (the
/// dashboard's in-TUI removal path) — `clean.rs` keeps owning the actual
/// git2 mechanics; the dashboard only decides which entries are checked.
pub(crate) fn remove_one(root: &Path, entry: &worktree::WorktreeEntry) -> Result<()> {
    let repo_path = root.join(&entry.repo);
    let repo = git2::Repository::open(&repo_path)
        .with_context(|| format!("opening {}", repo_path.display()))?;
    worktree::remove_worktree(&repo, &entry.slug)
        .with_context(|| format!("removing {}/{}", entry.repo, entry.slug))
}
