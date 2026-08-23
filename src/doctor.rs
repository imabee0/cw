use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::config::{self, Config};
use crate::selfupdate;

/// Runs every configured sanity check and returns one `(label, outcome)`
/// pair per check, in a stable, deterministic order (§5m): the fixed checks
/// first, then one row per configured `[agents.*]`, sorted by name —
/// `HashMap` iteration order would otherwise make `cw doctor`'s output
/// nondeterministic run to run. `Ok` carries an informational detail string
/// (empty for the plain pass/fail checks that had no more to say) printed
/// alongside "ok" — `check_install_source`/`check_update_status` are never
/// `Err`: neither a from-source build nor a stale binary blocks `cw` from
/// working, so neither should fail `cw doctor`'s exit code the way a real
/// blocker (unauthenticated `gh`, a missing agent binary) does.
pub fn run_doctor(config: &Config) -> Vec<(String, Result<String>)> {
    let mut checks: Vec<(String, Result<String>)> = vec![
        (
            "gh auth".to_string(),
            check_gh_auth().map(|()| String::new()),
        ),
        (
            "git credential helper".to_string(),
            check_credential_helper().map(|()| String::new()),
        ),
        (
            "terminal".to_string(),
            check_terminal().map(|()| String::new()),
        ),
        ("install source".to_string(), check_install_source()),
    ];
    // Read once — `check_update_status`'s neutral line and the "update
    // available" row below both describe the same cached check result, so
    // there's no reason to hit the cache file and re-parse it twice.
    let pending_version = config::log_dir()
        .ok()
        .and_then(|dir| selfupdate::cached_pending_version(&dir));
    checks.push((
        "update status".to_string(),
        Ok(update_status_message(pending_version.as_deref())),
    ));
    if let Some(latest) = &pending_version {
        checks.push((
            "update available".to_string(),
            Ok(stale_binary_message(latest)),
        ));
    }

    let mut names: Vec<&String> = config.agents.keys().collect();
    names.sort();
    for name in names {
        // Resolve through config::resolve_agent so a `$VAR`-expanding entry
        // (e.g. `[agents.shell]`'s `cmd = "$SHELL"`) is checked against the
        // real binary it launches, not the literal unexpanded string.
        let resolved_cmd = config::resolve_agent(Some(name), config)
            .map(|a| a.cmd)
            .unwrap_or_else(|_| config.agents[name].cmd.clone());
        checks.push((
            format!("agent: {name}"),
            check_binary_on_path(&resolved_cmd).map(|()| String::new()),
        ));
    }

    checks
}

/// Raw fields read directly off the install receipt JSON — not through
/// `axoupdater::AxoUpdater::load_receipt()`, which only reports whether a
/// receipt loaded, never its contents (`InstallReceipt`/`ReceiptProvider`
/// live in axoupdater's private `receipt` module, confirmed against its
/// 0.10.2 source — not part of its public API).
#[derive(Deserialize)]
struct ReceiptSummary {
    version: String,
    provider: ReceiptProvider,
}

#[derive(Deserialize)]
struct ReceiptProvider {
    source: String,
    version: String,
}

fn receipt_path() -> Result<PathBuf> {
    Ok(config::home_dir()?.join(".config/cw/cw-receipt.json"))
}

/// Which install method put this binary on disk — the fact that would have
/// caught this plan's own motivating incident (a `cw` three feature-commits
/// behind main with no visible symptom) at its actual root cause: whether
/// there's an install receipt at all to self-update from.
fn check_install_source() -> Result<String> {
    let path = receipt_path()?;
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(
            "from-source build (cargo install --path .) — no install receipt, self-update \
             unavailable"
                .to_string(),
        );
    };
    match serde_json::from_slice::<ReceiptSummary>(&bytes) {
        Ok(r) => Ok(format!(
            "installed via {} {} (receipt records version {})",
            r.provider.source, r.provider.version, r.version
        )),
        Err(_) => Ok(format!(
            "install receipt at {} is present but unreadable",
            path.display()
        )),
    }
}

