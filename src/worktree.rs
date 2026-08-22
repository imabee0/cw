use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};

/// Names that would collide with a real `cw` subcommand if allowed as a
/// worktree slug (`cw resume` would otherwise be ambiguous: "the `resume`
/// subcommand" or "a worktree literally named `resume`"). `help` is included
/// even though it isn't a named `Cmd` variant: clap treats `help` as a
/// special case inside the same subcommand-matching branch as named
/// subcommands (F12), so `cw help` never reaches slug parsing either.
pub const RESERVED_SLUGS: &[&str] = &[
    "resume",
    "clean",
    "doctor",
    "completions",
    "scratch",
    "help",
];

/// Validates a worktree slug against git's own ref-component rules, applied
/// per `/`-separated segment (F11 — a prior draft only blocked
/// `'+'`/`'..'`/absolute/whitespace ad hoc, which still admitted
/// `` ~ ^ : ? * [ \ ``, a leading `-`, and a trailing `.lock`, all of which
/// are either invalid in a git ref name or dangerous as a CLI arg — a slug
/// starting with `-` would otherwise be parsed by clap as a flag, not a
/// positional value).
///
/// The charset check alone (`[A-Za-z0-9._-]+`) doesn't reject a bare `.`/`..`
/// segment, a leading `-`, or a trailing `.lock` — those characters are all
/// individually allowed, just not in every position — so each is checked
/// explicitly alongside it. Together these SUBSUME the `'+'` rejection: `+`
/// just isn't in the allowed charset, so `"foo+bar"` is already invalid
/// before `flatten_slug` ever runs — which is exactly what makes
/// `flatten_slug`'s `/` -> `+` mapping injective (no raw slug this function
/// accepts can already contain the character flattening introduces).
pub fn validate_worktree_slug(s: &str) -> Result<()> {
    if s.is_empty() || s.trim().is_empty() {
        bail!("slug cannot be empty/whitespace");
    }
    if s.len() > 64 {
        bail!("slug too long (max 64 chars)");
    }
    if RESERVED_SLUGS.contains(&s) {
        bail!("slug '{s}' collides with a cw subcommand name — pick another");
    }
    for segment in s.split('/') {
        if segment.is_empty()
            || !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        {
            bail!("slug segment '{segment}' must match [A-Za-z0-9._-]+ (git ref rules)");
        }
        if segment == "."
            || segment == ".."
            || segment.starts_with('-')
            || segment.ends_with(".lock")
        {
            bail!("slug segment '{segment}' is not a usable git ref component");
        }
    }
    Ok(())
}

/// Mirrors Claude Code's own worktree naming scheme exactly (required for
/// interop with its EnterWorktree/subagent tooling): `/` in a slug becomes
/// `+` in the on-disk path and branch name.
pub fn flatten_slug(s: &str) -> String {
    s.replace('/', "+")
}

/// The on-disk path a worktree for `slug` would live at under `repo_root`,
/// plus whether it already exists there (the fast-resume predicate). Shared
/// by `create_or_resume_worktree` below and by callers (main.rs's
/// `worktree_precheck`) that need to know in advance whether a create call
/// will fast-resume or actually create — so the two checks can't drift apart
/// into disagreeing about what "already exists" means.
pub fn worktree_path_and_exists(repo_root: &Path, slug: &str) -> (PathBuf, bool) {
    let flat = flatten_slug(slug);
    let path = repo_root.join(".claude/worktrees").join(&flat);
    let existed = path.join(".git").exists();
    (path, existed)
}

/// The actual entry point every caller (default flow, `cw scratch`, §0a's
/// resume picker) uses. `create_worktree` below is force-create/reset (`-B`
/// semantics) — calling it unconditionally on every invocation would
/// silently discard in-progress work in an existing worktree on repeat runs.
/// This wrapper is what makes re-running `cw <same-slug>` a genuine no-op
/// fast-resume instead.
pub fn create_or_resume_worktree(
    repo: &git2::Repository,
    slug: &str,
    base_ref: &str,
) -> Result<PathBuf> {
    validate_worktree_slug(slug)?;
    let repo_root = repo.workdir().context("repo has no working directory")?;
    let (path, existed) = worktree_path_and_exists(repo_root, slug);
    if existed {
        return Ok(path); // fast-resume: skip creation AND both hooks entirely
    }
    create_worktree(repo, slug, base_ref)
}

