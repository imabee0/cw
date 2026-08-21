use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::gitstatus::is_dirty_repo;

/// `<root>/<owner>/<repo>` — never `<root>/<repo-name>` alone (§5a). A
/// personal repo and an org repo can share a bare name; flat layout would
/// silently collide, and owner-nesting also matches `gh`'s own default
/// clone shape.
///
/// Takes `owner`/`name` as plain strings rather than a `Repo` struct: §6
/// step 4 builds `github.rs` and `sync.rs` as parallel, independent waves,
/// so this module cannot compile-depend on a type owned by the other wave.
/// `root` is expected pre-expanded (`~` handled by the caller at
/// config-load time, not here).
pub fn resolve_local_path(root: &Path, owner: &str, name: &str) -> PathBuf {
    root.join(owner).join(name)
}

/// Result of a `clone_or_pull`/`fetch_and_ff` call — named so callers
/// (incl. `--dry-run`, §7b #15) can report exactly what happened/would
/// happen without re-deriving it from a raw `MergeAnalysis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    /// Repo wasn't cloned locally yet; `gh repo clone` ran instead of a pull.
    Cloned,
    /// Already up to date with the remote — nothing to fast-forward.
    UpToDate,
    /// Local branch fast-forwarded onto the fetched remote commit.
    FastForwarded,
    /// Fetched history isn't a fast-forward of the local branch (diverged) —
    /// left untouched; cw doesn't attempt a merge/rebase on the user's behalf.
    Diverged,
    /// A fast-forward was possible but the worktree has uncommitted local
    /// changes — aborted before touching any ref or file (§5c).
    DirtyLocalChanges,
}

/// git2 credentials callback for the *pull* path only — `gh repo clone`
/// authenticates itself via its own subprocess (§5b), so this is never
/// consulted during a fresh clone. Reuses `gh`'s own credential helper,
/// already wired into `~/.gitconfig` as `!gh auth git-credential` for
/// `https://github.com`; cw never stores or otherwise handles a token
/// itself.
fn credentials_callback(
    url: &str,
    username_from_url: Option<&str>,
    _allowed_types: git2::CredentialType,
) -> std::result::Result<git2::Cred, git2::Error> {
    let cfg = git2::Config::open_default()?;
    git2::Cred::credential_helper(&cfg, url, username_from_url)
}