/// Current vs. latest-known version — reuses `selfupdate.rs`'s cache
/// (whatever the background check most recently found) rather than forcing
/// a fresh network call here: `cw doctor` stays instant, and the dashboard/
/// fast-path background checks already keep that cache warm (see
/// `selfupdate.rs`'s and `main.rs`'s doc comments). Pure formatting over a
/// value `run_doctor` reads from the cache file once, shared with
/// `stale_binary_message` below, rather than each check re-reading it.
fn update_status_message(pending_version: Option<&str>) -> String {
    let current = env!("CARGO_PKG_VERSION");
    match pending_version {
        Some(latest) => format!("current {current}, {latest} available"),
        None => format!(
            "current {current} — up to date as of the last background check, or none has \
             completed yet"
        ),
    }
}

/// A separate, explicit row — present only when there's actually something
/// to warn about — so a stale binary doesn't just blend into the neutral
/// "update status" line above.
fn stale_binary_message(latest: &str) -> String {
    format!(
        "cw {latest} is available (current: {}) — run cw and press 'u', or re-run the \
         installer script",
        env!("CARGO_PKG_VERSION")
    )
}

fn check_gh_auth() -> Result<()> {
    let output = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .context("launching `gh auth status` — is `gh` on PATH?")?;
    if !output.status.success() {
        bail!("not authenticated — run `gh auth login`");
    }
    Ok(())
}

/// Confirms the git credential helper can resolve credentials for
/// `https://github.com` — asserts success only. The returned `Cred` is a
/// live credential (possibly an access token); it is immediately dropped,
/// NEVER logged, printed, or otherwise rendered (F37).
fn check_credential_helper() -> Result<()> {
    let cfg = git2::Config::open_default().context("opening git config")?;
    // `_cred` deliberately unused beyond confirming `Ok` — do not add a
    // Debug/Display of this value anywhere, in this function or a caller.
    let _cred = git2::Cred::credential_helper(&cfg, "https://github.com", None).context(
        "git credential helper for https://github.com — run `gh auth login`, or check credential.helper in ~/.gitconfig",
    )?;
    Ok(())
}

fn check_terminal() -> Result<()> {
    if crate::tui::is_interactive() {
        Ok(())
    } else {
        bail!("no interactive terminal detected (not a TTY, or /dev/tty is unavailable)")
    }
}

/// Splits `$PATH` and checks each directory for an executable file named
/// `cmd` — no new crate needed, this is ~10 lines. `cmd` containing a `/`
/// (an absolute/relative path, e.g. from `$SHELL` expansion) is checked
/// directly instead of searched.
pub fn check_binary_on_path(cmd: &str) -> Result<()> {
    let path_var = env::var_os("PATH").unwrap_or_default();
    resolve_on_path(cmd, &path_var)
}

/// The testable core of `check_binary_on_path`, taking `$PATH` as a
/// parameter rather than reading the real environment — lets the unit test
/// inject a fixture directory without mutating global process state.
fn resolve_on_path(cmd: &str, path_var: &OsStr) -> Result<()> {
    if cmd.is_empty() {
        bail!("empty command");
    }
    if cmd.contains('/') {
        return if is_executable_file(Path::new(cmd)) {
            Ok(())
        } else {
            bail!("'{cmd}' not found or not executable")
        };
    }
    for dir in env::split_paths(path_var) {
        if is_executable_file(&dir.join(cmd)) {
            return Ok(());
        }
    }
    bail!("'{cmd}' not found on PATH")
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn check_binary_on_path_found_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("foo");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_var = OsString::from(dir.path());

        assert!(resolve_on_path("foo", &path_var).is_ok());
        assert!(resolve_on_path("does-not-exist-anywhere", &path_var).is_err());
    }

    #[test]
    fn resolve_on_path_checks_absolute_path_directly() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bar");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let empty_path = OsString::new();

        assert!(resolve_on_path(&bin.to_string_lossy(), &empty_path).is_ok());
    }
}
