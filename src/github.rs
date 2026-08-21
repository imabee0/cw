use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// A repo discovered via `gh` — either the authenticated user's own or one
/// belonging to an org they're a member of. `owner`/`name` are exactly the
/// fields `sync.rs`'s `resolve_local_path` joins onto `root` (plan §5a:
/// `root.join(&repo.owner).join(&repo.name)`), so their names are
/// load-bearing, not just naming convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    pub owner: String,
    pub name: String,
    /// Raw ISO8601 `updatedAt` from `gh`'s JSON output. Kept as a string
    /// rather than parsed into `chrono::DateTime` deliberately: a
    /// zero-padded ISO8601 UTC timestamp sorts lexicographically identically
    /// to chronological order, so `cache.rs`'s recency sort needs no chrono
    /// parsing at all.
    pub updated_at: String,
}

impl Repo {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// Result of merging personal + per-org repo discovery. Per-org failures are
/// isolated into `warnings` rather than aborting the whole discovery.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoverResult {
    pub repos: Vec<Repo>,
    pub warnings: Vec<String>,
}

/// Builds the `gh repo list` argv. `owner: None` lists the authenticated
/// user's own repos; `Some(org)` lists that org's. Always passes `--limit
/// 1000` — `gh`'s own default is 30, which would silently truncate discovery
/// for any account/org past that count (the single most user-visible bug
/// the design audit found).
fn repo_list_args(owner: Option<&str>) -> Vec<String> {
    let mut args = vec!["repo".to_string(), "list".to_string()];
    if let Some(owner) = owner {
        args.push(owner.to_string());
    }
    args.push("--limit".to_string());
    args.push("1000".to_string());
    args.push("--json".to_string());
    args.push("nameWithOwner,updatedAt".to_string());
    args
}

/// Parses a completed `gh repo list --json nameWithOwner,updatedAt`
/// invocation's raw outcome (exit success + captured stdout/stderr),
/// decoupled from actually spawning the process so both the success and
/// failure paths are unit-testable without a real `gh` binary or a
/// platform-specific `ExitStatus` construction.
fn parse_gh_output(success: bool, stdout: &str, stderr: &str) -> Result<Vec<Repo>> {
    if !success {
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("gh repo list failed — try `gh auth login`");
        }
        bail!("gh repo list failed: {stderr} — try `gh auth login`");
    }

    #[derive(Deserialize)]
    struct RawRepo {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
        #[serde(rename = "updatedAt")]
        updated_at: String,
    }

    let raw: Vec<RawRepo> =
        serde_json::from_str(stdout).context("parsing `gh repo list` JSON output")?;

    let mut repos = Vec::with_capacity(raw.len());
    for r in raw {
        match r.name_with_owner.split_once('/') {
            Some((owner, name)) => repos.push(Repo {
                owner: owner.to_string(),
                name: name.to_string(),
                updated_at: r.updated_at,
            }),
            // Malformed entry (shouldn't happen against real `gh` output) —
            // skip rather than aborting the whole discovery, same tolerance
            // idiom as gitstatus.rs's read_dir_ok / cache.rs's load().
            None => continue,
        }
    }
    Ok(repos)
}

/// Lists repos for `owner` (`None` = the authenticated user's own repos) via
/// a `gh` subprocess.
pub fn list_repos(owner: Option<&str>) -> Result<Vec<Repo>> {
    let output = Command::new("gh")
        .args(repo_list_args(owner))
        .output()
        .context("running `gh repo list` — is `gh` installed and on PATH?")?;
    parse_gh_output(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

/// Lists the orgs the authenticated user belongs to, via `gh api
/// user/orgs`.
pub fn list_orgs() -> Result<Vec<String>> {
    let output = Command::new("gh")
        .args(["api", "user/orgs", "--paginate", "--jq", ".[].login"])
        .output()
        .context("running `gh api user/orgs` — is `gh` installed and on PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "listing orgs failed: {} — try `gh auth login`",
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Merges the authenticated user's own repos with one result per org,
/// isolating per-org failures: a bad org contributes a warning, not an
/// abort. Pure and separately testable from the `gh`-spawning wrapper below.
fn merge_discovered(
    personal: Vec<Repo>,
    org_results: Vec<(String, Result<Vec<Repo>>)>,
) -> DiscoverResult {
    let mut repos = personal;
    let mut warnings = Vec::new();
    for (org, result) in org_results {
        match result {
            Ok(mut org_repos) => repos.append(&mut org_repos),
            Err(e) => warnings.push(format!("org '{org}': {e}")),
        }
    }
    DiscoverResult { repos, warnings }
}

/// Discovers repos across the authenticated user's personal account plus
/// orgs. `org_filter` non-empty restricts discovery to those orgs and skips
/// `list_orgs()` entirely (the CLI's `--org` flag); empty discovers every
/// org the user belongs to. A failure listing personal repos is a hard
/// error (no isolation — there'd be nothing to show regardless); a failure
/// listing one org only warns, per-org, and discovery continues with the
/// rest.
pub fn discover_repos(org_filter: &[String]) -> Result<DiscoverResult> {
    let personal = list_repos(None).context("listing personal repos")?;

    let orgs: Vec<String> = if org_filter.is_empty() {
        list_orgs().context("listing orgs")?
    } else {
        org_filter.to_vec()
    };

    let org_results: Vec<(String, Result<Vec<Repo>>)> = orgs
        .into_iter()
        .map(|org| {
            let result = list_repos(Some(&org));
            (org, result)
        })
        .collect();

    Ok(merge_discovered(personal, org_results))
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
    fn gh_repo_list_uses_high_limit() {
        let args = repo_list_args(None);
        assert!(args.windows(2).any(|w| w == ["--limit", "1000"]));

        let args = repo_list_args(Some("imabee0"));
        assert!(args.windows(2).any(|w| w == ["--limit", "1000"]));
        assert!(args.contains(&"imabee0".to_string()));
    }

    #[test]
    fn parse_gh_output_unauthed() {
        let err = parse_gh_output(false, "", "auth error: not logged in").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gh auth login"));
    }

    #[test]
    fn parse_gh_output_empty_stdout_no_panic() {
        // Nonzero exit with genuinely empty stdout must not panic trying to
        // parse it as JSON — the exit-status check has to short-circuit
        // before that.
        let err = parse_gh_output(false, "", "").unwrap_err();
        assert!(err.to_string().contains("gh auth login"));
    }

    #[test]
    fn parse_gh_output_success() {
        let stdout = r#"[
            {"nameWithOwner": "imabee0/cw", "updatedAt": "2026-08-20T10:00:00Z"},
            {"nameWithOwner": "imabee1/other", "updatedAt": "2026-08-19T10:00:00Z"}
        ]"#;
        let repos = parse_gh_output(true, stdout, "").unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].owner, "imabee0");
        assert_eq!(repos[0].name, "cw");
        assert_eq!(repos[0].updated_at, "2026-08-20T10:00:00Z");
    }

    #[test]
    fn discover_repos_isolates_bad_org() {
        let personal = vec![repo("me", "personal-repo", "2026-01-01T00:00:00Z")];
        let org_results = vec![
            (
                "good-org".to_string(),
                Ok(vec![repo("good-org", "a", "2026-01-02T00:00:00Z")]),
            ),
            ("bad-org".to_string(), Err(anyhow::anyhow!("403 Forbidden"))),
            (
                "also-good".to_string(),
                Ok(vec![repo("also-good", "b", "2026-01-03T00:00:00Z")]),
            ),
        ];

        let result = merge_discovered(personal, org_results);

        assert_eq!(result.repos.len(), 3);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("bad-org"));
    }
}
