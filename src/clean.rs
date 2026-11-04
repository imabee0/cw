use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::Cli;
use crate::config::Config;
use crate::dashboard::{self, Entry};
use crate::worktree;

/// `cw clean`: opens the dashboard scoped to every worktree across every
/// repo, letting the user mark rows with Space and remove them with `d`/`y`
/// (age and work state already annotated, per `tui::model::WorktreeRow`) — renders
/// identically to `cw resume`, just entered under a different name/`--force`
/// default. The dashboard itself owns the scan/select/confirm/remove flow
/// (`DashboardModel::confirm_delete`) — this is now a thin entry point, not
/// the flow's owner.
pub fn run_clean(cli: &Cli, config: &Config, root: &Path, force: bool) -> Result<()> {
    dashboard::run(Entry::Clean { force }, cli, config, root)
}

/// Removes one worktree: opens its repo, prunes the worktree checkout, and
/// deletes its branch. Shared by `DashboardModel::confirm_delete` (the
/// dashboard's in-TUI removal path) — `clean.rs` keeps owning the actual
/// git2 mechanics; the dashboard only decides which entries are checked.
///
/// Work state is re-read here, at the destructive boundary, rather than
/// trusting the dashboard's earlier snapshot: unless `force`, a worktree
/// with changed files or unpushed commits is refused. A worktree locked by a
/// live agent session is always refused. When the last worktree of a
/// cw-managed bare store goes, the store itself is deleted — nothing is kept
/// on disk for a repo with no open sessions.
pub(crate) fn remove_one(entry: &worktree::WorktreeEntry, force: bool) -> Result<()> {
    let repo_path = entry.repo_root();
    let repo = git2::Repository::open(&repo_path)
        .with_context(|| format!("opening {}", repo_path.display()))?;
    // Recheck at the destructive boundary, never trust an earlier UI snapshot.
    if let Ok(wt) = repo.find_worktree(&entry.slug) {
        release_stale_lock(&wt)?;
    }
    let state = crate::gitstatus::work_state(&entry.path, &repo_path)?;
    if !force && state.has_unsaved_work() {
        anyhow::bail!(
            "session has changed files or unpushed commits; commit and push before finishing"
        );
    }
    worktree::remove_worktree(&repo, &entry.slug)
        .with_context(|| format!("removing {}/{}", entry.repo, entry.slug))?;
    if repo.is_bare()
        && repo.config()?.get_bool("cw.managed").unwrap_or(false)
        && repo.worktrees()?.is_empty()
    {
        drop(repo);
        std::fs::remove_dir_all(&repo_path).context("removing finished session storage")?;
        // The `<root>/<owner>` dir too, once its last repo is gone —
        // `remove_dir` only succeeds on an empty dir, so this never
        // touches a sibling repo.
        if let Some(owner_dir) = repo_path.parent() {
            let _ = std::fs::remove_dir(owner_dir);
        }
    }
    Ok(())
}

/// Refuses a worktree locked by a live session; clears a lock whose owning
/// `cw` process is gone (crash, SIGKILL, closed terminal), so one bad exit
/// can't make a session permanently un-finishable. Locks not written by
/// `cw` (a user's own `git worktree lock`) are always respected.
fn release_stale_lock(wt: &git2::Worktree) -> Result<()> {
    let reason = match wt.is_locked()? {
        git2::WorktreeLockStatus::Unlocked => return Ok(()),
        git2::WorktreeLockStatus::Locked(reason) => reason.unwrap_or_default(),
    };
    let owner = reason
        .trim()
        .strip_prefix(crate::agent::LOCK_REASON_PREFIX)
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|pid| pid.parse::<u32>().ok());
    match owner {
        Some(pid) if !process_alive(pid) => {
            wt.unlock()?;
            Ok(())
        }
        Some(pid) => {
            anyhow::bail!("an agent session (pid {pid}) is still running here; exit it first")
        }
        None => anyhow::bail!(
            "worktree is locked ({}); run `git worktree unlock` if that's stale",
            if reason.is_empty() {
                "no reason given"
            } else {
                reason.trim()
            }
        ),
    }
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_storage_lives_until_last_safe_session_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let container = tmp.path().join("owner/repo");
        let repo = git2::Repository::init_bare(container.join(".git")).unwrap();
        repo.config().unwrap().set_bool("cw.managed", true).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        repo.reference("refs/remotes/origin/main", oid, true, "fetched")
            .unwrap();
        let first =
            worktree::create_or_resume_worktree(&repo, &container, "first", "HEAD").unwrap();
        worktree::create_or_resume_worktree(&repo, &container, "second", "HEAD").unwrap();
        std::fs::write(first.join("unsaved"), "work").unwrap();
        let entries = worktree::scan_worktrees(tmp.path()).unwrap();
        let a = entries.iter().find(|e| e.slug == "first").unwrap();
        let b = entries.iter().find(|e| e.slug == "second").unwrap();
        assert!(remove_one(a, false).is_err());
        remove_one(b, false).unwrap();
        assert!(container.exists());
        std::fs::remove_file(first.join("unsaved")).unwrap();
        drop(tree);
        drop(repo);
        remove_one(a, false).unwrap();
        assert!(!container.exists());
        assert!(
            !tmp.path().join("owner").exists(),
            "empty owner dir is removed too"
        );
    }

    #[test]
    fn stale_cw_lock_is_cleared_but_live_and_foreign_locks_refuse() {
        let tmp = tempfile::tempdir().unwrap();
        let container = tmp.path().join("owner/repo");
        let repo = git2::Repository::init_bare(container.join(".git")).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        worktree::create_or_resume_worktree(&repo, &container, "s", "HEAD").unwrap();
        let entry = worktree::scan_worktrees(tmp.path())
            .unwrap()
            .into_iter()
            .find(|e| e.slug == "s")
            .unwrap();
        let wt = repo.find_worktree("s").unwrap();

        wt.lock(Some("mine")).unwrap();
        assert!(remove_one(&entry, true).is_err());
        wt.unlock().unwrap();

        let live = format!(
            "{}{})",
            crate::agent::LOCK_REASON_PREFIX,
            std::process::id()
        );
        wt.lock(Some(&live)).unwrap();
        assert!(remove_one(&entry, true).is_err());
        wt.unlock().unwrap();

        // pid_max on Linux tops out at 2^22; this pid can't be alive.
        let dead = format!("{}{})", crate::agent::LOCK_REASON_PREFIX, 99_999_999);
        wt.lock(Some(&dead)).unwrap();
        remove_one(&entry, true).unwrap();
        assert!(!entry.path.exists());
    }

    #[test]
    fn force_removes_unsaved_work() {
        let tmp = tempfile::tempdir().unwrap();
        let container = tmp.path().join("owner/repo");
        let repo = git2::Repository::init_bare(container.join(".git")).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        let path = worktree::create_or_resume_worktree(&repo, &container, "s", "HEAD").unwrap();
        std::fs::write(path.join("wip"), "x").unwrap();
        let entry = worktree::scan_worktrees(tmp.path())
            .unwrap()
            .into_iter()
            .find(|e| e.slug == "s")
            .unwrap();
        assert!(remove_one(&entry, false).is_err());
        remove_one(&entry, true).unwrap();
        assert!(!path.exists());
    }
}