fn create_worktree(repo: &git2::Repository, slug: &str, base_ref: &str) -> Result<PathBuf> {
    let flat = flatten_slug(slug);
    let branch_name = format!("worktree-{flat}");
    let path = repo
        .workdir()
        .context("repo has no working directory")?
        .join(".claude/worktrees")
        .join(&flat);

    // Zero-commit repos (a genuinely fresh `git init`, before §5n's
    // scratch-repo fix or a real empty GitHub repo) have no HEAD to peel —
    // surface a clear message here rather than an opaque git2::Error further
    // down.
    let target = repo
        .find_reference(base_ref)
        .and_then(|r| r.peel_to_commit())
        .context("repository has no commits yet — nothing to branch a worktree from")?;

    // -B semantics: force-create/reset the branch. Deliberate, mirrors
    // worktree.ts's `git worktree add -B worktree-<flat>` — matches Claude
    // Code's own worktree tooling exactly, not an independent design choice
    // (F10 — a prior draft of this plan never actually created this branch
    // at all; `repo.worktree(name, path, None)` would have named the branch
    // after the bare worktree name instead of `worktree-<flat>`, silently
    // breaking the interop this naming scheme exists for).
    let branch = repo.branch(&branch_name, &target, true)?;
    let branch_ref = branch.into_reference();
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(&branch_ref));

    // libgit2's `git_worktree_add` doesn't create intermediate directories —
    // `.claude/worktrees/` under a freshly cloned repo doesn't exist yet on
    // the very first worktree, and the call below fails outright without
    // this.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let result = match repo.worktree(&flat, &path, Some(&opts)) {
        Ok(_) => Ok(path.clone()),
        // Stale admin dir from a manually-deleted worktree directory: prune
        // the metadata, then retry once, git2-native (no shell-out).
        Err(_) if repo.find_worktree(&flat).is_ok() => {
            let wt = repo.find_worktree(&flat)?;
            wt.prune(Some(&mut git2::WorktreePruneOptions::new().valid(true)))?;
            repo.worktree(&flat, &path, Some(&opts))?;
            Ok(path.clone())
        }
        Err(e) => Err(e.into()),
    };

    // Append `.claude/worktrees/` to the PARENT repo's `.git/info/exclude`,
    // idempotently, right after a successful create — NOT a §5n scratch-only
    // fix (F5, the most consequential single catch in this round). Worktrees
    // live inside the main checkout; `gitstatus::is_dirty_repo` uses
    // include_untracked(true), so the moment any worktree exists, the parent
    // repo has an untracked `.claude/` entry and `sync::fetch_and_ff`'s
    // dirty guard starts rejecting every future `cw` pull on that repo,
    // forever — intermittently (repos that already .gitignore `.claude/`
    // are unaffected, which is exactly what let this slip past manual
    // testing).
    if result.is_ok() {
        append_line_if_missing(&repo.path().join("info/exclude"), "/.claude/worktrees/")?;
    }
    result
}

/// Appends `line` to the file at `path` unless it's already present
/// (idempotent — a second `create_worktree` call on the same repo must not
/// duplicate the exclude entry). Creates the file (and its parent dir, since
/// a freshly-`git init`'d repo may not have populated `.git/info/` yet) if
/// it doesn't exist.
fn append_line_if_missing(path: &Path, line: &str) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l == line) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {} for append", path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{line}")?;
    Ok(())
}

