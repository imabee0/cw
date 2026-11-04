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
/// subcommands, so `cw help` never reaches slug parsing either.
pub const RESERVED_SLUGS: &[&str] = &[
    "finish",
    "resume",
    "clean",
    "doctor",
    "completions",
    "scratch",
    "help",
];

/// Validates a worktree slug: at most 64 chars, not a subcommand name, and
/// each `/`-separated segment matches `[A-Za-z0-9._-]+` — an allowlist, so
/// `+` (reserved by `flatten_slug`), whitespace and every git-ref-invalid
/// character are out — and is not `.`/`..`, doesn't start with `-` (clap
/// would parse it as a flag), and doesn't end in `.lock`.
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

/// Reverses `flatten_slug` on a slug read back off disk (e.g.
/// `scan_worktrees`'s `entry.slug`, taken from the flattened directory
/// name). Safe precisely because `validate_worktree_slug` rejects a literal
/// `+` in any raw slug segment — no slug `create_or_resume_worktree` ever
/// accepted could already contain the character flattening introduces, so
/// this reversal can't misfire on a slug that legitimately contained `+`.
/// `dashboard.rs`'s `DashboardModel::start_pending` uses this on every
/// `WorktreeSelection::Existing` row before it ever reaches
/// `create_or_resume_worktree`'s `validate_worktree_slug` step.
pub fn unflatten_slug(flat: &str) -> String {
    flat.replace('+', "/")
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

/// The entry point every caller (default flow, `cw scratch`, the
/// dashboard) uses. An existing worktree at the slug's path is resumed as-is
/// (after checking it really belongs to `repo`) — re-running `cw
/// <same-slug>` is a genuine no-op fast-resume. Otherwise one is created on
/// branch `worktree-<flat>`: a new branch at `base_ref`, or, if that branch
/// already exists (its checkout was deleted by hand), the existing branch
/// with its commits intact — never reset.
///
/// `repo_root` is the path the caller resolved from its configured `root`,
/// never one derived from `repo`: git2 reports a repo's location with
/// symlinks resolved (macOS `/var` → `/private/var`, a symlinked `$HOME`),
/// which would differ textually from the paths cw scans and displays.
pub fn create_or_resume_worktree(
    repo: &git2::Repository,
    repo_root: &Path,
    slug: &str,
    base_ref: &str,
) -> Result<PathBuf> {
    validate_worktree_slug(slug)?;
    let (path, existed) = worktree_path_and_exists(repo_root, slug);
    if existed {
        let existing = git2::Repository::open(&path).context("opening existing worktree")?;
        if !existing.is_worktree()
            || existing.commondir().canonicalize()? != repo.commondir().canonicalize()?
        {
            anyhow::bail!("{} is not a worktree of this repository", path.display());
        }
        return Ok(path); // fast-resume: skip creation AND both hooks entirely
    }
    create_worktree(repo, &path, slug, base_ref)
}

fn create_worktree(
    repo: &git2::Repository,
    path: &Path,
    slug: &str,
    base_ref: &str,
) -> Result<PathBuf> {
    let flat = flatten_slug(slug);
    let branch_name = format!("worktree-{flat}");
    let path = path.to_path_buf();

    // Zero-commit repos (a real empty GitHub repo) have no HEAD to peel —
    // surface a clear message here rather than an opaque git2::Error further
    // down.
    let target = repo
        .find_reference(base_ref)
        .and_then(|r| r.peel_to_commit())
        .context("repository has no commits yet — nothing to branch a worktree from")?;

    // Reuse an orphaned branch without resetting its commits.
    let branch = match repo.find_branch(&branch_name, git2::BranchType::Local) {
        Ok(branch) => branch,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            repo.branch(&branch_name, &target, false)?
        }
        Err(e) => return Err(e.into()),
    };
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
            if wt.path().exists() || !matches!(wt.is_locked()?, git2::WorktreeLockStatus::Unlocked)
            {
                anyhow::bail!(
                    "worktree metadata is still active or locked; refusing to prune {}",
                    wt.path().display()
                );
            }
            wt.prune(None)?;
            repo.worktree(&flat, &path, Some(&opts))?;
            Ok(path.clone())
        }
        Err(e) => Err(e.into()),
    };

    // Append `/.claude/worktrees/` to the repo's `info/exclude`, idempotently,
    // after every successful create. Needed for full checkouts (scratch, or a
    // `root` the user populated): worktrees live inside the checkout, and
    // `gitstatus::is_dirty_repo` counts untracked files, so without it the
    // first worktree would make `sync::fetch_and_ff`'s dirty guard reject
    // every future update of that repo. Harmless for a bare store, which has
    // no working tree to be dirty.
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

/// Removes a worktree: prunes its checkout and admin entry, then deletes its
/// `worktree-<flat>` branch (prune alone would leak one branch per removal).
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

