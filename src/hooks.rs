use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::warn;

/// Fixed precedence for `post_create_hook = "auto"` (§3): the first
/// matching lockfile under `repo_root` wins, deterministically — never
/// "whichever `read_dir` happens to return first".
const LOCKFILE_PRECEDENCE: &[(&str, &str, &[&str])] = &[
    ("Cargo.lock", "cargo", &["build"]),
    ("package-lock.json", "npm", &["ci"]),
    ("pnpm-lock.yaml", "pnpm", &["install"]),
];

/// Resolves `post_create_hook = "auto"` against `repo_root`'s lockfiles, per
/// §3's fixed precedence. `None` if no known lockfile is present — auto mode
/// is then a no-op, not an error, since plenty of repos have no lockfile at
/// all.
pub fn detect_auto_hook(repo_root: &Path) -> Option<(&'static str, &'static [&'static str])> {
    LOCKFILE_PRECEDENCE
        .iter()
        .find(|(lockfile, _, _)| repo_root.join(lockfile).exists())
        .map(|(_, program, args)| (*program, *args))
}

/// One resolved hook invocation — the program, its argv, and the directory
/// it runs in — everything the confirm-once-per-repo prompt (§5h) needs to
/// print before the very first execution against a given repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHook {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

/// Resolves `post_clone_hook` (§3: `None` = unset, cw's shipped default;
/// `Some(path)` = that repo-relative script). Runs once, cwd = repo root,
/// only right after a FRESH `gh repo clone` — this path is repo-relative,
/// so the CLONED REPO supplies the script body cw is about to execute, not
/// the user (§0's "repo-supplied hook execution" decision).
pub fn resolve_post_clone_hook(repo_root: &Path, hook: Option<&Path>) -> Option<ResolvedHook> {
    let rel = hook?;
    Some(ResolvedHook {
        program: repo_root.join(rel).to_string_lossy().into_owned(),
        args: Vec::new(),
        cwd: repo_root.to_path_buf(),
    })
}

/// Resolves `post_create_hook` (`None` = disabled; `Some("auto")` =
/// `detect_auto_hook`'s lockfile-sniffing; `Some(path)` = that repo-relative
/// script). Runs once, cwd = the NEW worktree, only right after worktree
/// creation (skipped entirely on resume of an existing worktree — callers
/// only invoke this from the creation path, never from
/// `create_or_resume_worktree`'s fast-resume branch). The script path, when
/// explicit, is resolved against the ORIGINAL repo — same repo-relative
/// rule as `post_clone_hook` — even though it runs with the worktree as
/// cwd.
pub fn resolve_post_create_hook(
    repo_root: &Path,
    worktree_path: &Path,
    hook: Option<&str>,
) -> Option<ResolvedHook> {
    let hook = hook?;
    if hook == "auto" {
        let (program, args) = detect_auto_hook(repo_root)?;
        return Some(ResolvedHook {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: worktree_path.to_path_buf(),
        });
    }
    Some(ResolvedHook {
        program: repo_root.join(hook).to_string_lossy().into_owned(),
        args: Vec::new(),
        cwd: worktree_path.to_path_buf(),
    })
}

/// Per-repo hook consent (§5h), persisted at
/// `~/.cache/cw/hook-consent.json`: `"owner/repo" -> confirmed`, so the
/// confirmation prompt fires once per repo, not once per worktree.
pub type HookConsent = HashMap<String, bool>;

/// Missing, truncated, or invalid-JSON consent file -> "nothing confirmed
/// yet", never a panic or hard error — same tolerance idiom as
/// `cache::load`.
pub fn load_consent(path: &Path) -> HookConsent {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Atomic write (tmp file + rename), matching `cache.rs`'s `save()` — the
/// same concurrent-write/crash-mid-write hazard a plain `File::create`
/// would have applies to this file too.
pub fn save_consent(path: &Path, consent: &HookConsent) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(consent)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Checks whether `--yes` is set or `repo` is already confirmed; if
/// neither, calls `confirm` (the interactive y/N prompt) exactly once and
/// records the answer — confirmed or declined — so a later call for the
/// same repo never prompts again, regardless of the answer. Returns
/// whether the hook is cleared to run.
///
/// `auto_yes` is checked BEFORE the cache, not after: it's the documented
/// non-interactive escape hatch (§0/§3/§4's `--yes`), so it must be able to
/// override a previously *declined* consent for THIS run, not just skip
/// prompting on a first encounter. Checking the cache first would let one
/// earlier "n" permanently disable a repo's hooks with no way back short of
/// hand-editing `hook-consent.json`. Deliberately NOT recorded to `consent`,
/// though: `--yes` is a per-invocation override, not a re-answer of the
/// prompt — writing `true` here would silently flip a stored decline to a
/// standing "yes" that a later run WITHOUT `--yes` would then honor with no
/// prompt at all, defeating the confirm-once-per-repo gate on exactly the
/// runs that most need it (an interactive session against a repo someone
/// once declined).
fn gate(
    repo: &str,
    resolved: &ResolvedHook,
    consent: &mut HookConsent,
    auto_yes: bool,
    confirm: impl FnOnce(&ResolvedHook) -> bool,
) -> bool {
    if auto_yes {
        return true;
    }
    if let Some(&confirmed) = consent.get(repo) {
        return confirmed;
    }
    let confirmed = confirm(resolved);
    consent.insert(repo.to_string(), confirmed);
    confirmed
}

/// Env vars passed to every hook script invocation, so a script knows which
/// repo/worktree/slug/agent it's running for without re-deriving it from
/// argv or cwd.
pub struct HookEnv<'a> {
    pub repo: &'a str,
    pub worktree_path: &'a Path,
    pub slug: &'a str,
    pub agent: &'a str,
}

impl HookEnv<'_> {
    fn as_pairs(&self) -> [(&'static str, String); 4] {
        [
            ("CW_REPO", self.repo.to_string()),
            (
                "CW_WORKTREE_PATH",
                self.worktree_path.to_string_lossy().into_owned(),
            ),
            ("CW_SLUG", self.slug.to_string()),
            ("CW_AGENT", self.agent.to_string()),
        ]
    }
}

/// What happened when a hook was considered for execution — distinguishes
/// "nothing configured" and "user declined" from an actual run, since a
/// caller reports each differently.
#[derive(Debug)]
pub enum HookOutcome {
    NotConfigured,
    Declined,
    Ran { status: std::process::ExitStatus },
}

/// Direct exec (`Command::new`, no shell) — preserves whatever shebang the
/// script declares. No timeout, deliberately (§5h): `cargo build`/`npm ci`
/// can legitimately run long, and a timeout would actively break legitimate
/// use; Ctrl-C remains available. On `PermissionDenied` (script exists but
/// isn't executable), maps to an actionable `chmod +x` message instead of a
/// raw OS error. On a nonzero exit, warns and returns `Ok` rather than
/// aborting — a failed setup script shouldn't block getting into an
/// interactive agent session where the user can fix it themselves.
pub(crate) fn exec_hook(resolved: &ResolvedHook, env: &HookEnv) -> Result<HookOutcome> {
    let status = Command::new(&resolved.program)
        .args(&resolved.args)
        .current_dir(&resolved.cwd)
        .envs(env.as_pairs())
        .status();

    let status = match status {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            bail!(
                "hook script {} isn't executable — run chmod +x {}",
                resolved.program,
                resolved.program
            );
        }
        Err(e) => return Err(e).with_context(|| format!("running hook {}", resolved.program)),
    };

    if !status.success() {
        warn!(
            "hook {} exited with {status} — continuing anyway",
            resolved.program
        );
    }

    Ok(HookOutcome::Ran { status })
}

