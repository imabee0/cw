use std::path::Path;

use anyhow::Result;

/// What a worktree holds that exists nowhere else — the signal the
/// dashboard's removal confirm uses to tell a disposable worktree from one
/// carrying real work. Computed once per background scan
/// (`dashboard.rs::spawn_worktrees_thread`), never per keystroke.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkState {
    /// Modified, staged, or untracked (non-ignored) files.
    pub changed_files: usize,
    /// Commits reachable from the worktree's HEAD but from neither the main
    /// checkout's HEAD nor any remote-tracking ref — history that would be
    /// lost with the branch.
    pub unpushed_commits: usize,
}

impl WorkState {
    pub fn has_unsaved_work(&self) -> bool {
        self.changed_files > 0 || self.unpushed_commits > 0
    }
}

/// Upper bound on the unpushed-commit walk — the count is a warning signal,
/// not an exact figure, and a runaway walk over a huge unrelated history
/// must not stall the scan thread.
const UNPUSHED_CAP: usize = 1000;

/// Ignored files never count; untracked files do — a new source file is
/// live work the pull guard and the removal confirm must not skip.
pub fn is_dirty_repo(repo: &git2::Repository) -> Result<bool> {
    Ok(changed_file_count(repo)? > 0)
}

fn changed_file_count(repo: &git2::Repository) -> Result<usize> {
    let mut opts = git2::StatusOptions::new();
    opts.include_ignored(false)
        .include_untracked(true)
        .recurse_untracked_dirs(false);
    Ok(repo.statuses(Some(&mut opts))?.len())
}

/// Full work-state read for one worktree. `repo_root` is the main checkout
/// the worktree hangs off — its HEAD counts as "already saved" when
/// deciding which of the worktree's commits are unique to it.
pub fn work_state(worktree_path: &Path, repo_root: &Path) -> Result<WorkState> {
    let wt = git2::Repository::open(worktree_path)?;
    let changed_files = changed_file_count(&wt)?;
    let unpushed_commits = unpushed_commit_count(&wt, repo_root)?;
    Ok(WorkState {
        changed_files,
        unpushed_commits,
    })
}

/// Commits reachable from `wt`'s HEAD that are hidden by neither the main
/// checkout's HEAD nor any `refs/remotes/*` ref. A branch that was pushed
/// (even if never merged) counts as saved; a scratch worktree, which has no
/// remote at all, counts every commit — correct, since that history exists
/// nowhere else. An unborn HEAD (no commits yet) is simply 0.
fn unpushed_commit_count(wt: &git2::Repository, repo_root: &Path) -> Result<usize> {
    let Some(head) = wt.head().ok().and_then(|h| h.target()) else {
        return Ok(0);
    };
    let mut walk = wt.revwalk()?;
    walk.push(head)?;
    let base = git2::Repository::open(repo_root)
        .ok()
        .and_then(|main| main.head().ok().and_then(|h| h.target()));
    if let Some(base) = base {
        // A base the worktree's object db can't see (shouldn't happen — they
        // share one) just isn't hidden, which errs toward reporting more
        // unpushed work, never less.
        let _ = walk.hide(base);
    }
    for reference in wt.references_glob("refs/remotes/*")?.flatten() {
        if let Some(oid) = reference.resolve().ok().and_then(|r| r.target()) {
            let _ = walk.hide(oid);
        }
    }
    Ok(walk.take(UNPUSHED_CAP).filter(Result::is_ok).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_file(repo: &git2::Repository, dir: &Path, name: &str, msg: &str) -> git2::Oid {
        std::fs::write(dir.join(name), msg).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(name)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parents: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
            .unwrap()
    }

    #[test]
    fn is_dirty_ignores_gitignored_counts_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Commit the .gitignore itself first, so it doesn't register as an
        // untracked file and confound the "gitignored-only -> clean" check below.
        commit_file(&repo, dir.path(), ".gitignore", "target/\n");

        // Gitignored file only -> not dirty.
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/build-output"), "x").unwrap();
        assert!(!is_dirty_repo(&repo).unwrap());

        // Real untracked file -> dirty.
        std::fs::write(dir.path().join("src.rs"), "fn main() {}").unwrap();
        assert!(is_dirty_repo(&repo).unwrap());
    }

    #[test]
    fn work_state_distinguishes_clean_changed_unpushed_and_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        commit_file(&repo, &root, "README.md", "init");

        let wt_path = crate::worktree::create_or_resume_worktree(&repo, "feat", "HEAD").unwrap();
        let clean = work_state(&wt_path, &root).unwrap();
        assert_eq!(clean, WorkState::default());
        assert!(!clean.has_unsaved_work());

        // An untracked file is unsaved work.
        std::fs::write(wt_path.join("new.rs"), "x").unwrap();
        let changed = work_state(&wt_path, &root).unwrap();
        assert_eq!(changed.changed_files, 1);
        assert_eq!(changed.unpushed_commits, 0);
        assert!(changed.has_unsaved_work());

        // Committing it in the worktree turns it into an unpushed commit —
        // still unsaved work, since it exists on no remote and not on main.
        let wt = git2::Repository::open(&wt_path).unwrap();
        let tip = commit_file(&wt, &wt_path, "new.rs", "feature work");
        let committed = work_state(&wt_path, &root).unwrap();
        assert_eq!(committed.changed_files, 0);
        assert_eq!(committed.unpushed_commits, 1);
        assert!(committed.has_unsaved_work());

        // A remote-tracking ref at the tip (what a push leaves behind) makes
        // the commit saved: nothing unique to the worktree any more.
        repo.reference("refs/remotes/origin/worktree-feat", tip, true, "push")
            .unwrap();
        let pushed = work_state(&wt_path, &root).unwrap();
        assert_eq!(pushed.unpushed_commits, 0);
        assert!(!pushed.has_unsaved_work());
    }
}
