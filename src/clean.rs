use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::picker::{self, Pick};
use crate::worktree;

/// `cw clean` (§5e): scan every worktree under `root`, let the user
/// multi-select which to remove via `picker::pick_worktrees_multi` (idle/
/// dirty already annotated per §5l), then prune each selection. A dirty
/// entry is skipped unless `force` is set — `cw clean`'s default selection
/// never removes uncommitted work on age/idle status alone.
///
/// Continues past a per-entry failure (a repo that won't open, a worktree
/// that won't prune) instead of `?`-aborting the rest of a multi-selected
/// batch — same continue-on-error discipline as `scan_worktrees` (F34) and
/// `.worktreeinclude`'s copy loop (§5f fix 2): one bad entry shouldn't leave
/// every other selected worktree un-removed.
pub fn run_clean(config: &Config, root: &Path, force: bool) -> Result<()> {
    let entries = worktree::scan_worktrees(root)?;

    let candidates = match picker::pick_worktrees_multi(entries, config.idle_threshold_days)? {
        Pick::Selected(c) => c,
        Pick::Empty | Pick::Cancelled => return Ok(()),
    };

    for candidate in candidates {
        let entry = &candidate.entry;
        if candidate.dirty && !force {
            println!(
                "skipping {}/{} — has uncommitted changes (use --force to remove anyway)",
                entry.repo, entry.slug
            );
            continue;
        }

        if let Err(err) = remove_one(root, entry) {
            println!("failed to remove {}/{}: {err:#}", entry.repo, entry.slug);
            continue;
        }
        println!("removed {}/{}", entry.repo, entry.slug);
    }

    Ok(())
}

fn remove_one(root: &Path, entry: &worktree::WorktreeEntry) -> Result<()> {
    let repo_path = root.join(&entry.repo);
    let repo = git2::Repository::open(&repo_path)
        .with_context(|| format!("opening {}", repo_path.display()))?;
    worktree::remove_worktree(&repo, &entry.slug)
        .with_context(|| format!("removing {}/{}", entry.repo, entry.slug))
}
