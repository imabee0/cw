use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tracing::warn;

/// Parses `.worktreeinclude` pattern content (gitignore syntax — a plain
/// pattern means "copy this", a `!`-prefixed pattern means "don't", exactly
/// like `.gitignore`'s ignore/whitelist semantics, just applied to what gets
/// copied into a new worktree instead of what git tracks) into a matcher
/// rooted at `root`.
///
/// CRLF/BOM-tolerant: a leading UTF-8 BOM is stripped from the
/// first line, and each line's trailing `\r` is trimmed, before being handed
/// to `GitignoreBuilder` — a `.worktreeinclude` authored on Windows (CRLF
/// line endings) or saved by an editor that writes a BOM must match
/// identically to a plain-LF, no-BOM file.
pub fn parse_patterns(root: &Path, content: &str) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    for (i, raw_line) in content.lines().enumerate() {
        // `str::lines()` already splits on "\n" and "\r\n", but a lone
        // trailing '\r' (e.g. content that arrived pre-split some other way)
        // is trimmed explicitly too, rather than relying on that alone.
        let mut line = raw_line.trim_end_matches('\r');
        if i == 0 {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        builder
            .add_line(None, line)
            .with_context(|| format!("parsing .worktreeinclude line {}: {line:?}", i + 1))?;
    }
    builder.build().context("building .worktreeinclude matcher")
}

/// One file `.worktreeinclude` matched but that could not be copied into the
/// new worktree. Collected rather than `?`-propagated, so a
/// single unreadable/permission-denied file doesn't abort worktree creation.
#[derive(Debug)]
pub struct CopyFailure {
    pub path: PathBuf,
    pub error: io::Error,
}

/// Copies `src` to `dst`, preserving a symlink rather than following it
///: if `src` is itself a symlink, `dst` becomes a symlink to the
/// same target, instead of `fs::copy` dereferencing it and copying whatever
/// it points to (which could be arbitrarily large, or somewhere the caller
/// never intended to read — e.g. a `.env` symlinked to a password-manager
/// mount).
fn copy_preserving_symlinks(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        // A previous run (or symlink_shared_dirs) may have already left
        // something at `dst`; symlink() errors if the destination exists.
        let _ = fs::remove_file(dst);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, dst)?;
        }
        #[cfg(not(unix))]
        {
            // No non-unix target is in scope for cw (macOS + Linux only) —
            // fall back to a plain copy rather than leaving dst missing.
            fs::copy(&target, dst)?;
        }
        Ok(())
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

/// Copies every entry in `matches` (relative paths, as produced by walking a
/// `.worktreeinclude` matcher) from `repo_root` to the same relative path
/// under `worktree_path`. Continues past a per-file failure instead of
/// aborting the whole batch — failures are collected and handed
/// back to the caller to report as a warning list.
pub fn copy_matches(
    repo_root: &Path,
    worktree_path: &Path,
    matches: &[PathBuf],
) -> Vec<CopyFailure> {
    let mut failures = Vec::new();
    for rel in matches {
        let src = repo_root.join(rel);
        let dst = worktree_path.join(rel);
        if let Err(error) = copy_preserving_symlinks(&src, &dst) {
            failures.push(CopyFailure {
                path: rel.clone(),
                error,
            });
        }
    }
    failures
}

/// Walks `root` (skipping `.git`) and returns the relative paths of every
/// FILE (or symlink — never recursed into, so a symlinked directory is
/// treated as a leaf) that `gi` matches. Directories that don't themselves
/// match are still walked, since `.worktreeinclude` patterns like
/// `config/**/*.key` target files nested inside otherwise-unmatched dirs.
fn walk_matches(root: &Path, gi: &Gitignore) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(root, root, gi, &mut out);
    out
}

fn walk_dir(root: &Path, dir: &Path, gi: &Gitignore, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        if rel
            .components()
            .next()
            .is_some_and(|c| c.as_os_str() == ".git")
        {
            continue;
        }
        let is_symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        let is_dir = !is_symlink && entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        if !is_dir && gi.matched(&rel, false).is_ignore() {
            out.push(rel);
        } else if is_dir {
            if gi.matched(&rel, true).is_ignore() {
                walk_all_files(root, &path, out);
            } else {
                walk_dir(root, &path, gi, out);
            }
        }
    }
}

