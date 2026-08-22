use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::{self, Config};

/// Runs every configured sanity check and returns one `(label, outcome)`
/// pair per check, in a stable, deterministic order (§5m): the fixed checks
/// first, then one row per configured `[agents.*]`, sorted by name —
/// `HashMap` iteration order would otherwise make `cw doctor`'s output
/// nondeterministic run to run.
pub fn run_doctor(config: &Config) -> Vec<(String, Result<()>)> {
    let mut checks: Vec<(String, Result<()>)> = vec![
        ("gh auth".to_string(), check_gh_auth()),
        (
            "git credential helper".to_string(),
            check_credential_helper(),
        ),
        ("terminal".to_string(), check_terminal()),
    ];

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
            check_binary_on_path(&resolved_cmd),
        ));
    }

    checks
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
