//! Background self-update check + apply, backing the dashboard's `u` key
//! and `cw doctor`'s stale-binary diagnostic.
//!
//! Releases are GitHub Releases tagged `cw-vX.Y.Z`, cut by
//! `.github/workflows/ci.yml`, each carrying `cw-<target-triple>.tar.gz`
//! (the `cw` binary at the archive root) and `cw-<target-triple>.tar.gz.sha256`
//! (`<hex>  <archive name>`). `install/cw.sh` installs from the same assets.
//!
//! Only a release build — one built by that pipeline, which passes
//! `CI_VERSION_CW` to `build.rs` — checks for updates: a from-source build has
//! no released artifact it corresponds to. Every failure (from-source build,
//! offline, GitHub rate-limited, `gh` missing, a corrupt cache file) is
//! swallowed to a `tracing::debug!` line; the check thread then returns
//! without sending, so the receiver never yields — exactly as if the check
//! hadn't run. Never an error, never touches the process exit code.
//!
//! GitHub access goes through `gh`, like every other GitHub call in cw.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The version this binary reports: the released version for a release
/// build, the Cargo.toml version otherwise (see `build.rs`).
pub const CURRENT_VERSION: &str = env!("CW_VERSION");
/// The target triple this binary was built for — selects the release asset.
const TARGET: &str = env!("CW_TARGET");
const TAG_PREFIX: &str = "cw-v";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Whether this binary was built by the release pipeline.
pub fn is_release_build() -> bool {
    !env!("CW_RELEASE_BUILD").is_empty()
}

#[derive(Serialize, Deserialize)]
struct UpdateCache {
    /// Latest released version seen by the last check.
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
/// `None` when unchecked, up to date, or the cache is corrupt. Used by
/// `cw doctor` (reusing whatever the last background check found, never
/// forcing a fresh network call of its own).
pub fn cached_pending_version(cache_dir: &Path) -> Option<String> {
    read_cache(&cache_file(cache_dir)).and_then(pending_from)
}

/// `u`'s suspend target (`dashboard.rs::run_suspend_chain`): downloads the
/// latest release's archive for this target, verifies its SHA256, and
/// replaces the running binary on disk. Returns whether an update was
/// actually applied — the latest release is re-read here, so a cached
/// "pending" version that's no longer newer returns `Ok(false)`, not an
/// error.
pub fn apply_update() -> Result<bool> {
    ensure!(
        is_release_build(),
        "this cw was built from source; self-update only applies to release builds"
    );
    let Some(latest) = latest_release()? else {
        return Ok(false);
    };
    if !is_newer(&latest, CURRENT_VERSION) {
        return Ok(false);
    }
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locating the running cw binary")?;
    let work = scratch_dir()?;
    let result = download_and_install(&latest, &work, &exe);
    let _ = std::fs::remove_dir_all(&work);
    result.map(|()| true)
}

fn run_check(cache_dir: &Path) -> Result<Option<String>> {
    ensure!(is_release_build(), "from-source build");
    let path = cache_file(cache_dir);
    if let Some(cached) = read_cache(&path) {
        // `checked_sub`, not `saturating_sub`: a `checked_at` in the future
        // (clock skew) must not read as freshly checked and silently
        // suppress the check until real time catches up.
        if let Some(age) = now_secs().checked_sub(cached.checked_at) {
            if age < CHECK_INTERVAL_SECS {
                return Ok(pending_from(cached)); // rate-limited — still fresh
            }
        }
    }

    let version = latest_release()?.unwrap_or_else(|| CURRENT_VERSION.to_string());
    let cache = UpdateCache {
        version,
        checked_at: now_secs(),
    };
    if let Err(e) = write_cache(&path, &cache) {
        tracing::debug!("self-update: failed to persist check result: {e:#}");
    }
    Ok(pending_from(cache))
}

fn repo_slug() -> Result<&'static str> {
    env!("CARGO_PKG_REPOSITORY")
        .strip_prefix("https://github.com/")
        .context("CARGO_PKG_REPOSITORY is not a github.com URL")
}