/// Once a directory itself has matched `.worktreeinclude` (e.g. a pattern
/// like `assets/`), every file beneath it is included — no need to keep
/// matching individual files.
fn walk_all_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(Result::ok) {
        if entry.file_name() == ".git" {
            // A matched directory can itself contain a nested repo (a
            // submodule) — never sweep its `.git` into the worktree, same
            // guard `walk_dir` applies at the top level.
            continue;
        }
        let path = entry.path();
        let is_symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        let is_dir = !is_symlink && entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            walk_all_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_path_buf());
        }
    }
}

/// Where a new worktree's `.worktreeinclude` files (untracked local files
/// like `.env`) are copied from:
/// - full checkout: the checkout itself;
/// - bare store (no checkout): the repo's most recently used *other*
///   session worktree. With no other session there's nothing to copy —
///   those files only ever lived in sessions, and went with them.
fn include_source(repo_root: &Path, worktree_path: &Path) -> Option<PathBuf> {
    if !crate::worktree::is_bare_store(repo_root) {
        return Some(repo_root.to_path_buf());
    }
    fs::read_dir(repo_root.join(".claude/worktrees"))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p != worktree_path && p.join(".git").exists())
        .max_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        })
}

/// Copies the untracked files a `.worktreeinclude` names into a new
/// worktree, from `include_source`. The pattern file is the new worktree's
/// own (tracked) `.worktreeinclude`, falling back to the source's. No
/// pattern file, or no source, is a silent no-op — most repos have neither.
/// A pattern file matching zero files (typo'd pattern) logs a warning
/// instead of silently no-op'ing. Per-file copy failures are collected and
/// returned rather than aborting worktree creation.
pub fn apply_worktreeinclude(repo_root: &Path, worktree_path: &Path) -> Result<Vec<CopyFailure>> {
    let Some(source) = include_source(repo_root, worktree_path) else {
        return Ok(Vec::new());
    };
    let mut content = None;
    let mut pattern_path = PathBuf::new();
    for candidate in [
        worktree_path.join(".worktreeinclude"),
        source.join(".worktreeinclude"),
    ] {
        match fs::read_to_string(&candidate) {
            Ok(c) => {
                content = Some(c);
                pattern_path = candidate;
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", candidate.display())),
        }
    }
    let Some(content) = content else {
        return Ok(Vec::new());
    };

    let gi = parse_patterns(&source, &content)?;
    let mut matches = walk_matches(&source, &gi);
    if matches.is_empty() {
        warn!(
            "{} matched zero files — check the patterns for typos",
            pattern_path.display()
        );
        return Ok(Vec::new());
    }
    // `.worktreeinclude` is for untracked local files. Never overwrite a
    // file the new worktree tracks: the source (especially an older session)
    // may hold a stale or locally-edited copy of it.
    if let Ok(index) = git2::Repository::open(worktree_path).and_then(|r| r.index()) {
        matches.retain(|rel| index.get_path(rel, 0).is_none());
    }

    Ok(copy_matches(&source, worktree_path, &matches))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare store has no checkout to copy from: untracked files come from
    /// the most recent other session, using the new worktree's own tracked
    /// `.worktreeinclude` — and a file the new worktree tracks is never
    /// overwritten with that session's copy.
    #[test]
    fn bare_store_copies_untracked_files_from_another_session() {
        let tmp = tempfile::tempdir().unwrap();
        let container = tmp.path().join("acme/proj");
        let repo = git2::Repository::init_bare(container.join(".git")).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let mut tb = repo.treebuilder(None).unwrap();
        for (name, body) in [(".worktreeinclude", ".env\nREADME\n"), ("README", "fresh")] {
            let blob = repo.blob(body.as_bytes()).unwrap();
            tb.insert(name, blob, 0o100644).unwrap();
        }
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();

        // The first session has nothing to copy from — a no-op, not an error.
        let a = crate::worktree::create_or_resume_worktree(&repo, &container, "a", "HEAD").unwrap();
        assert!(apply_worktreeinclude(&container, &a).unwrap().is_empty());
        assert!(!a.join(".env").exists());

        fs::write(a.join(".env"), "SECRET=1").unwrap();
        fs::write(a.join("README"), "a's local edit").unwrap();

        let b = crate::worktree::create_or_resume_worktree(&repo, &container, "b", "HEAD").unwrap();
        assert!(apply_worktreeinclude(&container, &b).unwrap().is_empty());
        assert_eq!(fs::read_to_string(b.join(".env")).unwrap(), "SECRET=1");
        assert_eq!(
            fs::read_to_string(b.join("README")).unwrap(),
            "fresh",
            "a tracked file must keep the new worktree's version"
        );
    }

    #[test]
    fn worktreeinclude_matches_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let gi = parse_patterns(root, ".env\nconfig/**/*.key\n!config/public.key\n").unwrap();

        assert!(gi.matched(Path::new(".env"), false).is_ignore());
        assert!(gi
            .matched(Path::new("config/secret.key"), false)
            .is_ignore());
        assert!(
            !gi.matched(Path::new("config/public.key"), false)
                .is_ignore(),
            "negation pattern must exclude a file the earlier pattern matched"
        );
        assert!(!gi.matched(Path::new("README.md"), false).is_ignore());
    }

    #[test]
    fn worktreeinclude_handles_crlf_bom() {
        // A lone BOM is the part that genuinely breaks matching without the
        // fix: `str::lines()` already splits CRLF cleanly, so without
        // BOM-stripping the first pattern would parse as "\u{feff}.env" and
        // silently fail to match ".env" at all.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = "\u{feff}.env\r\nconfig/**/*.key\r\n";
        let gi = parse_patterns(root, content).unwrap();

        assert!(
            gi.matched(Path::new(".env"), false).is_ignore(),
            "BOM-prefixed first line must still match .env"
        );
        assert!(gi
            .matched(Path::new("config/secret.key"), false)
            .is_ignore());
    }

    #[test]
    fn worktreeinclude_continues_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        let worktree = dir.path().join("wt");
        fs::create_dir_all(&repo_root).unwrap();
        fs::create_dir_all(&worktree).unwrap();

        fs::write(repo_root.join("one.txt"), "1").unwrap();
        // "two.txt" deliberately does not exist — the failure this test
        // exercises. `copy_preserving_symlinks` fails on the missing source
        // without ever touching "three.txt".
        fs::write(repo_root.join("three.txt"), "3").unwrap();

        let matches = vec![
            PathBuf::from("one.txt"),
            PathBuf::from("two.txt"),
            PathBuf::from("three.txt"),
        ];
        let failures = copy_matches(&repo_root, &worktree, &matches);

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].path, PathBuf::from("two.txt"));
        assert!(
            worktree.join("one.txt").exists(),
            "item 1 must still be copied"
        );
        assert!(
            worktree.join("three.txt").exists(),
            "item 3 must still be copied despite item 2 failing"
        );
    }

    #[test]
    fn worktreeinclude_preserves_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        let worktree = dir.path().join("wt");
        fs::create_dir_all(&repo_root).unwrap();
        fs::create_dir_all(&worktree).unwrap();

        fs::write(repo_root.join("real-target.txt"), "actual contents").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("real-target.txt", repo_root.join("link.txt")).unwrap();

            copy_preserving_symlinks(&repo_root.join("link.txt"), &worktree.join("link.txt"))
                .unwrap();

            let dst_meta = fs::symlink_metadata(worktree.join("link.txt")).unwrap();
            assert!(
                dst_meta.file_type().is_symlink(),
                "destination must be a symlink, not a copy of the target's contents"
            );
            let target = fs::read_link(worktree.join("link.txt")).unwrap();
            assert_eq!(target, Path::new("real-target.txt"));
        }
    }
}