/// The repo identity and confirm-once-per-repo policy shared by both
/// `run_post_clone_hook` and `run_post_create_hook` — grouped into one
/// borrow so callers don't thread `repo`/`consent`/`auto_yes` through
/// separately (also keeps each function under clippy's argument-count
/// lint). Callers are responsible for persisting `consent` via
/// `save_consent` afterward — these functions only mutate the in-memory
/// map.
pub struct HookSession<'a> {
    pub repo: &'a str,
    pub consent: &'a mut HookConsent,
    pub auto_yes: bool,
}

/// Runs `post_clone_hook` if configured, gated by the confirm-once-per-repo
/// consent check (§5h).
pub fn run_post_clone_hook(
    session: &mut HookSession,
    repo_root: &Path,
    hook: Option<&Path>,
    env: &HookEnv,
    confirm: impl FnOnce(&ResolvedHook) -> bool,
) -> Result<HookOutcome> {
    let Some(resolved) = resolve_post_clone_hook(repo_root, hook) else {
        return Ok(HookOutcome::NotConfigured);
    };
    if !gate(
        session.repo,
        &resolved,
        session.consent,
        session.auto_yes,
        confirm,
    ) {
        return Ok(HookOutcome::Declined);
    }
    exec_hook(&resolved, env)
}

