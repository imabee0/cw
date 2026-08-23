//! Background self-update check + apply, backing the dashboard's `u` key
//! and `cw doctor`'s stale-binary diagnostic. Mirrors `dashboard.rs`'s own
//! background-thread shape (`spawn_worktrees_thread`): one `thread::spawn`,
//! nothing shared but an `mpsc::channel`. Every failure here (no install
//! receipt — a plain `cargo install --path .` build has none — offline,
//! GitHub rate-limited, a corrupt cache file) is swallowed to a
//! `tracing::debug!` line; on failure the thread simply returns without
//! sending, so the receiver never yields — exactly as if the check hadn't
//! run at all. Never an error, never touches the process exit code.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context, Result};
use axoupdater::AxoUpdater;
use serde::{Deserialize, Serialize};

const APP_NAME: &str = "cw";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

#[derive(Serialize, Deserialize)]
struct UpdateCache {
    version: String,
    checked_at: u64,
}

/// Spawns the background check — `cache_dir` is `~/.cache/cw`
/// (`config::log_dir()`), the same directory `repos.json`/
/// `hook-consent.json`/the rolling log already live in.
pub fn spawn_check(cache_dir: PathBuf) -> mpsc::Receiver<Option<String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || match run_check(&cache_dir) {
        Ok(pending) => {
            let _ = tx.send(pending);
        }
        Err(e) => tracing::debug!("self-update check skipped: {e:#}"),
    });
    rx
}

/// Reads the cache file if present and returns the pending version string —
/// `None` when unchecked, up to date, or the cache is stale/corrupt. Used by
/// `cw doctor` (reusing whatever the last background check found, never
/// forcing a fresh network call of its own).
pub fn cached_pending_version(cache_dir: &Path) -> Option<String> {
    read_cache(&cache_file(cache_dir)).and_then(pending_from)
}

/// `u`'s suspend target (`dashboard.rs::run_suspend_chain`): downloads and
/// runs the platform installer for the latest release, replacing the
/// running binary on disk. Returns whether an update was actually applied —
/// `run_sync` re-checks freshness itself and returns `Ok(None)` (not an
/// error) when nothing turned out to be needed after all (e.g. the cached
/// "pending" version this call was triggered from was already stale — the
/// receipt was updated by some other means since), so the caller must not
/// assume success here means a new binary landed on disk.
pub fn apply_update() -> Result<bool> {
    let mut updater = AxoUpdater::new_for(APP_NAME);
    updater.load_receipt()?;
    Ok(updater.run_sync()?.is_some())
}

fn run_check(cache_dir: &Path) -> Result<Option<String>> {
    let path = cache_file(cache_dir);
    if let Some(cached) = read_cache(&path) {
        // `checked_sub`, not `saturating_sub`: a `checked_at` in the future
        // (clock skew — the cache was written while the system clock was
        // fast, then corrected backward) must not saturate to age-zero and
        // read as freshly-checked, or the check stays silently suppressed
        // until real time catches back up past it. Same "distrust a
        // suspicious timestamp, treat it as stale" call `cache.rs`'s
        // `refresh_if_needed` makes for a `fetched_at` in the future.
        if let Some(age) = now_secs().checked_sub(cached.checked_at) {
            if age < CHECK_INTERVAL_SECS {
                return Ok(pending_from(cached)); // rate-limited — still fresh
            }
        }
    }

    let mut updater = AxoUpdater::new_for(APP_NAME);
    updater.load_receipt()?; // Err here IS the mandatory-degrade case
    let needed = updater.is_update_needed_sync()?;
    let version = if needed {
        latest_release_tag()?
    } else {
        CURRENT_VERSION.to_string()
    };
    let cache = UpdateCache {
        version,
        checked_at: now_secs(),
    };
    if let Err(e) = write_cache(&path, &cache) {
        tracing::debug!("self-update: failed to persist check result: {e:#}");
    }
    Ok(pending_from(cache))
}

/// `axoupdater`'s blocking API has no synchronous accessor for the actual
/// target version (only `is_update_needed_sync` -> bool and `run_sync`,
/// which performs the real install) — see the Cargo.toml comment. So the
/// version shown to the user comes from `gh`, this codebase's own
/// established path for every other GitHub API call.
fn latest_release_tag() -> Result<String> {
    let repo = env!("CARGO_PKG_REPOSITORY")
        .strip_prefix("https://github.com/")
        .context("CARGO_PKG_REPOSITORY is not a github.com URL")?;
    let output = Command::new("gh")
        .args([
            "release", "view", "--json", "tagName", "-q", ".tagName", "-R", repo,
        ])
        .output()
        .context("launching `gh release view`")?;
    ensure!(
        output.status.success(),
        "gh release view failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tag = String::from_utf8(output.stdout)?;
    let tag = tag.trim();
    Ok(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

fn pending_from(cache: UpdateCache) -> Option<String> {
    (cache.version != CURRENT_VERSION).then_some(cache.version)
}

fn cache_file(cache_dir: &Path) -> PathBuf {
    cache_dir.join("update-check.json")
}

fn read_cache(path: &Path) -> Option<UpdateCache> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Atomic tmp+rename, matching `cache.rs`'s own convention — this file can
/// be written from a thread whose process exits at any moment (the fast,
/// non-interactive `cw` paths), so a bare `fs::write` risks a truncated
/// file on an abrupt exit mid-write.
fn write_cache(path: &Path, cache: &UpdateCache) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(cache)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_from_reports_only_a_real_version_mismatch() {
        let same = UpdateCache {
            version: CURRENT_VERSION.to_string(),
            checked_at: 0,
        };
        assert_eq!(pending_from(same), None);

        let newer = UpdateCache {
            version: "999.0.0".to_string(),
            checked_at: 0,
        };
        assert_eq!(pending_from(newer), Some("999.0.0".to_string()));
    }

    #[test]
    fn cache_round_trips_through_atomic_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = cache_file(dir.path());
        assert!(
            read_cache(&path).is_none(),
            "no file written yet must read as no cache"
        );

        let cache = UpdateCache {
            version: "1.2.3".to_string(),
            checked_at: 42,
        };
        write_cache(&path, &cache).expect("write_cache");
        let read_back = read_cache(&path).expect("cache file must be readable after a write");
        assert_eq!(read_back.version, "1.2.3");
        assert_eq!(read_back.checked_at, 42);
        // The tmp file used for the atomic rename must not linger.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn read_cache_tolerates_a_corrupt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = cache_file(dir.path());
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_cache(&path).is_none());
    }

    #[test]
    fn cached_pending_version_is_none_for_a_missing_cache_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(cached_pending_version(&dir.path().join("nope")), None);
    }
}