#[derive(Deserialize)]
struct ReleaseSummary {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// The newest published `cw-vX.Y.Z` release's version, if any.
fn latest_release() -> Result<Option<String>> {
    let output = Command::new("gh")
        .args([
            "api",
            &format!("repos/{}/releases?per_page=100", repo_slug()?),
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .context("launching `gh api` — is `gh` on PATH?")?;
    ensure!(
        output.status.success(),
        "listing releases failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let releases: Vec<ReleaseSummary> =
        serde_json::from_slice(&output.stdout).context("parsing the release list")?;
    Ok(newest_version(&releases))
}

fn newest_version(releases: &[ReleaseSummary]) -> Option<String> {
    releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| r.tag_name.strip_prefix(TAG_PREFIX))
        .filter_map(|v| parse_version(v).map(|parsed| (parsed, v)))
        .max_by_key(|(parsed, _)| *parsed)
        .map(|(_, v)| v.to_string())
}

/// `X.Y.Z` only — a pre-release or build suffix isn't something the release
/// pipeline produces, so such a tag is ignored rather than guessed at.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

fn pending_from(cache: UpdateCache) -> Option<String> {
    is_newer(&cache.version, CURRENT_VERSION).then_some(cache.version)
}

fn archive_name() -> String {
    format!("cw-{TARGET}.tar.gz")
}

fn download_and_install(version: &str, work: &Path, exe: &Path) -> Result<()> {
    let archive = archive_name();
    let checksum = format!("{archive}.sha256");
    let status = Command::new("gh")
        .args(["release", "download", &format!("{TAG_PREFIX}{version}")])
        .args(["-R", repo_slug()?, "-p", &archive, "-p", &checksum, "-D"])
        .arg(work)
        .env("GH_PROMPT_DISABLED", "1")
        .status()
        .context("launching `gh release download`")?;
    ensure!(status.success(), "downloading {archive} failed");

    let expected = std::fs::read_to_string(work.join(&checksum))
        .with_context(|| format!("reading {checksum}"))?;
    verify_sha256(&work.join(&archive), &expected)?;

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(work.join(&archive))
        .arg("-C")
        .arg(work)
        .status()
        .context("launching tar")?;
    ensure!(status.success(), "extracting {archive} failed");
    replace_binary(&work.join("cw"), exe)
}

/// `expected` is the checksum file's contents: `<hex>  <file name>`.
fn verify_sha256(file: &Path, expected: &str) -> Result<()> {
    let expected = expected
        .split_whitespace()
        .next()
        .context("checksum file is empty")?
        .to_ascii_lowercase();
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        bail!(
            "SHA256 mismatch for {}: expected {expected}, got {actual}",
            file.display()
        );
    }
    Ok(())
}

/// Copies `new` next to `exe`, makes it executable, then renames it over
/// `exe` — atomic on the same filesystem, and safe while `exe` is running
/// (the running process keeps the old inode).
fn replace_binary(new: &Path, exe: &Path) -> Result<()> {
    let dir = exe
        .parent()
        .context("binary path has no parent directory")?;
    let staged = dir.join(format!(".cw.update-{}", std::process::id()));
    std::fs::copy(new, &staged).with_context(|| format!("writing {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    if let Err(e) = std::fs::rename(&staged, exe) {
        let _ = std::fs::remove_file(&staged);
        return Err(e).with_context(|| format!("replacing {}", exe.display()));
    }
    Ok(())
}

fn scratch_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("cw-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn cache_file(cache_dir: &Path) -> PathBuf {
    cache_dir.join("update-check.json")
}

/// A cache whose version isn't plain `X.Y.Z` — e.g. `"cw-v0.2.2"`, as the
/// axoupdater-based 0.2.0 wrote it — reads as no cache, so the next check
/// runs now instead of trusting an entry it can't compare for up to a day.
fn read_cache(path: &Path) -> Option<UpdateCache> {
    let bytes = std::fs::read(path).ok()?;
    let cache: UpdateCache = serde_json::from_slice(&bytes).ok()?;
    parse_version(&cache.version).is_some().then_some(cache)
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
    fn pending_from_reports_only_a_newer_version() {
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

        let older = UpdateCache {
            version: "0.0.1".to_string(),
            checked_at: 0,
        };
        assert_eq!(pending_from(older), None, "never offer a downgrade");
    }

    #[test]
    fn newest_version_picks_the_highest_published_cw_tag() {
        let releases: Vec<ReleaseSummary> = serde_json::from_str(
            r#"[
                {"tag_name": "cw-v0.10.0", "draft": true, "prerelease": false},
                {"tag_name": "cw-v0.9.1", "draft": false, "prerelease": true},
                {"tag_name": "v9.9.9", "draft": false, "prerelease": false},
                {"tag_name": "cw-v0.3.0", "draft": false, "prerelease": false},
                {"tag_name": "cw-v0.3.0-rc.1", "draft": false, "prerelease": false},
                {"tag_name": "cw-v0.2.10", "draft": false, "prerelease": false}
            ]"#,
        )
        .unwrap();
        assert_eq!(newest_version(&releases), Some("0.3.0".to_string()));
        assert_eq!(newest_version(&[]), None);
    }

    #[test]
    fn version_comparison_is_numeric_not_lexical() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
    }

    #[test]
    fn verify_sha256_accepts_a_match_and_rejects_a_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.tar.gz");
        std::fs::write(&file, b"hello").unwrap();
        let good = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824  a.tar.gz\n";
        verify_sha256(&file, good).unwrap();
        let bad = format!("{}  a.tar.gz", "0".repeat(64));
        assert!(verify_sha256(&file, &bad).is_err());
        assert!(verify_sha256(&file, "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replace_binary_swaps_the_file_atomically_and_makes_it_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("cw");
        std::fs::write(&exe, b"old").unwrap();
        let new = dir.path().join("cw.new");
        std::fs::write(&new, b"new").unwrap();
        replace_binary(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".cw.update-"))
            .collect();
        assert!(leftovers.is_empty(), "no staged file may linger");
    }

    #[test]
    fn cache_round_trips_through_atomic_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = cache_file(dir.path());
        assert!(read_cache(&path).is_none());
        let cache = UpdateCache {
            version: "1.2.3".to_string(),
            checked_at: 42,
        };
        write_cache(&path, &cache).expect("write_cache");
        let read_back = read_cache(&path).expect("cache file must be readable after a write");
        assert_eq!(read_back.version, "1.2.3");
        assert_eq!(read_back.checked_at, 42);
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
    fn read_cache_ignores_a_version_it_cannot_compare() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = cache_file(dir.path());
        // Written by the axoupdater-based 0.2.0: the tag, not a version.
        std::fs::write(&path, br#"{"version":"cw-v0.2.2","checked_at":1793845949}"#).unwrap();
        assert!(read_cache(&path).is_none());

        std::fs::write(&path, br#"{"version":"0.2.2","checked_at":1793845949}"#).unwrap();
        assert_eq!(
            read_cache(&path).map(|c| c.version),
            Some("0.2.2".to_string())
        );
    }

    #[test]
    fn cached_pending_version_is_none_for_a_missing_cache_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(cached_pending_version(&dir.path().join("nope")), None);
    }
}
