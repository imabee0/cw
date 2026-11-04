//! Getting a repo's history onto disk, on demand. A repo is cloned only when
//! a session in it starts, as a **bare store** (`<root>/<owner>/<name>/.git`,
//! no checkout of its own); every session is a worktree of that store, and
//! `clean::remove_one` deletes the store with its last session. Network
//! access is `gh repo clone` for the first clone and system `git fetch`
//! afterwards — both honour SSH remotes and configured credential helpers,
//! and neither ever prompts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::gitstatus::is_dirty_repo;

/// `<root>/<owner>/<repo>` — never `<root>/<repo-name>` alone. A personal
/// repo and an org repo can share a name; a flat layout would silently
/// collide. `root` is expected pre-expanded (`~` handled by the caller).
pub fn resolve_local_path(root: &Path, owner: &str, name: &str) -> PathBuf {
    root.join(owner).join(name)
}

/// What `clone_or_pull` did, so callers can report it. `Diverged` and
/// `DirtyLocalChanges` never reach a caller of `clone_or_pull` (it turns
/// them into errors); they're `fetch_and_ff`'s own results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    /// No store existed yet; a fresh bare store was cloned (or, if another
    /// `cw` finished cloning the same repo first, that one is used).
    Cloned,
    /// An existing bare store fetched the remote's current default branch;
    /// new sessions branch from it.
    Fetched,
    /// A full checkout (a `root` the user populated themselves) was already
    /// up to date with its remote branch.
    UpToDate,
    /// A full checkout's branch fast-forwarded onto the fetched commit.
    FastForwarded,
    /// Fetched history isn't a fast-forward of the local branch (diverged) —
    /// left untouched; cw doesn't attempt a merge/rebase on the user's behalf.
    Diverged,
    /// A fast-forward was possible but the checkout has uncommitted local
    /// changes — aborted before touching any ref or file.
    DirtyLocalChanges,
    /// Already present; `--no-pull` skipped fetching entirely.
    Skipped,
}

/// Which stdio a `gh repo clone` subprocess should use — `Inherit` for the
/// plain-terminal fast path (the user sees `gh`'s own progress output),
/// `Capture` for the dashboard's background clone thread, where inherited
/// stdio would corrupt the active alt-screen.
/// `Capture` also sets `GH_PROMPT_DISABLED=1` (confirmed via `gh help
/// environment`) so a `gh` that would otherwise block on an interactive
/// prompt fails fast instead of hanging a background thread forever.
pub enum CloneStdio {
    Inherit,
    Capture,
}

/// Makes `owner/name` ready to branch a new session from, with the
/// terminal's stdio for `gh` (see `clone_or_pull_ex`).
pub fn clone_or_pull(
    root: &Path,
    owner: &str,
    name: &str,
    pull: bool,
) -> Result<(git2::Repository, PullOutcome)> {
    clone_or_pull_ex(root, owner, name, pull, CloneStdio::Inherit)
}

