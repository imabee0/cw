use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)] // typo'd keys (e.g. `deafult_agent`) fail loudly at startup, not silently ignored
pub struct Config {
    #[serde(default = "default_root")]
    pub root: PathBuf, // "~/..." expanded at use-site, never at parse-time
    #[serde(default = "default_ttl")]
    pub cache_ttl_minutes: u64,
    #[serde(default = "default_agent_name")]
    pub default_agent: String,
    #[serde(default = "default_symlink_dirs")]
    pub symlink_dirs: Vec<String>,
    #[serde(default = "default_idle_days")]
    pub idle_threshold_days: u64,
    pub post_clone_hook: Option<PathBuf>,
    pub post_create_hook: Option<String>, // None = disabled; Some("auto") = lockfile-sniffing; Some(path) = that script
    #[serde(default = "default_agents")]
    pub agents: HashMap<String, AgentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_root() -> PathBuf {
    PathBuf::from("~/repos")
}

fn default_ttl() -> u64 {
    15
}

fn default_agent_name() -> String {
    "claude".to_string()
}

fn default_symlink_dirs() -> Vec<String> {
    Vec::new()
}

fn default_idle_days() -> u64 {
    14
}

fn default_agents() -> HashMap<String, AgentConfig> {
    let plain = |cmd: &str| AgentConfig {
        cmd: cmd.to_string(),
        args: vec![],
    };
    let mut agents = HashMap::new();
    // Every bundled agent CLI opens its interactive session in the current
    // directory with no arguments — `agent::launch` sets `current_dir` to
    // the worktree, so none of them need a path flag.
    for name in ["claude", "codex", "grok", "opencode"] {
        agents.insert(name.to_string(), plain(name));
    }
    // `$SHELL` is expanded by `resolve_agent()` itself (F13): `Command::new("$SHELL")`
    // would NOT go through a shell and fails with ENOENT — see `expand_var` below.
    agents.insert("shell".to_string(), plain("$SHELL"));
    agents
}

impl Default for Config {
    fn default() -> Self {
        Config {
            root: default_root(),
            cache_ttl_minutes: default_ttl(),
            default_agent: default_agent_name(),
            symlink_dirs: default_symlink_dirs(),
            idle_threshold_days: default_idle_days(),
            post_clone_hook: None,
            post_create_hook: None,
            agents: default_agents(),
        }
    }
}

/// `$HOME`-based dotfile convention (matches `gh`/`cargo`/`rustup`), not
/// `directories::ProjectDirs` — see plan §0: `ProjectDirs` would put macOS
/// config at `~/Library/Application Support/cw`, diverging from Linux.
pub fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable not set")
}

pub fn config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/cw/config.toml"))
}

pub fn cache_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cache/cw/repos.json"))
}

/// Directory the rolling log file lives in. `RollingFileAppender` with
/// `Rotation::DAILY` and `filename_prefix("cw.log")` (main.rs's
/// `init_logging`) names each day's actual file `cw.log.<yyyy-MM-dd>` inside
/// this directory — never a literal `cw.log` — so callers that need the
/// directory (to create it, to point the appender at it) use this, not a
/// single fixed log file path.
pub fn log_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cache/cw"))
}

/// Per-repo hook confirm-once consent store (§5h) — `"owner/repo" -> confirmed`.
pub fn hook_consent_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cache/cw/hook-consent.json"))
}

/// Missing config file -> defaults, not an error (fresh-machine case).
pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config at {}", path.display()))
}

// No `save()`: nothing in the CLI surface (§4) ever writes config.toml back —
// it's hand-edited by the user, only ever loaded by cw. Adding a write path
// with no caller would sit as permanent dead code once `scripts/check.sh`'s
// `cargo clippy --all-targets -- -D warnings` starts running in phase 4.