/// Runs `post_create_hook` if configured, gated by the same
/// confirm-once-per-repo consent check as `run_post_clone_hook`.
pub fn run_post_create_hook(
    session: &mut HookSession,
    repo_root: &Path,
    worktree_path: &Path,
    hook: Option<&str>,
    env: &HookEnv,
    confirm: impl FnOnce(&ResolvedHook) -> bool,
) -> Result<HookOutcome> {
    let Some(resolved) = resolve_post_create_hook(repo_root, worktree_path, hook) else {
        return Ok(HookOutcome::NotConfigured);
    };
    if !gate(
        session.repo,
        &resolved,
        session.consent,
        session.auto_yes,
        confirm,
    ) {
        return Ok(HookOutcome::Declined);
    }
    exec_hook(&resolved, env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_default_hook_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.lock"), "").unwrap();
        fs::write(root.join("package-lock.json"), "").unwrap();

        let (program, args) = detect_auto_hook(root).unwrap();
        assert_eq!(program, "cargo");
        assert_eq!(args, &["build"]);
    }

    #[test]
    fn detect_auto_hook_falls_through_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("package-lock.json"), "").unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "").unwrap();

        // Cargo.lock absent -> package-lock.json wins over pnpm-lock.yaml,
        // per the fixed §3 order, not whichever the filesystem lists first.
        let (program, args) = detect_auto_hook(root).unwrap();
        assert_eq!(program, "npm");
        assert_eq!(args, &["ci"]);
    }

    #[test]
    fn detect_auto_hook_none_when_no_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_auto_hook(dir.path()).is_none());
    }

    #[test]
    fn gate_prompts_once_per_repo() {
        let mut consent = HookConsent::new();
        let resolved = ResolvedHook {
            program: "true".into(),
            args: vec![],
            cwd: PathBuf::from("/tmp"),
        };
        let mut prompts = 0;

        assert!(gate("me/repo", &resolved, &mut consent, false, |_| {
            prompts += 1;
            true
        }));
        // Second call for the same repo must not invoke confirm again.
        assert!(gate("me/repo", &resolved, &mut consent, false, |_| {
            prompts += 1;
            true
        }));
        assert_eq!(prompts, 1);
    }

    #[test]
    fn gate_yes_overrides_a_persisted_decline_without_recording_it() {
        let mut consent = HookConsent::new();
        let resolved = ResolvedHook {
            program: "true".into(),
            args: vec![],
            cwd: PathBuf::from("/tmp"),
        };

        // First run: declined interactively, recorded as `false`.
        assert!(!gate("me/repo", &resolved, &mut consent, false, |_| false));
        assert_eq!(consent.get("me/repo"), Some(&false));

        // A later `--yes` run must be able to override that persisted
        // decline for ITS OWN run, not be permanently blocked by it.
        assert!(gate("me/repo", &resolved, &mut consent, true, |_| {
            panic!("auto_yes must not consult the interactive confirm callback")
        }));

        // But it must NOT durably rewrite the stored decline to a standing
        // "yes" — a later interactive run (no --yes) should still see the
        // original decline and re-gate on it, not silently run the hook.
        assert_eq!(consent.get("me/repo"), Some(&false));
    }

    #[test]
    fn exec_hook_maps_permission_denied_to_actionable_message() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("setup.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        // Deliberately NOT chmod +x'd.

        let resolved = ResolvedHook {
            program: script.to_string_lossy().into_owned(),
            args: vec![],
            cwd: dir.path().to_path_buf(),
        };
        let env = HookEnv {
            repo: "me/repo",
            worktree_path: dir.path(),
            slug: "slug",
            agent: "claude",
        };

        let err = exec_hook(&resolved, &env).unwrap_err();
        assert!(err.to_string().contains("chmod +x"));
    }
}