/// Makes `owner/name` ready to branch a new session from:
/// - nothing on disk → clone a managed bare store (`Cloned`);
/// - an existing bare store → fetch the remote default branch into
///   `refs/cw/default`, which `HEAD` points at (`Fetched`);
/// - a full checkout (only if the user put one under `root` themselves) →
///   fast-forward its current branch, refusing — as an error — a detached
///   HEAD, a diverged branch, or uncommitted changes, rather than silently
///   starting a session from stale code;
/// - `pull == false` (`--no-pull`) → use what's on disk (`Skipped`).
///
/// `stdio` only affects the `gh repo clone` subprocess; fetches always run
/// captured, since their stderr is folded into the error.
pub fn clone_or_pull_ex(
    root: &Path,
    owner: &str,
    name: &str,
    pull: bool,
    stdio: CloneStdio,
) -> Result<(git2::Repository, PullOutcome)> {
    let path = resolve_local_path(root, owner, name);

    if path.join(".git").exists() {
        let repo = git2::Repository::open(&path)
            .with_context(|| format!("opening existing clone at {}", path.display()))?;
        if !pull {
            return Ok((repo, PullOutcome::Skipped));
        }
        if repo.is_bare() {
            fetch_default(&repo)?;
            return Ok((repo, PullOutcome::Fetched));
        }
        if repo.head_detached()? {
            bail!("{owner}/{name} has a detached HEAD; check out a branch before updating, or use --no-pull to explicitly use local code");
        }
        let branch = {
            let head = repo.head().with_context(|| {
                format!(
                    "{owner}/{name} has no HEAD — empty repository with no commits yet ({})",
                    path.display()
                )
            })?;
            head.shorthand()
                .context("HEAD is not a valid UTF-8 branch name")?
                .to_string()
        };
        let outcome = fetch_and_ff(&repo, &branch)?;
        match outcome {
            PullOutcome::Diverged => bail!("{owner}/{name} has diverged from origin; reconcile the branch before continuing, or use --no-pull to explicitly use local code"),
            PullOutcome::DirtyLocalChanges => bail!("{owner}/{name} has local changes preventing an update; commit or stash them before continuing, or use --no-pull to explicitly use local code"),
            _ => {}
        }
        return Ok((repo, outcome));
    }

    let parent = path
        .parent()
        .context("resolved local path has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    // Clone into a private staging dir, then rename it into place: a failed
    // or interrupted clone never leaves a half-made store where the `.git`
    // check above would mistake it for a real one, and two `cw`s cloning the
    // same repo at once can't trample each other — the loser of the rename
    // discards its copy and uses the winner's. Moving a bare repo is safe
    // here because it has no worktrees yet.
    let staging = parent.join(format!(".{name}.cw-clone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging); // leftover from a killed run with a reused pid
    let result = clone_managed_store(&staging, owner, name, stdio).and_then(|repo| {
        drop(repo);
        // An empty dir (nothing else ever creates one here) would block
        // the rename; `remove_dir` refuses anything non-empty.
        let _ = std::fs::remove_dir(&path);
        match std::fs::rename(&staging, &path) {
            Ok(()) => Ok(PullOutcome::Cloned),
            Err(_) if path.join(".git").exists() => Ok(PullOutcome::Cloned),
            Err(e) => Err(anyhow::Error::new(e)
                .context(format!("moving fresh clone into {}", path.display()))),
        }
    });
    let _ = std::fs::remove_dir_all(&staging);
    let outcome = result?;
    let repo = git2::Repository::open(&path)
        .with_context(|| format!("opening freshly cloned repo at {}", path.display()))?;
    Ok((repo, outcome))
}

/// `gh repo clone -- --bare` into `<path>/.git`, then `configure_managed_store`.
/// Only history lives on disk — no checkout until a session worktree is
/// created.
fn clone_managed_store(
    path: &Path,
    owner: &str,
    name: &str,
    stdio: CloneStdio,
) -> Result<git2::Repository> {
    let slug = format!("{owner}/{name}");
    let mut cmd = Command::new("gh");
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    let git_path = path.join(".git");
    cmd.args([
        "repo",
        "clone",
        &slug,
        &git_path.to_string_lossy(),
        "--",
        "--bare",
    ]);

    match stdio {
        CloneStdio::Inherit => {
            let status = cmd.status().context(
                "launching `gh repo clone` — is `gh` on PATH and authenticated? run `gh auth login`",
            )?;
            if !status.success() {
                bail!("`gh repo clone {slug}` exited with {status}");
            }
        }
        // `.output()`, not `.status()` with piped stdio left unread — a
        // subprocess with a full pipe buffer and nobody draining it can
        // deadlock. Mirrors `github.rs`'s existing convention for a
        // subprocess run concurrently with an active TUI.
        CloneStdio::Capture => {
            cmd.env("GH_PROMPT_DISABLED", "1").stdin(Stdio::null());
            let output = cmd.output().context(
                "launching `gh repo clone` — is `gh` on PATH and authenticated? run `gh auth login`",
            )?;
            if !output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let detail = [stdout.trim(), stderr.trim()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                if detail.is_empty() {
                    bail!("`gh repo clone {slug}` exited with {}", output.status);
                }
                bail!(
                    "`gh repo clone {slug}` exited with {}: {detail}",
                    output.status
                );
            }
        }
    }

    let repo = git2::Repository::open(path)
        .with_context(|| format!("opening freshly cloned repo at {}", path.display()))?;
    configure_managed_store(&repo)?;
    Ok(repo)
}

/// Turns a fresh `clone --bare` into a store worktrees can work against
/// like a normal clone:
/// - `cw.managed = true` marks it as cw's to delete once the last session
///   finishes (a user's own bare repo is never touched);
/// - a standard `origin` fetch refspec, which `--bare` omits, so `git
///   fetch`/`pull`/`status` inside a worktree see `origin/*` as usual;
/// - `origin/HEAD` pointing at the default branch;
/// - the `refs/heads/*` snapshot `--bare` copies from the remote is
///   dropped — those would silently go stale; sessions branch from the
///   fetched `refs/cw/default`, and `git switch <branch>` inside a worktree
///   still creates a tracking branch from `origin/<branch>`.
fn configure_managed_store(repo: &git2::Repository) -> Result<()> {
    let default_branch = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|h| h.symbolic_target().ok().flatten().map(str::to_owned))
        .and_then(|t| t.strip_prefix("refs/heads/").map(str::to_owned));
    let mut config = repo.config()?;
    config.set_bool("cw.managed", true)?;
    config.set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")?;
    // Session branches have no upstream yet; let a plain `git push` from an
    // agent create `origin/worktree-<slug>` instead of erroring.
    config.set_bool("push.autoSetupRemote", true)?;
    fetch_default(repo)?;
    if let Some(branch) = default_branch {
        let target = format!("refs/remotes/origin/{branch}");
        if repo.find_reference(&target).is_ok() {
            repo.reference_symbolic("refs/remotes/origin/HEAD", &target, true, "cw: origin/HEAD")?;
        }
    }
    let stale: Vec<String> = repo
        .references_glob("refs/heads/*")?
        .flatten()
        .filter_map(|r| r.name().ok().map(str::to_owned))
        .collect();
    for name in stale {
        repo.find_reference(&name)?.delete()?;
    }
    Ok(())
}

/// Fetches the remote's HEAD (whatever its default branch is today, so a
/// rename is followed) into `refs/cw/default` and every remote branch into
/// `refs/remotes/origin/*`, then points the store's `HEAD` at
/// `refs/cw/default` (detached) — the base every new session branches from.
fn fetch_default(repo: &git2::Repository) -> Result<()> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repo.path())
        .args([
            // No `--prune`: a session's `origin/worktree-<slug>` must outlive
            // the remote branch (deleted on PR merge), or its already-pushed
            // commits would read as unpushed and block `cw finish`.
            "fetch",
            "origin",
            "+HEAD:refs/cw/default",
            "+refs/heads/*:refs/remotes/origin/*",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .context("fetching remote default branch")?;
    if !output.status.success() {
        bail!(
            "fetching remote default branch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    repo.set_head("refs/cw/default")?;
    Ok(())
}

/// Full checkouts only (a bare store uses `fetch_default`): fetches `branch`
/// from `origin` with system `git` and fast-forwards it. Uncommitted changes
/// abort before any ref or file is touched (`DirtyLocalChanges`); otherwise
/// the new tree is checked out with a *safe* checkout first and the branch
/// moved only after that succeeded, so an obstruction can never leave the
/// branch advanced over an old working tree. A non-fast-forward is reported
/// as `Diverged` and left alone — cw never merges or rebases for you.
pub fn fetch_and_ff(repo: &git2::Repository, branch: &str) -> Result<PullOutcome> {
    // Use Git's configured transport and credential helpers (including SSH),
    // consistently with gh's clone subprocess. Never prompt inside the TUI.
    let output = Command::new("git")
        .arg("-C")
        .arg(
            repo.workdir()
                .context("repository has no working directory")?,
        )
        .args([
            "fetch",
            "--no-tags",
            "origin",
            &format!("refs/heads/{branch}"),
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .context("running git fetch")?;
    if !output.status.success() {
        bail!(
            "fetching {branch} from origin failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

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
    // Check out safely before moving the branch: an obstruction must not
    // leave HEAD advanced with the old working tree still on disk.
    let target = repo.find_object(fetch_commit.id(), None)?;
    repo.checkout_tree(
        &target,
        Some(git2::build::CheckoutBuilder::default().safe()),
    )?;
    reference.set_target(fetch_commit.id(), "cw: fast-forward pull")?;
    repo.set_head(&refname)?;

    Ok(PullOutcome::FastForwarded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn rev(repo: &git2::Repository, spec: &str) -> git2::Oid {
        repo.revparse_single(spec)
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
    }

    /// The full managed-store lifecycle against a local "remote", minus the
    /// `gh` subprocess: bare clone → configure → session worktree that
    /// behaves like a normal clone → a later fetch moves new sessions onto
    /// the latest default branch.
    #[test]
    fn managed_store_tracks_remote_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "trunk"]);
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&origin, &["branch", "feature"]);

        let store = tmp.path().join("store/acme/proj");
        std::fs::create_dir_all(&store).unwrap();
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                &origin.to_string_lossy(),
                &store.join(".git").to_string_lossy(),
            ],
        );
        let repo = git2::Repository::open(&store).unwrap();
        configure_managed_store(&repo).unwrap();

        assert!(repo.config().unwrap().get_bool("cw.managed").unwrap());
        assert_eq!(
            repo.references_glob("refs/heads/*").unwrap().count(),
            0,
            "stale branch snapshots from --bare must be dropped"
        );
        assert_eq!(
            repo.find_reference("refs/remotes/origin/HEAD")
                .unwrap()
                .symbolic_target()
                .ok()
                .flatten(),
            Some("refs/remotes/origin/trunk")
        );
        let origin_repo = git2::Repository::open(&origin).unwrap();
        assert_eq!(rev(&repo, "HEAD"), rev(&origin_repo, "trunk"));

        // A session worktree is a normal clone as far as git is concerned.
        let wt = crate::worktree::create_or_resume_worktree(&repo, &store, "s", "HEAD").unwrap();
        git(&wt, &["fetch", "-q"]);
        git(&wt, &["rev-parse", "--verify", "origin/feature"]);

        // New upstream work lands in the next session's base.
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let (repo, outcome) = clone_or_pull_ex(
            &tmp.path().join("store"),
            "acme",
            "proj",
            true,
            CloneStdio::Capture,
        )
        .unwrap();
        assert_eq!(outcome, PullOutcome::Fetched);
        assert_eq!(rev(&repo, "HEAD"), rev(&origin_repo, "trunk"));
        let wt2 = crate::worktree::create_or_resume_worktree(&repo, &store, "t", "HEAD").unwrap();
        let wt2_repo = git2::Repository::open(wt2).unwrap();
        assert_eq!(rev(&wt2_repo, "HEAD"), rev(&origin_repo, "trunk"));
        // The earlier session's branch is untouched by the fetch.
        let wt_repo = git2::Repository::open(&wt).unwrap();
        assert_ne!(rev(&wt_repo, "HEAD"), rev(&origin_repo, "trunk"));

        // A plain `git push` publishes the session branch (no -u needed),
        // and it stays "pushed" after the remote deletes it on merge.
        git(
            &wt,
            &["commit", "-q", "--allow-empty", "-m", "session work"],
        );
        let store_root = store.clone();
        let unpushed = || {
            crate::gitstatus::work_state(&wt, &store_root)
                .unwrap()
                .unpushed_commits
        };
        assert_eq!(unpushed(), 1);
        git(&wt, &["push", "-q"]);
        assert_eq!(unpushed(), 0);
        git(&origin, &["branch", "-D", "worktree-s"]);
        clone_or_pull_ex(
            &tmp.path().join("store"),
            "acme",
            "proj",
            true,
            CloneStdio::Capture,
        )
        .unwrap();
        assert_eq!(
            unpushed(),
            0,
            "a merged-and-deleted branch must stay pushed"
        );
    }

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

    /// Regression guard: a fast-forward-able pull on a checkout with
    /// uncommitted local changes must abort entirely —
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