/// A timestamp-based slug for an auto-generated worktree (no explicit SLUG
/// given) — `main.rs`/`dashboard.rs` share this so a fresh worktree's name
/// is generated in exactly one place.
pub fn generate_timestamp_slug() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

/// One worktree found by `scan_worktrees`, ready for `dashboard.rs`'s
/// idle/dirty annotation and removal flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// `"owner/repo"` — matches the label `resolve_local_path` derives the
    /// on-disk layout from.
    pub repo: String,
    pub slug: String,
    pub path: PathBuf,
    pub mtime: SystemTime,
}

impl WorktreeEntry {
    /// The repo (or bare store container) this worktree hangs off:
    /// `<repo>/.claude/worktrees/<slug>` → `<repo>`. Derived from the
    /// entry's own path, so it can never disagree with where the worktree
    /// actually is.
    pub fn repo_root(&self) -> PathBuf {
        self.path
            .ancestors()
            .nth(3)
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }
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
/// layout for every session under `root`. The single scanner behind the
/// dashboard, `cw finish`, and the fast path's resume check.
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
                    // `.ok()`-chained, not `?` — one unreadable entry
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

/// Whether `repo_root` holds a cw-managed bare store (`clone --bare`, no
/// checkout of its own) rather than a full checkout.
pub fn is_bare_store(repo_root: &Path) -> bool {
    git2::Repository::open(repo_root).is_ok_and(|r| r.is_bare())
}

/// Symlinks each configured `symlink_dirs` entry into a freshly created
/// worktree, so e.g. `node_modules` is installed once per repo instead of
/// once per session. Off unless the user opts a dir in (the default list is
/// empty): a symlinked dir is shared mutable state across every session of
/// the repo.
///
/// The shared copy lives in the repo's directory under `root`:
/// - bare store: `<store>/<dir>`, created empty on first use — the first
///   session's install populates it through the symlink, later sessions
///   reuse it, and it goes away with the store when the last session
///   finishes;
/// - full checkout: the checkout's own `<dir>`, linked only if present.
///
/// Each linked dir is also added to the repo's shared `info/exclude`: a
/// symlink doesn't match a `node_modules/`-style (directory-only) ignore
/// pattern, so it would otherwise show as an untracked file and make every
/// session look like it has unsaved work. A dir already present in the
/// worktree (tracked, or a re-run) is left alone.
pub fn symlink_shared_dirs(repo_root: &Path, worktree_path: &Path, dirs: &[String]) -> Result<()> {
    if dirs.is_empty() {
        return Ok(());
    }
    let bare = is_bare_store(repo_root);
    let exclude = git2::Repository::open(repo_root)
        .ok()
        .map(|r| r.commondir().join("info/exclude"));
    for dir in dirs {
        let src = repo_root.join(dir);
        if !src.exists() {
            if !bare {
                continue;
            }
            fs::create_dir_all(&src).with_context(|| format!("creating {}", src.display()))?;
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
        if let Some(exclude) = &exclude {
            append_line_if_missing(exclude, &format!("/{}", dir.trim_matches('/')))?;
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
/// renderer (`tui::model`'s dashboard pane) so the mapping exists in exactly
/// one place.
pub fn display_repo_label(repo: &str) -> String {
    if repo == format!("{SCRATCH_OWNER}/{SCRATCH_REPO}") {
        "scratch".to_string()
    } else {
        repo.to_string()
    }
}

/// One of the two rows the dashboard's worktree pane can commit: a real,
/// previously-created worktree, or the synthetic "+ new worktree" row
/// (offered only when the pane's scope has a concrete repo to create one
/// under — see `tui::model::Scope`).
#[derive(Debug, Clone)]
pub enum WorktreeSelection {
    Existing(WorktreeEntry),
    New,
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
    fn existing_branch_is_not_reset_when_creating_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_repo_with_commit(tmp.path());
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let oid = repo
            .commit(
                None,
                &sig,
                &sig,
                "saved work",
                &base.tree().unwrap(),
                &[&base],
            )
            .unwrap();
        repo.branch("worktree-saved", &repo.find_commit(oid).unwrap(), false)
            .unwrap();
        let path = create_or_resume_worktree(&repo, tmp.path(), "saved", "HEAD").unwrap();
        assert_eq!(
            git2::Repository::open(path)
                .unwrap()
                .head()
                .unwrap()
                .target(),
            Some(oid)
        );
    }

    /// `symlink_dirs` on a bare store: one shared copy per repo, created on
    /// first use, visible from every session, never counted as unsaved work,
    /// and never deleted by removing a single session (prune must not follow
    /// the symlink) — only with the store itself.
    #[cfg(unix)]
    #[test]
    fn symlink_dirs_share_one_copy_across_sessions_of_a_bare_store() {
        let tmp = tempfile::tempdir().unwrap();
        let container = tmp.path().join("acme/proj");
        let repo = git2::Repository::init_bare(container.join(".git")).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        let dirs = vec!["node_modules".to_string()];

        let a = create_or_resume_worktree(&repo, &container, "a", "HEAD").unwrap();
        symlink_shared_dirs(&container, &a, &dirs).unwrap();
        assert!(
            container.join("node_modules").is_dir(),
            "created on first use"
        );
        fs::write(a.join("node_modules/dep.js"), "x").unwrap();

        let b = create_or_resume_worktree(&repo, &container, "b", "HEAD").unwrap();
        symlink_shared_dirs(&container, &b, &dirs).unwrap();
        assert_eq!(
            fs::read_to_string(b.join("node_modules/dep.js")).unwrap(),
            "x"
        );

        let state = crate::gitstatus::work_state(&a, &container).unwrap();
        assert_eq!(
            state.changed_files, 0,
            "the symlink must not read as unsaved work"
        );

        remove_worktree(&repo, "a").unwrap();
        assert!(!a.exists());
        assert!(
            container.join("node_modules/dep.js").exists(),
            "removing one session must not follow the symlink into the shared copy"
        );
    }

    /// git2 reports a repo's location with symlinks resolved; the worktree
    /// path must still be built from the root the caller configured, or it
    /// drifts from what `scan_worktrees` and the dashboard show (macOS's
    /// `/var` → `/private/var` tempdirs, a symlinked `$HOME`).
    #[cfg(unix)]
    #[test]
    fn worktree_path_keeps_the_callers_root_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let link = tmp.path().join("link");
        fs::create_dir_all(real.join("acme/proj")).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let container = link.join("acme/proj");
        let repo = git2::Repository::init_bare(container.join(".git")).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        let repo = git2::Repository::open(&container).unwrap();

        let created = create_or_resume_worktree(&repo, &container, "s", "HEAD").unwrap();
        assert_eq!(created, container.join(".claude/worktrees/s"));
        let scanned = scan_worktrees(&link).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].path, created);
        let resumed = create_or_resume_worktree(&repo, &container, "s", "HEAD").unwrap();
        assert_eq!(resumed, created);
    }

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

    /// Regression guard: `scan_worktrees`'s `entry.slug` is read off the
    /// on-disk (already flattened) directory name. Feeding that verbatim
    /// into `create_or_resume_worktree` — whose first step is
    /// `validate_worktree_slug`, which rejects a literal `+` by design —
    /// would make resuming any worktree whose raw slug contained `/` fail
    /// every time. This drives the exact round trip
    /// `DashboardModel::start_pending`'s `Existing` arms perform, without
    /// needing an interactive dashboard session.
    #[test]
    fn resumed_slug_survives_scan_and_unflatten_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo_dir = root.join("acme").join("proj");
        fs::create_dir_all(&repo_dir).unwrap();
        let repo = init_repo_with_commit(&repo_dir);

        let raw_slug = "myfeat/x";
        let created = create_or_resume_worktree(&repo, &repo_dir, raw_slug, "HEAD").unwrap();

        let scanned = scan_worktrees(root).unwrap();
        let entry = scanned
            .into_iter()
            .find(|e| e.repo == "acme/proj")
            .expect("scan_worktrees must find the worktree just created");
        // Confirms the premise: what comes back off disk IS the flattened
        // form, not the raw slug.
        assert_eq!(entry.slug, "myfeat+x");

        let resumed_slug = unflatten_slug(&entry.slug);
        assert_eq!(resumed_slug, raw_slug);
        assert!(validate_worktree_slug(&resumed_slug).is_ok());

        // The actual regression: feeding the unflattened slug back through
        // `create_or_resume_worktree` must fast-resume the same path, not
        // error on a rejected '+' segment.
        let resumed = create_or_resume_worktree(&repo, &repo_dir, &resumed_slug, "HEAD").unwrap();
        assert_eq!(resumed, created);
    }

    #[test]
    fn unflatten_slug_reverses_flatten() {
        assert_eq!(unflatten_slug("foo+bar"), "foo/bar");
        assert_eq!(unflatten_slug("plain"), "plain");
        assert_eq!(unflatten_slug(&flatten_slug("a/b/c")), "a/b/c");
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

        create_worktree(
            &repo,
            &dir.path().join(".claude/worktrees/foo"),
            "foo",
            "HEAD",
        )
        .unwrap();

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

        create_worktree(
            &repo,
            &dir.path().join(".claude/worktrees/foo"),
            "foo",
            "HEAD",
        )
        .unwrap();
        create_worktree(
            &repo,
            &dir.path().join(".claude/worktrees/bar"),
            "bar",
            "HEAD",
        )
        .unwrap();

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
        // practically-reproducible half of this regression guard: one
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