/// Clones `owner/name` under `root` via `gh repo clone` if it isn't present
/// locally yet, otherwise opens the existing clone and fast-forwards its
/// current branch in place. Returns the opened repo plus what happened.
pub fn clone_or_pull(
    root: &Path,
    owner: &str,
    name: &str,
) -> Result<(git2::Repository, PullOutcome)> {
    let path = resolve_local_path(root, owner, name);

    if path.join(".git").exists() {
        let repo = git2::Repository::open(&path)
            .with_context(|| format!("opening existing clone at {}", path.display()))?;
        let branch = repo
            .head()
            .context("repo HEAD unresolved — detached or unborn?")?
            .shorthand()
            .context("HEAD is not a valid UTF-8 branch name")?
            .to_string();
        let outcome = fetch_and_ff(&repo, &branch)?;
        return Ok((repo, outcome));
    }

    let parent = path
        .parent()
        .context("resolved local path has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let slug = format!("{owner}/{name}");
    let status = Command::new("gh")
        .args(["repo", "clone", &slug, &path.to_string_lossy()])
        .status()
        .context(
            "launching `gh repo clone` — is `gh` on PATH and authenticated? run `gh auth login`",
        )?;
    if !status.success() {
        bail!("`gh repo clone {slug}` exited with {status}");
    }

    let repo = git2::Repository::open(&path)
        .with_context(|| format!("opening freshly cloned repo at {}", path.display()))?;
    Ok((repo, PullOutcome::Cloned))
}

/// Fetches `origin` and fast-forwards `branch` onto it — dirty-tree guard
/// added (§5c; a prior draft would have force-checked-out over uncommitted
/// local edits). The guard runs BEFORE any ref mutation: `set_target`/
/// `set_head`/`checkout_head` only ever run after `is_dirty_repo` has
/// already returned `false`, so an aborted pull leaves both the working
/// tree and the ref graph byte-for-byte unchanged — not just the file
/// contents, which a checkout could disturb even for files the incoming
/// commit doesn't touch (force-checkout resets the whole tree, not just
/// the diff).
pub fn fetch_and_ff(repo: &git2::Repository, branch: &str) -> Result<PullOutcome> {
    let mut remote = repo
        .find_remote("origin")
        .context("repo has no 'origin' remote")?;

    let mut callbacks = git2::RemoteCallbacks::new();
    callbacks.credentials(credentials_callback);
    let mut fetch_opts = git2::FetchOptions::new();
    fetch_opts.remote_callbacks(callbacks);

    remote
        .fetch(&[branch], Some(&mut fetch_opts), None)
        .with_context(|| format!("fetching '{branch}' from origin"))?;

    let fetch_head = repo
        .find_reference("FETCH_HEAD")
        .context("no FETCH_HEAD after fetch")?;
    let fetch_commit = repo.reference_to_annotated_commit(&fetch_head)?;
    let (analysis, _preference) = repo.merge_analysis(&[&fetch_commit])?;

    if analysis.is_up_to_date() {
        return Ok(PullOutcome::UpToDate);
    }
    if !analysis.is_fast_forward() {
        return Ok(PullOutcome::Diverged);
    }

    // Fast-forward is possible — but not at the cost of uncommitted local
    // work. Must be checked, and must abort, before any of the ref/checkout
    // calls below ever run.
    if is_dirty_repo(repo)? {
        return Ok(PullOutcome::DirtyLocalChanges);
    }

    let refname = format!("refs/heads/{branch}");
    let mut reference = repo
        .find_reference(&refname)
        .with_context(|| format!("local branch ref '{refname}' not found"))?;
    reference.set_target(fetch_commit.id(), "cw: fast-forward pull")?;
    repo.set_head(&refname)?;
    repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))?;

    Ok(PullOutcome::FastForwarded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_local_path_no_collision() {
        let root = Path::new("/tmp/cw-root-test");
        let a = resolve_local_path(root, "alice", "app");
        let b = resolve_local_path(root, "bob", "app");
        assert_ne!(
            a, b,
            "same repo name under different owners must not collide"
        );
        assert_eq!(a, root.join("alice").join("app"));
        assert_eq!(b, root.join("bob").join("app"));
    }

    /// F33 / §7b #9's underlying regression guard: a fast-forward-able pull
    /// on a worktree with uncommitted local changes must abort entirely —
    /// not just leave file bytes alone, but leave the branch ref untouched
    /// too, since a naive fix could move the ref before the guard runs.
    #[test]
    fn fetch_and_ff_aborts_when_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();

        // "origin": a real repo (not bare) used purely as a local fetch
        // source over file:// — never pushed to, so its own working tree
        // going out of sync with HEAD after the second commit below is fine.
        let origin_path = tmp.path().join("origin");
        std::fs::create_dir_all(&origin_path).unwrap();
        let origin = git2::Repository::init(&origin_path).unwrap();
        std::fs::write(origin_path.join("a.txt"), "v1").unwrap();
        let commit1 = {
            let mut index = origin.index().unwrap();
            index.add_path(Path::new("a.txt")).unwrap();
            index.write().unwrap();
            let tree = origin.find_tree(index.write_tree().unwrap()).unwrap();
            origin
                .commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
                .unwrap()
        };
        let branch = origin.head().unwrap().shorthand().unwrap().to_string();

        // Clone it locally via git2 (this is the worktree under test).
        let work_path = tmp.path().join("work");
        let work_url = format!("file://{}", origin_path.display());
        let work = git2::Repository::clone(&work_url, &work_path).unwrap();
        assert_eq!(work.head().unwrap().target().unwrap(), commit1);

        // New upstream commit in "origin" — a real fast-forward is available.
        {
            std::fs::write(origin_path.join("b.txt"), "v2").unwrap();
            let mut index = origin.index().unwrap();
            index.add_path(Path::new("b.txt")).unwrap();
            index.write().unwrap();
            let tree = origin.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = origin.find_commit(commit1).unwrap();
            origin
                .commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
                .unwrap();
        }

        // Uncommitted local edit in the clone — must survive the aborted pull.
        std::fs::write(work_path.join("a.txt"), "local-edit-not-yet-committed").unwrap();
        assert!(is_dirty_repo(&work).unwrap());

        let head_before = work.head().unwrap().target().unwrap();
        let outcome = fetch_and_ff(&work, &branch).unwrap();

        assert_eq!(outcome, PullOutcome::DirtyLocalChanges);
        assert_eq!(
            work.head().unwrap().target().unwrap(),
            head_before,
            "branch ref must not move when the pull aborts"
        );
        let bytes = std::fs::read_to_string(work_path.join("a.txt")).unwrap();
        assert_eq!(
            bytes, "local-edit-not-yet-committed",
            "uncommitted local edit must not be discarded by an aborted pull"
        );
    }
}