/// Resolves `--agent`/`default_agent` against `config.agents`. Never panics
/// on a `HashMap` miss: an unknown name errors listing the configured agents.
/// Expands a leading `$VAR`/`${VAR}` in the resolved `cmd`/`args` (F13), so
/// returns an owned `AgentConfig` rather than a borrow.
pub fn resolve_agent(name: Option<&str>, cfg: &Config) -> Result<AgentConfig> {
    let name = name.unwrap_or(cfg.default_agent.as_str());
    let agent = cfg.agents.get(name).ok_or_else(|| {
        let mut known: Vec<&str> = cfg.agents.keys().map(String::as_str).collect();
        known.sort_unstable();
        anyhow::anyhow!(
            "unknown agent '{name}' — configured agents: {}",
            known.join(", ")
        )
    })?;
    Ok(AgentConfig {
        cmd: expand_var(&agent.cmd)?,
        args: agent
            .args
            .iter()
            .map(|a| expand_var(a))
            .collect::<Result<Vec<_>>>()?,
    })
}

/// Expands a leading `$VAR` or `${VAR}` by reading the env var directly
/// (never routes through `sh -c`). `$SHELL` specifically falls back to
/// `/bin/sh` when unset (§3). Anything else that fails to resolve — a
/// malformed `${VAR}suffix` form (no closing brace, or trailing text after
/// one), or any other variable that's simply unset — is an error naming the
/// exact string that didn't resolve, rather than silently substituting an
/// empty string: an empty `cmd`/arg surfaces later as an opaque "agent not
/// on PATH" failure with no indication which config value or variable
/// caused it.
fn expand_var(s: &str) -> Result<String> {
    let var = if let Some(rest) = s.strip_prefix("${") {
        rest.strip_suffix('}').with_context(|| {
            format!("malformed variable reference '{s}' in config — expected '${{VAR}}'")
        })?
    } else if let Some(rest) = s.strip_prefix('$') {
        rest
    } else {
        return Ok(s.to_string());
    };
    if var == "SHELL" {
        return Ok(env::var(var).unwrap_or_else(|_| "/bin/sh".to_string()));
    }
    env::var(var).with_context(|| {
        format!("config references '{s}', but environment variable '{var}' is not set")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_agent_missing_name() {
        let cfg = Config::default();
        let err = resolve_agent(Some("gpt"), &cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gpt"));
        assert!(msg.contains("claude"));
    }

    #[test]
    fn default_agents_include_bundled_clis() {
        let cfg = Config::default();
        for name in ["claude", "codex", "grok", "opencode", "shell"] {
            assert!(
                cfg.agents.contains_key(name),
                "missing default agent {name}"
            );
            assert!(cfg.agents[name].args.is_empty());
        }
        assert_eq!(cfg.agents["codex"].cmd, "codex");
        assert_eq!(cfg.agents["opencode"].cmd, "opencode");
        assert_eq!(cfg.agents["shell"].cmd, "$SHELL");
    }

    // `cargo test` runs tests concurrently by default, but SHELL is process-global —
    // two tests mutating it without synchronization race (this is what made
    // resolve_agent_expands_dollar_var flaky on CI's higher parallelism). Both SHELL
    // mutators lock this for their duration.
    static SHELL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_agent_expands_dollar_var() {
        let _guard = SHELL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: test-only env mutation, serialized against other SHELL mutators via SHELL_ENV_LOCK.
        unsafe { env::set_var("SHELL", "/bin/zsh") };
        let cfg = Config::default();
        let agent = resolve_agent(Some("shell"), &cfg).unwrap();
        assert_eq!(agent.cmd, "/bin/zsh");
        unsafe { env::remove_var("SHELL") };
    }

    #[test]
    fn expand_var_shell_falls_back_when_unset() {
        let _guard = SHELL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: test-only env mutation, serialized against other SHELL mutators via SHELL_ENV_LOCK.
        unsafe { env::remove_var("SHELL") };
        assert_eq!(expand_var("$SHELL").unwrap(), "/bin/sh");
    }

    #[test]
    fn expand_var_errors_on_unset_non_shell_var() {
        let err = expand_var("$CW_TEST_DEFINITELY_UNSET_VAR").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("CW_TEST_DEFINITELY_UNSET_VAR"));
    }

    #[test]
    fn expand_var_errors_on_malformed_braced_form() {
        let err = expand_var("${FOO}suffix").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("${FOO}suffix"));
    }

    #[test]
    fn config_rejects_unknown_key() {
        let toml_text = "deafult_agent = \"claude\"\n";
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        assert!(err.to_string().contains("deafult_agent"));
    }
}
