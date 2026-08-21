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
/// CRLF/BOM-tolerant (§5f fix 3): a leading UTF-8 BOM is stripped from the
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
/// new worktree. Collected rather than `?`-propagated (§5f fix 2), so a
/// single unreadable/permission-denied file doesn't abort worktree creation.
#[derive(Debug)]
pub struct CopyFailure {
    pub path: PathBuf,
    pub error: io::Error,
}

/// Copies `src` to `dst`, preserving a symlink rather than following it
/// (§5f fix 1): if `src` is itself a symlink, `dst` becomes a symlink to the
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
            // No non-unix target is in scope for cw (§0: Mac + Linux only) —
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
/// aborting the whole batch (§5f fix 2) — failures are collected and handed
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

/// Reads `.worktreeinclude` at `repo_root` (a missing file is a silent
/// no-op — most repos won't have one) and copies every file it matches into
/// `worktree_path`. A `.worktreeinclude` present but matching zero files
/// (typo'd pattern) logs a warning instead of silently no-op'ing. Per-file
/// copy failures are collected and returned rather than aborting worktree
/// creation.
pub fn apply_worktreeinclude(repo_root: &Path, worktree_path: &Path) -> Result<Vec<CopyFailure>> {
    let pattern_path = repo_root.join(".worktreeinclude");
    let content = match fs::read_to_string(&pattern_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", pattern_path.display())),
    };

    let gi = parse_patterns(repo_root, &content)?;
    let matches = walk_matches(repo_root, &gi);

    if matches.is_empty() {
        warn!(
            "{} matched zero files — check the patterns for typos",
            pattern_path.display()
        );
        return Ok(Vec::new());
    }

    Ok(copy_matches(repo_root, worktree_path, &matches))
}

#[cfg(test)]
mod tests {
    use super::*;

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
