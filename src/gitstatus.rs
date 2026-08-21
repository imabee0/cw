use std::path::Path;

use anyhow::Result;

// Explicit StatusOptions, not `statuses(None)`'s defaults (advisor-caught —
// the two prior drafts of this function disagreed on the method name AND
// implicitly on whether ignored/untracked files count). include_ignored(false)
// means anything gitignored (e.g. target/, node_modules/) never counts as
// dirty. include_untracked(true) means a real untracked file (e.g. a new
// source file not yet `git add`ed) DOES count as dirty — that's live,
// not-yet-committed work, and both the pull guard (§5c) and `cw clean`
// should treat it exactly like a modified tracked file, not ignore it.
pub fn is_dirty(worktree_path: &Path) -> Result<bool> {
    let repo = git2::Repository::open(worktree_path)?;
    is_dirty_repo(&repo)
}

pub fn is_dirty_repo(repo: &git2::Repository) -> Result<bool> {
    let mut opts = git2::StatusOptions::new();
    opts.include_ignored(false)
        .include_untracked(true)
        .recurse_untracked_dirs(false);
    Ok(!repo.statuses(Some(&mut opts))?.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_dirty_ignores_gitignored_counts_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Commit the .gitignore itself first, so it doesn't register as an
        // untracked file and confound the "gitignored-only -> clean" check below.
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".gitignore")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        // Gitignored file only -> not dirty.
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/build-output"), "x").unwrap();
        assert!(!is_dirty_repo(&repo).unwrap());

        // Real untracked file -> dirty.
        std::fs::write(dir.path().join("src.rs"), "fn main() {}").unwrap();
        assert!(is_dirty_repo(&repo).unwrap());
    }
}