/// `cw clean` / worktree removal — signature corrected, branch cleanup added
/// (F9: the design pass's original form doesn't compile — `prune()` is an
/// INSTANCE method on `Worktree`, not `Worktree::prune(opts)` — and prune
/// alone leaks a branch per removal).
pub fn remove_worktree(repo: &git2::Repository, slug: &str) -> Result<()> {
    let flat = flatten_slug(slug);
    let wt = repo.find_worktree(&flat)?;
    // prune() is an INSTANCE method on Worktree, not `Worktree::prune(opts)` —
    // and its parameter is `Option<&mut WorktreePruneOptions>`.
    wt.prune(Some(
        &mut git2::WorktreePruneOptions::new()
            .valid(true)
            .working_tree(true),
    ))?;
    // prune() removes the checkout + `.git/worktrees/<name>` admin entry,
    // but NOT the branch itself — worktree.ts explicitly deletes it too
    // (`worktree-<flat>` would otherwise leak, one per `cw clean`d
    // worktree, forever). Tolerate NotFound in case it was already removed
    // by hand.
    match repo.find_branch(&format!("worktree-{flat}"), git2::BranchType::Local) {
        Ok(mut b) => {
            b.delete()?;
        }
        Err(e) if e.code() == git2::ErrorCode::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// One worktree found by `scan_worktrees`, ready for `picker.rs`'s
/// idle/dirty annotation (§5l) and `clean.rs`'s removal flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// `"owner/repo"` — matches the label `resolve_local_path` derives the
    /// on-disk layout from (§5a/§5g).
    pub repo: String,
    pub slug: String,
    pub path: PathBuf,
    pub mtime: SystemTime,
}

/// Read a directory's entries, tolerating a missing/unreadable dir as
/// "nothing here" rather than propagating an `Err` — the whole point of the
/// tolerant scan below (fresh-machine case: `root` itself doesn't exist yet;
/// one repo's dir is mid-deletion or permission-denied; neither should abort
/// the scan of every other repo).
fn read_dir_ok(path: &Path) -> impl Iterator<Item = fs::DirEntry> {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
}

/// Scans the two-level `<root>/<owner>/<repo>/.claude/worktrees/<slug>`
/// layout (this was the advisor's #2, the most serious catch: without this
/// fix, `cw resume` and `cw clean` silently return nothing under the
/// owner-nested repo layout §5a introduced). `clean.rs` reuses this same
/// function rather than duplicating a one-level scan.
pub fn scan_worktrees(root: &Path) -> Result<Vec<WorktreeEntry>> {
    let mut out = vec![];
    for owner_dir in read_dir_ok(root) {
        // level 1: <root>/<owner>/
        for repo_dir in read_dir_ok(&owner_dir.path()) {
            // level 2: <root>/<owner>/<repo>/
            let repo_label = format!(
                "{}/{}",
                owner_dir.file_name().to_string_lossy(),
                repo_dir.file_name().to_string_lossy()
            );
            let wt_root = repo_dir.path().join(".claude/worktrees");
            for e in read_dir_ok(&wt_root) {
                // level 3: .../.claude/worktrees/<slug>/
                let path = e.path();
                if !path.join(".git").exists() {
                    continue;
                }
                out.push(WorktreeEntry {
                    repo: repo_label.clone(),
                    slug: path
                        .file_name()
                        .expect("invariant: read_dir entry always has a file name")
                        .to_string_lossy()
                        .into_owned(),
                    // `.ok()`-chained, not `?` (F34) — one unreadable entry
                    // (permissions, a race with concurrent deletion) would
                    // otherwise abort the ENTIRE scan via `?`, defeating
                    // read_dir_ok's whole point of being tolerant.
                    mtime: path
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH),
                    path,
                });
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.mtime));
    Ok(out)
}

/// Symlinks each configured `symlink_dirs` entry (config.rs's
/// `Config::symlink_dirs`, §3/§5c-note) from the repo's main checkout into a
/// freshly created worktree, when present in the main checkout — so e.g.
/// `node_modules` doesn't need reinstalling in every worktree. Mirrors
/// Claude Code's own `--worktree` behavior; unlike that tool, cw's own
/// default `symlink_dirs` list is empty (§3) — this only runs at all when
/// the user has explicitly opted a dir in, because a symlinked
/// `node_modules` is shared mutable state across every worktree of the repo.
/// A dir missing from the main checkout (nothing installed yet) or already
/// present in the worktree (re-running against an existing worktree) is
/// silently skipped, not an error.
pub fn symlink_shared_dirs(repo_root: &Path, worktree_path: &Path, dirs: &[String]) -> Result<()> {
    for dir in dirs {
        let src = repo_root.join(dir);
        if !src.exists() {
            continue;
        }
        let dst = worktree_path.join(dir);
        if dst.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&src, &dst)
            .with_context(|| format!("symlinking {} -> {}", dst.display(), src.display()))?;
        #[cfg(not(unix))]
        {
            bail!("symlink_dirs is only supported on unix targets");
        }
    }
    Ok(())
}

