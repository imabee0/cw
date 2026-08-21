use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::github::Repo;

/// On-disk snapshot of the last successful repo discovery. `fetched_at`
/// drives `cache_ttl_minutes` staleness checks in `refresh_if_needed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoCache {
    pub repos: Vec<Repo>,
    pub fetched_at: DateTime<Utc>,
}

impl RepoCache {
    /// Repos ordered most-recently-updated first. Plain string comparison on
    /// the ISO8601 `updated_at` field — a zero-padded ISO8601 UTC timestamp
    /// sorts lexicographically identically to chronological order, so no
    /// chrono parsing is needed here.
    pub fn sorted_repos(&self) -> Vec<Repo> {
        let mut repos = self.repos.clone();
        repos.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        repos
    }

    /// Atomic write: tmp file + rename, so a concurrent `cw` invocation or a
    /// crash mid-write can never leave a torn/corrupt cache on disk.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating cache directory {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Loads the repo cache from disk. A missing file, or one that fails to
/// parse (truncated write, corruption), is treated as "no cache" — `Ok(None)`
/// — never a panic or a hard error, since the caller's fallback is simply a
/// fresh fetch.
pub fn load(path: &Path) -> Result<Option<RepoCache>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(serde_json::from_slice::<RepoCache>(&bytes).ok())
}

/// Outcome of a `refresh_if_needed` call, so callers (`main.rs`'s default
/// flow) can decide whether to print a staleness warning.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshOutcome {
    /// Existing cache was within `cache_ttl_minutes`; no fetch attempted.
    Cached,
    /// A live fetch succeeded and the cache was rewritten.
    Fresh,
    /// A live fetch failed but a non-empty existing cache covered the gap.
    Stale { warning: String },
}

/// Returns the current repo list plus how it was obtained. `force` bypasses
/// the TTL check outright (`--refresh`). On a fetch failure, falls back to
/// the existing cache (if non-empty) with a warning rather than failing hard
/// — the whole point of caching offline-usable repo data. `fetch` is
/// injected so this is testable without spawning a real `gh` subprocess.
pub fn refresh_if_needed(
    path: &Path,
    ttl_minutes: u64,
    force: bool,
    fetch: impl FnOnce() -> Result<Vec<Repo>>,
) -> Result<(Vec<Repo>, RefreshOutcome)> {
    let existing = load(path)?;

    let is_stale = match &existing {
        None => true,
        Some(cache) => {
            let age_minutes = Utc::now()
                .signed_duration_since(cache.fetched_at)
                .num_minutes();
            // Negative age (clock skew / a fetched_at in the future) is
            // treated as stale too — force a refetch rather than trusting
            // suspicious cache metadata.
            age_minutes < 0 || age_minutes as u64 >= ttl_minutes
        }
    };

    if !force && !is_stale {
        let cache = existing.expect("is_stale is false only when existing is Some");
        return Ok((cache.repos, RefreshOutcome::Cached));
    }

    match fetch() {
        Ok(repos) => {
            let cache = RepoCache {
                repos: repos.clone(),
                fetched_at: Utc::now(),
            };
            cache.save(path)?;
            Ok((repos, RefreshOutcome::Fresh))
        }
        Err(e) => match existing {
            Some(cache) if !cache.repos.is_empty() => {
                let warning = format!(
                    "gh fetch failed ({e}) — using cached repo list from {}",
                    cache.fetched_at
                );
                Ok((cache.repos, RefreshOutcome::Stale { warning }))
            }
            _ => Err(e.context("fetching repos and no usable cache to fall back to")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, name: &str, updated_at: &str) -> Repo {
        Repo {
            owner: owner.into(),
            name: name.into(),
            updated_at: updated_at.into(),
        }
    }

    #[test]
    fn cache_load_tolerates_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.json");

        // Missing file -> no cache, not an error.
        assert!(load(&path).unwrap().is_none());

        // Truncated/invalid JSON -> no cache, not a panic.
        fs::write(&path, b"{\"repos\": [ this is not valid json").unwrap();
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn cache_save_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.json");
        let cache = RepoCache {
            repos: vec![repo("me", "a", "2026-01-01T00:00:00Z")],
            fetched_at: Utc::now(),
        };
        cache.save(&path).unwrap();

        // No leftover tmp file next to the target...
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "tmp file left behind after save");

        // ...and the target itself parses as valid JSON / a valid cache.
        let loaded = load(&path).unwrap().expect("saved cache should load back");
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos[0].name, "a");
    }

    #[test]
    fn refresh_falls_back_to_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.json");
        let stale = RepoCache {
            repos: vec![repo("me", "a", "2026-01-01T00:00:00Z")],
            fetched_at: Utc::now() - chrono::Duration::minutes(100),
        };
        stale.save(&path).unwrap();

        let (repos, outcome) = refresh_if_needed(&path, 15, false, || {
            Err(anyhow::anyhow!("network unreachable"))
        })
        .unwrap();

        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "a");
        match outcome {
            RefreshOutcome::Stale { warning } => assert!(warning.contains("network unreachable")),
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn refresh_uses_cache_within_ttl_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.json");
        let fresh = RepoCache {
            repos: vec![repo("me", "a", "2026-01-01T00:00:00Z")],
            fetched_at: Utc::now(),
        };
        fresh.save(&path).unwrap();

        let (repos, outcome) = refresh_if_needed(&path, 15, false, || {
            panic!("fetch should not be called when cache is within TTL")
        })
        .unwrap();

        assert_eq!(repos.len(), 1);
        assert_eq!(outcome, RefreshOutcome::Cached);
    }

    #[test]
    fn refresh_propagates_error_with_no_usable_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.json");

        let result = refresh_if_needed(&path, 15, false, || {
            Err(anyhow::anyhow!("network unreachable"))
        });

        assert!(result.is_err());
    }

    #[test]
    fn repo_cache_sort_order() {
        let cache = RepoCache {
            repos: vec![
                repo("me", "old", "2025-01-01T00:00:00Z"),
                repo("me", "newest", "2026-06-01T00:00:00Z"),
                repo("me", "mid", "2026-01-01T00:00:00Z"),
            ],
            fetched_at: Utc::now(),
        };

        let sorted = cache.sorted_repos();
        let names: Vec<&str> = sorted.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["newest", "mid", "old"]);
    }
}