// GitHub usernames can never start with '.' — guaranteed no collision with a
// real cloned repo under <root>. `pub` so main.rs can build the same
// "owner/repo"-shaped label `scan_worktrees` derives, for `cw scratch`'s
// dispatch and its `--dry-run` preview, without duplicating these literals.
pub const SCRATCH_OWNER: &str = ".scratch";
pub const SCRATCH_REPO: &str = "workspace";

/// `.scratch/workspace` displays as `scratch` — display-label only, never
/// affects the underlying repo/slug values `remove_worktree`/
/// `create_or_resume_worktree` operate on. Shared by every worktree-table
/// renderer (`tui::model`'s repo/worktree screens) so the mapping exists in
/// exactly one place.
pub fn display_repo_label(repo: &str) -> String {
    if repo == format!("{SCRATCH_OWNER}/{SCRATCH_REPO}") {
        "scratch".to_string()
    } else {
        repo.to_string()
    }
}

/// `cw scratch` — repo-less worktrees, reusing the existing worktree
/// machinery unchanged. Lazily creates a synthetic repo at
/// `<root>/.scratch/workspace` with one empty commit (so
/// `create_or_resume_worktree`'s `find_reference(base_ref)?.peel_to_commit()`
/// step has something to branch from) the first time it's needed, then
/// returns its path on every later call.
pub fn ensure_scratch_repo(root: &Path) -> Result<PathBuf> {
    let path = root.join(SCRATCH_OWNER).join(SCRATCH_REPO);
    if path.join(".git").exists() {
        return Ok(path);
    }
    fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
    let repo = git2::Repository::init(&path)
        .with_context(|| format!("initializing scratch repo at {}", path.display()))?;
    let sig = git2::Config::open_default()
        .and_then(|cfg| {
            git2::Signature::now(
                &cfg.get_string("user.name").unwrap_or_else(|_| "cw".into()),
                &cfg.get_string("user.email")
                    .unwrap_or_else(|_| "cw@localhost".into()),
            )
        })
        .or_else(|_| git2::Signature::now("cw", "cw@localhost"))?;
    let tree_id = repo.treebuilder(None)?.write()?; // empty tree
    let tree = repo.find_tree(tree_id)?;
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "cw: scratch workspace root",
        &tree,
        &[],
    )?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_repo_label_maps_scratch() {
        assert_eq!(display_repo_label(".scratch/workspace"), "scratch");
        assert_eq!(display_repo_label("imabee0/cw"), "imabee0/cw");
    }

    /// Inits a non-bare repo at `path` with one commit on its default
    /// branch, so `create_worktree`'s `find_reference("HEAD")?.peel_to_commit()`
    /// step has something to branch a worktree from.
    fn init_repo_with_commit(path: &Path) -> git2::Repository {
        let repo = git2::Repository::init(path).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        fs::write(path.join("README.md"), "hi").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("README.md")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }
        repo
    }

    #[test]
    fn flatten_slug_no_collision() {
        // A raw '+' would let `flatten_slug` collide two different slugs —
        // "foo/bar" and "foo+bar" both flatten to the literal string
        // "foo+bar" — but that collision is unreachable in practice: '+'
        // sits outside the per-segment `[A-Za-z0-9._-]+` allowlist, so
        // `validate_worktree_slug` rejects "foo+bar" before `flatten_slug`
        // ever runs on it. That rejection, not `flatten_slug` itself, is
        // what makes the '/' -> '+' mapping injective over every slug cw
        // actually accepts.
        assert_eq!(flatten_slug("foo/bar"), "foo+bar");
        assert_eq!(flatten_slug("foo/bar"), flatten_slug("foo+bar"));
        assert!(
            validate_worktree_slug("foo+bar").is_err(),
            "'+' must be rejected pre-flatten so the collision above never reaches a real worktree path"
        );
        assert!(validate_worktree_slug("foo/bar").is_ok());
    }

    #[test]
    fn validate_slug_rejects_invalid_segments() {
        for bad in [
            "../x",
            "/abs",
            "",
            "   ",
            "~x",
            "a b",
            "-lead",
            &"a".repeat(65),
        ] {
            assert!(
                validate_worktree_slug(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
        assert!(validate_worktree_slug("feature").is_ok());
        assert!(validate_worktree_slug("foo/bar").is_ok());
    }

    #[test]
    fn validate_slug_rejects_reserved_names() {
        for reserved in [
            "resume",
            "clean",
            "doctor",
            "completions",
            "scratch",
            "help",
        ] {
            assert!(
                validate_worktree_slug(reserved).is_err(),
                "expected reserved slug '{reserved}' to be rejected"
            );
        }
    }

    #[test]
    fn create_worktree_branch_is_named_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo_with_commit(dir.path());

        create_worktree(&repo, "foo", "HEAD").unwrap();

        assert!(
            repo.find_branch("worktree-foo", git2::BranchType::Local)
                .is_ok(),
            "create_worktree must create a branch literally named worktree-<flat>, not <flat>"
        );
        assert!(
            repo.find_branch("foo", git2::BranchType::Local).is_err(),
            "must not also (or instead) create a branch named after the bare slug"
        );
    }

    #[test]
    fn create_worktree_updates_info_exclude() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo_with_commit(dir.path());

        create_worktree(&repo, "foo", "HEAD").unwrap();
        create_worktree(&repo, "bar", "HEAD").unwrap();

        let exclude_path = repo.path().join("info/exclude");
        let content = fs::read_to_string(&exclude_path).unwrap();
        let occurrences = content
            .lines()
            .filter(|l| *l == "/.claude/worktrees/")
            .count();
        assert_eq!(
            occurrences, 1,
            "the exclude line must appear exactly once across two create_worktree calls, not once per call"
        );
    }

    #[test]
    fn scan_worktrees_two_levels() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Correct two-level layout: <root>/<owner>/<repo>/.claude/worktrees/<slug>
        let good = root
            .join("owner")
            .join("repo")
            .join(".claude/worktrees/slug-a");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join(".git"), "gitdir: ../../.git/worktrees/slug-a\n").unwrap();

        // One-level-only fixture (missing the owner nesting) must NOT be found.
        let flat = root.join("flat-repo").join(".claude/worktrees/slug-b");
        fs::create_dir_all(&flat).unwrap();
        fs::write(flat.join(".git"), "gitdir: ../../.git/worktrees/slug-b\n").unwrap();

        let entries = scan_worktrees(root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].repo, "owner/repo");
        assert_eq!(entries[0].slug, "slug-a");
    }

    #[test]
    fn scan_worktrees_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let entries = scan_worktrees(&missing).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn scan_worktrees_tolerates_unreadable_entry() {
        // `path.metadata()` can't be made to fail deterministically once
        // `path.join(".git").exists()` has already succeeded on the SAME
        // path — `stat()` only checks search permission on ancestor
        // directories, never any permission bit on the target itself, so
        // there's no non-root, non-racy way to make the two disagree. The
        // practically-reproducible half of this regression guard (F34): one
        // repo whose worktrees directory can't be traversed at all must not
        // abort the scan of every other repo. `read_dir_ok`'s `.ok()`
        // chaining (not `?`) is what makes that true; this test would fail
        // if that were reverted to `?`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let good_wt = root
            .join("owner")
            .join("repo-good")
            .join(".claude/worktrees/slug");
        fs::create_dir_all(&good_wt).unwrap();
        fs::write(good_wt.join(".git"), "gitdir: ../../.git/worktrees/slug\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let bad_repo = root.join("owner").join("repo-bad");
            let bad_wt = bad_repo.join(".claude/worktrees/slug");
            fs::create_dir_all(&bad_wt).unwrap();
            fs::write(bad_wt.join(".git"), "gitdir: ../../.git/worktrees/slug\n").unwrap();

            let mut perms = fs::metadata(&bad_repo).unwrap().permissions();
            perms.set_mode(0o000);
            fs::set_permissions(&bad_repo, perms.clone()).unwrap();

            let result = scan_worktrees(root);

            // Restore permissions before the tempdir's Drop tries to remove it.
            perms.set_mode(0o755);
            fs::set_permissions(&bad_repo, perms).unwrap();

            let entries = result.unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].repo, "owner/repo-good");
        }
    }
}
