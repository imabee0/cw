mod agent;
mod cache;
mod clean;
mod cli;
mod config;
mod doctor;
mod github;
mod gitstatus;
mod hooks;
mod picker;
mod sync;
mod worktree;
mod worktreeinclude;

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Cmd};
use config::Config;

fn main() -> ExitCode {
    // Bound in main's own scope for the whole process lifetime (§5m) — a
    // guard dropped early (e.g. inside a helper that returns only `()`)
    // silently loses any log lines still buffered in the non-blocking
    // writer at exit. Never shadow or drop this before `run` returns.
    let _guard = match init_logging() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("failed to initialize logging: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    let cli = Cli::parse();

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `tracing::error!` goes through the same tee'd writer as every
            // other log call (stderr + the day's log file, §5m) — one call
            // prints the chain AND persists it, rather than risking the
            // console seeing it twice via a separate eprintln! of the same
            // text.
            tracing::error!("{e:#}");
            eprintln!("details logged to {}", log_file_hint());
            ExitCode::FAILURE
        }
    }
}

/// Sets up `tracing` + `tracing-appender` per §5m: a `Rotation::DAILY`
/// rolling file under `~/.cache/cw/`, tee'd with stderr so warnings surface
/// live AND persist. Returns the `WorkerGuard` — the caller (`main`) must
/// keep it alive for the whole process, never drop it early.
fn init_logging() -> Result<WorkerGuard> {
    let log_dir = config::log_dir()?;
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("creating log directory {}", log_dir.display()))?;
    // 0600-on-one-file doesn't survive rotation (`RollingFileAppender`
    // creates each day's file itself) — 0700 on the directory is what
    // actually holds across every rotation, and it can end up adjacent to
    // credential-helper diagnostic output (F37's discipline only helps if
    // the file itself isn't world-readable).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&log_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", log_dir.display()))?;
    }

    // `Rotation::DAILY` + `filename_prefix("cw.log")` names each day's file
    // `cw.log.<yyyy-MM-dd>` inside `log_dir` — there is no single literal
    // `cw.log` on disk (tracing-appender 0.2 always date-suffixes a rotating
    // appender's filename). `max_log_files(7)` keeps a week of history.
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("cw.log")
        .max_log_files(7)
        .build(&log_dir)
        .with_context(|| format!("initializing log file appender in {}", log_dir.display()))?;

    // Belt-and-suspenders on top of the 0700 directory above: `.build()`
    // already eagerly created today's file (tracing-appender's
    // `RollingFileAppender::new` opens it at construction, not on first
    // write), so it exists here to chmod. Best-effort only — the directory
    // permission is what actually holds across every future day's rotation,
    // since a fresh file each day would otherwise start back at the
    // process's default umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let today_log = log_dir.join(format!("cw.log.{}", chrono::Utc::now().format("%Y-%m-%d")));
        let _ = fs::set_permissions(&today_log, fs::Permissions::from_mode(0o600));
    }

    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(io::stderr.and(file_writer))
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .init();

    Ok(guard)
}

/// The real (day-stamped) log file a reader should look at, for the
/// top-level error hint — see `init_logging`'s comment on why this isn't a
/// literal `cw.log`.
fn log_file_hint() -> String {
    let today = chrono::Utc::now().format("%Y-%m-%d");
    format!("~/.cache/cw/cw.log.{today}")
}

fn run(cli: Cli) -> Result<()> {
    // Completions need neither config nor a resolved root — handled before
    // either is loaded.
    if let Some(Cmd::Completions { shell }) = cli.cmd {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, name, &mut io::stdout());
        return Ok(());
    }

    let config = config::load()?;
    let root = resolve_root(cli.root.as_deref(), &config)?;

    match &cli.cmd {
        Some(Cmd::Doctor) => run_doctor_cmd(&config),
        Some(Cmd::Resume) => run_resume(&cli, &config, &root),
        Some(Cmd::Clean { force }) => clean::run_clean(&config, &root, *force),
        Some(Cmd::Scratch { slug, dry_run }) => {
            run_scratch(&cli, &config, &root, slug.clone(), *dry_run)
        }
        Some(Cmd::Completions { .. }) => unreachable!("handled above"),
        None => run_default(&cli, &config, &root),
    }
}

/// `--root`/`config.root` with `~` expanded — pure, no filesystem mutation,
/// so it's safe to call unconditionally, including on the `--dry-run` path
/// (§7b #15's "no mutation" must hold literally). Root creation itself is
/// handled by whichever call actually needs it to exist
/// (`sync::clone_or_pull`'s clone path, `worktree::ensure_scratch_repo`) —
/// `scan_worktrees` already tolerates a nonexistent root as "no worktrees".
fn resolve_root(cli_root: Option<&Path>, config: &Config) -> Result<PathBuf> {
    let raw = cli_root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.root.clone());
    expand_tilde(&raw)
}

fn expand_tilde(path: &Path) -> Result<PathBuf> {
    if let Ok(rest) = path.strip_prefix("~") {
        Ok(config::home_dir()?.join(rest))
    } else {
        Ok(path.to_path_buf())
    }
}

fn generate_timestamp_slug() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

fn parse_owner_repo(spec: &str) -> Result<github::Repo> {
    let (owner, name) = spec
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty())
        .with_context(|| format!("--repo expects OWNER/NAME, got '{spec}'"))?;
    Ok(github::Repo {
        owner: owner.to_string(),
        name: name.to_string(),
        updated_at: String::new(),
    })
}

/// The path a worktree for `slug` would live at under `repo_root`, plus
/// whether it already exists — computed BEFORE calling
/// `worktree::create_or_resume_worktree`, since that function's return value
/// alone doesn't say whether it just created a worktree or fast-resumed an
/// existing one, and callers need that distinction to decide whether
/// symlinking/`.worktreeinclude`/hooks should run at all. Delegates to
/// `worktree::worktree_path_and_exists` — the same predicate
/// `create_or_resume_worktree` itself uses — so this precheck can't drift
/// out of sync with what "already exists" actually means there.
fn worktree_precheck(repo_root: &Path, slug: &str) -> (PathBuf, bool) {
    worktree::worktree_path_and_exists(repo_root, slug)
}

/// Whether `--agent`/`default_agent` need the interactive picker at all
/// (§5l, F15): only when `--agent` was NOT passed AND `default_agent`
/// doesn't name a real entry in `config.agents` — never simply "more than
/// one agent is configured" (§3 ships three by default, which would
/// otherwise make the picker fire on every single invocation).
fn agent_picker_needed(explicit: Option<&str>, cfg: &Config) -> bool {
    if explicit.is_some() {
        return false;
    }
    !cfg.agents.contains_key(&cfg.default_agent)
}

/// Resolves which agent NAME to launch, prompting interactively only when
/// `agent_picker_needed` says so. `Ok(None)` means the user cancelled out of
/// the picker — callers print a "cancelled" message and stop, rather than
/// falling back to a default the user never confirmed.
fn resolve_agent_name(explicit: Option<&str>, config: &Config) -> Result<Option<String>> {
    if let Some(name) = explicit {
        return Ok(Some(name.to_string()));
    }
    if !agent_picker_needed(explicit, config) {
        return Ok(Some(config.default_agent.clone()));
    }
    match picker::pick_agent(&config.agents)? {
        picker::Pick::Selected(name) => Ok(Some(name)),
        picker::Pick::Empty | picker::Pick::Cancelled => Ok(None),
    }
}

/// §0a: with an explicit slug this is a pure pass-through. Otherwise, scan
/// worktrees already belonging to `repo_label` — if any exist, offer a
/// picker (existing worktrees + "+ new worktree"); only auto-generate a
/// fresh timestamp slug when none exist yet, or the user explicitly picks
/// "new". `Ok(None)` means the user cancelled the picker.
fn determine_slug(
    explicit: Option<&str>,
    root: &Path,
    repo_label: &str,
    config: &Config,
) -> Result<Option<String>> {
    if let Some(s) = explicit {
        return Ok(Some(s.to_string()));
    }

    let same_repo: Vec<worktree::WorktreeEntry> = worktree::scan_worktrees(root)?
        .into_iter()
        .filter(|e| e.repo == repo_label)
        .collect();
    if same_repo.is_empty() {
        return Ok(Some(generate_timestamp_slug()));
    }

    match picker::pick_worktree(same_repo, config.idle_threshold_days, true)? {
        picker::Pick::Selected(picker::WorktreeSelection::Existing(entry)) => {
            // MUST unflatten here: `entry.slug` is the on-disk (flattened)
            // directory name, and the caller passes this straight to
            // `create_or_resume_worktree`, whose first step
            // (`validate_worktree_slug`) rejects a literal '+'. Regression
            // test: `tests::resumed_slug_survives_scan_and_unflatten_round_trip`
            // — that test exercises `unflatten_slug` directly, not this match
            // arm (which requires a TTY to reach via `pick_worktree`), so it
            // will NOT catch a revert of this line back to `entry.slug` alone.
            Ok(Some(unflatten_slug(&entry.slug)))
        }
        picker::Pick::Selected(picker::WorktreeSelection::New) => {
            Ok(Some(generate_timestamp_slug()))
        }
        picker::Pick::Empty => {
            unreachable!("pick_worktree is only called here with a non-empty `same_repo` list")
        }
        picker::Pick::Cancelled => Ok(None),
    }
}

/// Reverses `worktree::flatten_slug` on a slug read back off disk (e.g.
/// `scan_worktrees`'s `entry.slug`, taken from the flattened directory
/// name). Safe precisely because `validate_worktree_slug` rejects a literal
/// `+` in any raw slug segment (§5d) — no slug `create_or_resume_worktree`
/// ever accepted could already contain the character flattening introduces,
/// so this reversal can't misfire on a slug that legitimately contained `+`.
/// Without this, §0a's existing-worktrees picker fed a flattened slug
/// straight back into `create_or_resume_worktree`, whose first step
/// (`validate_worktree_slug`) then rejected it — resuming any worktree whose
/// raw slug contained `/` was unreachable through the picker.
fn unflatten_slug(flat: &str) -> String {
    flat.replace('+', "/")
}

fn interactive_confirm(resolved: &hooks::ResolvedHook) -> bool {
    // Never block on a piped/absent stdin: no controlling terminal means
    // decline, not hang — the same non-interactive discipline §5j's picker
    // prechecks apply, extended to this y/N prompt.
    if !picker::is_interactive() {
        return false;
    }
    println!(
        "about to run: {} {}",
        resolved.program,
        resolved.args.join(" ")
    );
    println!("  cwd: {}", resolved.cwd.display());
    print!("proceed? [y/N] ");
    if io::stdout().flush().is_err() {
        return false;
    }
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

fn report_hook_outcome(label: &str, outcome: hooks::HookOutcome) {
    match outcome {
        hooks::HookOutcome::NotConfigured => {}
        hooks::HookOutcome::Declined => println!("{label}: declined, skipped"),
        hooks::HookOutcome::Ran { status } => {
            if status.success() {
                println!("{label}: completed");
            }
            // A nonzero exit is already logged as a warning inside
            // hooks::exec_hook (warn-and-continue, §5h) — nothing more to
            // report here.
        }
    }
}

/// Runs `post_clone_hook` if configured, gated by the confirm-once-per-repo
/// consent store (§5h). Only ever called right after a fresh `gh repo
/// clone` — never on a pull of an already-cloned repo, and never for
/// `cw scratch` (there's no clone event to trigger it).
#[allow(clippy::too_many_arguments)]
fn run_post_clone_hook_step(
    config: &Config,
    auto_yes: bool,
    repo_label: &str,
    repo_root: &Path,
    worktree_path_hint: &Path,
    slug: &str,
    agent_name: &str,
) -> Result<()> {
    if config.post_clone_hook.is_none() {
        return Ok(());
    }
    let consent_path = config::hook_consent_path()?;
    let mut consent = hooks::load_consent(&consent_path);
    let env = hooks::HookEnv {
        repo: repo_label,
        worktree_path: worktree_path_hint,
        slug,
        agent: agent_name,
    };
    let mut session = hooks::HookSession {
        repo: repo_label,
        consent: &mut consent,
        auto_yes,
    };
    let result = hooks::run_post_clone_hook(
        &mut session,
        repo_root,
        config.post_clone_hook.as_deref(),
        &env,
        interactive_confirm,
    );
    hooks::save_consent(&consent_path, &consent)?;
    report_hook_outcome("post_clone_hook", result?);
    Ok(())
}

/// Finishes creating a freshly-made worktree: symlinks configured shared
/// dirs (§5c-note), copies `.worktreeinclude` matches (§5f), then runs
/// `post_create_hook` if configured (§5h). Never called on a fast-resumed
/// worktree — callers only invoke this when `worktree_precheck` reported
/// `existed == false`.
#[allow(clippy::too_many_arguments)]
fn finish_worktree_creation(
    config: &Config,
    auto_yes: bool,
    repo_label: &str,
    repo_root: &Path,
    worktree_path: &Path,
    slug: &str,
    agent_name: &str,
) -> Result<()> {
    worktree::symlink_shared_dirs(repo_root, worktree_path, &config.symlink_dirs)?;

    let failures = worktreeinclude::apply_worktreeinclude(repo_root, worktree_path)?;
    for f in &failures {
        tracing::warn!(
            file = %f.path.display(),
            error = %f.error,
            "worktreeinclude: failed to copy file, continuing"
        );
    }

    if config.post_create_hook.is_none() {
        return Ok(());
    }
    let consent_path = config::hook_consent_path()?;
    let mut consent = hooks::load_consent(&consent_path);
    let env = hooks::HookEnv {
        repo: repo_label,
        worktree_path,
        slug,
        agent: agent_name,
    };
    let mut session = hooks::HookSession {
        repo: repo_label,
        consent: &mut consent,
        auto_yes,
    };
    let result = hooks::run_post_create_hook(
        &mut session,
        repo_root,
        worktree_path,
        config.post_create_hook.as_deref(),
        &env,
        interactive_confirm,
    );
    hooks::save_consent(&consent_path, &consent)?;
    report_hook_outcome("post_create_hook", result?);
    Ok(())
}

fn report_pull_outcome(repo_label: &str, outcome: sync::PullOutcome) {
    match outcome {
        sync::PullOutcome::Cloned => println!("cloned {repo_label}"),
        sync::PullOutcome::UpToDate => {}
        sync::PullOutcome::FastForwarded => println!("pulled latest changes for {repo_label}"),
        sync::PullOutcome::Diverged => println!(
            "warning: {repo_label}'s local branch has diverged from origin — left untouched"
        ),
        sync::PullOutcome::DirtyLocalChanges => println!(
            "warning: {repo_label} has uncommitted local changes — skipped pull to avoid discarding them"
        ),
        sync::PullOutcome::Skipped => {}
    }
}

fn pick_repo_interactive(cli: &Cli, config: &Config) -> Result<picker::Pick<github::Repo>> {
    let cache_path = config::cache_path()?;
    let org_filter = cli.org.clone();
    let (repos, outcome) =
        cache::refresh_if_needed(&cache_path, config.cache_ttl_minutes, cli.refresh, || {
            let result = github::discover_repos(&org_filter)?;
            for w in &result.warnings {
                tracing::warn!("{w}");
            }
            Ok(result.repos)
        })?;
    match &outcome {
        cache::RefreshOutcome::Cached | cache::RefreshOutcome::Fresh => {}
        cache::RefreshOutcome::Stale { warning } => eprintln!("warning: {warning}"),
    }
    // `sorted_repos()` does the most-recent-first ordering (§5a/§5k) — reuse
    // it here rather than re-sorting inline, `fetched_at` is irrelevant to
    // that method and only needed to satisfy the struct's shape.
    let repos = cache::RepoCache {
        repos,
        fetched_at: chrono::Utc::now(),
    }
    .sorted_repos();
    picker::pick_repo(repos)
}

fn run_default(cli: &Cli, config: &Config, root: &Path) -> Result<()> {
    if cli.dry_run {
        return run_dry_run(cli, config, root);
    }

    let repo_choice = if let Some(spec) = &cli.repo {
        parse_owner_repo(spec)?
    } else {
        match pick_repo_interactive(cli, config)? {
            picker::Pick::Selected(r) => r,
            picker::Pick::Empty | picker::Pick::Cancelled => return Ok(()),
        }
    };
    let repo_label = repo_choice.full_name();

    let (git_repo, pull_outcome) =
        sync::clone_or_pull(root, &repo_choice.owner, &repo_choice.name, !cli.no_pull)?;
    report_pull_outcome(&repo_label, pull_outcome);

    let Some(slug) = determine_slug(cli.slug.as_deref(), root, &repo_label, config)? else {
        println!("cancelled");
        return Ok(());
    };

    let Some(agent_name) = resolve_agent_name(cli.agent.as_deref(), config)? else {
        println!("cancelled");
        return Ok(());
    };

    let repo_root = git_repo
        .workdir()
        .context("repo has no working directory")?
        .to_path_buf();
    let (precreate_path, was_existing) = worktree_precheck(&repo_root, &slug);

    if matches!(pull_outcome, sync::PullOutcome::Cloned) {
        run_post_clone_hook_step(
            config,
            cli.yes,
            &repo_label,
            &repo_root,
            &precreate_path,
            &slug,
            &agent_name,
        )?;
    }

    let worktree_path = worktree::create_or_resume_worktree(&git_repo, &slug, "HEAD")?;

    if !was_existing {
        finish_worktree_creation(
            config,
            cli.yes,
            &repo_label,
            &repo_root,
            &worktree_path,
            &slug,
            &agent_name,
        )?;
    }

    let agent_cfg = config::resolve_agent(Some(&agent_name), config)?;
    agent::launch(&agent_cfg, &worktree_path)
}

/// `--dry-run` (requires `--repo` AND an explicit SLUG, F17): prints the
/// clone-vs-pull decision, resolved worktree path, which hook(s) would run,
/// and which agent would launch — performs no mutation whatsoever. Neither
/// the interactive repo picker nor §0a's existing-worktrees picker nor the
/// agent picker is ever invoked on this path.
fn run_dry_run(cli: &Cli, config: &Config, root: &Path) -> Result<()> {
    let repo_spec = cli
        .repo
        .as_deref()
        .context("--dry-run requires --repo OWNER/NAME")?;
    let slug = cli
        .slug
        .as_deref()
        .context("--dry-run requires an explicit SLUG")?;
    worktree::validate_worktree_slug(slug)?;
    let repo_choice = parse_owner_repo(repo_spec)?;
    let repo_label = repo_choice.full_name();

    let local_path = sync::resolve_local_path(root, &repo_choice.owner, &repo_choice.name);
    let already_cloned = local_path.join(".git").exists();
    let pull_decision = if !already_cloned {
        "clone (not yet present locally)".to_string()
    } else if cli.no_pull {
        "skip pull (--no-pull)".to_string()
    } else {
        "pull/fast-forward".to_string()
    };

    let (worktree_path, worktree_exists) = worktree_precheck(&local_path, slug);

    let agent_name = cli
        .agent
        .clone()
        .unwrap_or_else(|| config.default_agent.clone());
    let agent_cfg = config::resolve_agent(Some(&agent_name), config)?;

    println!("would resolve repo: {repo_label}");
    println!("clone/pull: {pull_decision}");
    println!("worktree path: {}", worktree_path.display());
    println!("worktree exists already: {worktree_exists}");
    if worktree_exists {
        println!("worktree already exists — fast-resume, hooks skipped");
    } else {
        if !config.symlink_dirs.is_empty() {
            println!("would symlink: {}", config.symlink_dirs.join(", "));
        }
        if !already_cloned {
            if let Some(hook) = &config.post_clone_hook {
                println!("would run post_clone_hook: {}", hook.display());
            }
        }
        print_create_hook_preview(&config.post_create_hook, &local_path);
    }
    println!("agent: {agent_name} ({})", agent_cfg.cmd);
    Ok(())
}

fn print_create_hook_preview(hook: &Option<String>, repo_root: &Path) {
    match hook.as_deref() {
        Some("auto") => match hooks::detect_auto_hook(repo_root) {
            Some((prog, args)) => {
                println!(
                    "would run post_create_hook (auto): {prog} {}",
                    args.join(" ")
                )
            }
            None => println!("post_create_hook: auto, but no known lockfile present"),
        },
        Some(h) => println!("would run post_create_hook: {h}"),
        None => {}
    }
}

fn run_resume(cli: &Cli, config: &Config, root: &Path) -> Result<()> {
    let entries = worktree::scan_worktrees(root)?;
    let selection = match picker::pick_worktree(entries, config.idle_threshold_days, false)? {
        picker::Pick::Selected(s) => s,
        picker::Pick::Empty | picker::Pick::Cancelled => return Ok(()),
    };
    let entry = match selection {
        picker::WorktreeSelection::Existing(e) => e,
        picker::WorktreeSelection::New => {
            unreachable!("pick_worktree(include_new=false) never returns New")
        }
    };

    let Some(agent_name) = resolve_agent_name(cli.agent.as_deref(), config)? else {
        println!("cancelled");
        return Ok(());
    };
    let agent_cfg = config::resolve_agent(Some(&agent_name), config)?;
    agent::launch(&agent_cfg, &entry.path)
}

fn run_scratch(
    cli: &Cli,
    config: &Config,
    root: &Path,
    slug: Option<String>,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return run_scratch_dry_run(cli, config, root, slug.as_deref());
    }

    let repo_root = worktree::ensure_scratch_repo(root)?;
    let git_repo = git2::Repository::open(&repo_root)
        .with_context(|| format!("opening scratch repo at {}", repo_root.display()))?;
    let repo_label = format!("{}/{}", worktree::SCRATCH_OWNER, worktree::SCRATCH_REPO);

    let Some(slug) = determine_slug(slug.as_deref(), root, &repo_label, config)? else {
        println!("cancelled");
        return Ok(());
    };

    let Some(agent_name) = resolve_agent_name(cli.agent.as_deref(), config)? else {
        println!("cancelled");
        return Ok(());
    };

    let (_, was_existing) = worktree_precheck(&repo_root, &slug);
    let worktree_path = worktree::create_or_resume_worktree(&git_repo, &slug, "HEAD")?;

    if !was_existing {
        // `post_clone_hook` never fires here — there is no `gh repo clone`
        // event for a scratch worktree (§3, §5n).
        finish_worktree_creation(
            config,
            cli.yes,
            &repo_label,
            &repo_root,
            &worktree_path,
            &slug,
            &agent_name,
        )?;
    }

    let agent_cfg = config::resolve_agent(Some(&agent_name), config)?;
    agent::launch(&agent_cfg, &worktree_path)
}

/// Same rationale as the top-level `--dry-run` (F17): the existing-
/// worktrees-first picker (§0a applies to scratch too, per §4) is
/// interactive, so a dry-run preview requires an explicit SLUG to bypass it
/// rather than blocking a TTY-less invocation.
fn run_scratch_dry_run(cli: &Cli, config: &Config, root: &Path, slug: Option<&str>) -> Result<()> {
    let slug = slug.context("cw scratch --dry-run requires an explicit SLUG")?;
    worktree::validate_worktree_slug(slug)?;

    let repo_label = format!("{}/{}", worktree::SCRATCH_OWNER, worktree::SCRATCH_REPO);
    let repo_root = root
        .join(worktree::SCRATCH_OWNER)
        .join(worktree::SCRATCH_REPO);
    let scratch_repo_exists = repo_root.join(".git").exists();
    let (worktree_path, worktree_exists) = worktree_precheck(&repo_root, slug);

    let agent_name = cli
        .agent
        .clone()
        .unwrap_or_else(|| config.default_agent.clone());
    let agent_cfg = config::resolve_agent(Some(&agent_name), config)?;

    println!("scratch repo: {repo_label} (at {})", repo_root.display());
    println!("scratch repo exists already: {scratch_repo_exists}");
    println!("worktree path: {}", worktree_path.display());
    println!("worktree exists already: {worktree_exists}");
    if worktree_exists {
        println!("worktree already exists — fast-resume, hooks skipped");
    } else {
        if !config.symlink_dirs.is_empty() {
            println!("would symlink: {}", config.symlink_dirs.join(", "));
        }
        print_create_hook_preview(&config.post_create_hook, &repo_root);
    }
    println!("agent: {agent_name} ({})", agent_cfg.cmd);
    Ok(())
}

fn run_doctor_cmd(config: &Config) -> Result<()> {
    let checks = doctor::run_doctor(config);
    let mut any_failed = false;
    for (name, result) in &checks {
        match result {
            Ok(()) => println!("{name}: ok"),
            Err(e) => {
                println!("{name}: FAIL: {e:#}");
                any_failed = true;
            }
        }
    }
    if any_failed {
        bail!("one or more doctor checks failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg_with_agents(default_agent: &str, names: &[&str]) -> Config {
        let mut agents = HashMap::new();
        for n in names {
            agents.insert(
                (*n).to_string(),
                config::AgentConfig {
                    cmd: (*n).to_string(),
                    args: vec![],
                },
            );
        }
        Config {
            default_agent: default_agent.to_string(),
            agents,
            ..Config::default()
        }
    }

    #[test]
    fn pick_agent_skipped_when_default_valid() {
        let cfg = cfg_with_agents("claude", &["claude", "grok", "shell"]);
        assert!(
            !agent_picker_needed(None, &cfg),
            "a valid default_agent must skip the picker"
        );
        assert!(
            !agent_picker_needed(Some("grok"), &cfg),
            "an explicit --agent always skips the picker, regardless of default_agent"
        );

        let cfg_invalid = cfg_with_agents("gpt", &["claude", "grok", "shell"]);
        assert!(
            agent_picker_needed(None, &cfg_invalid),
            "an invalid default_agent must trigger the picker"
        );

        let mut cfg_unset = cfg_with_agents("claude", &["claude"]);
        cfg_unset.default_agent = String::new();
        assert!(
            agent_picker_needed(None, &cfg_unset),
            "an empty/unset default_agent must trigger the picker"
        );
    }

    #[test]
    fn unflatten_slug_reverses_flatten() {
        assert_eq!(unflatten_slug("foo+bar"), "foo/bar");
        assert_eq!(unflatten_slug("plain"), "plain");
        assert_eq!(unflatten_slug(&worktree::flatten_slug("a/b/c")), "a/b/c");
    }

    /// Regression guard for the bug the advisor pass caught: §0a's
    /// existing-worktrees picker returns `entry.slug` straight off
    /// `scan_worktrees`, which reads it from the on-disk (already flattened)
    /// directory name. Feeding that verbatim into
    /// `create_or_resume_worktree` — whose first step is
    /// `validate_worktree_slug`, which rejects a literal `+` by design — made
    /// resuming any worktree whose raw slug contained `/` fail every time
    /// `determine_slug` picked it back up. This drives the exact round trip
    /// `determine_slug`'s Existing arm performs, without needing an
    /// interactive picker.
    #[test]
    fn resumed_slug_survives_scan_and_unflatten_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo_dir = root.join("acme").join("proj");
        fs::create_dir_all(&repo_dir).unwrap();
        let repo = git2::Repository::init(&repo_dir).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        fs::write(repo_dir.join("README.md"), "hi").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("README.md")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }

        let raw_slug = "myfeat/x";
        let created = worktree::create_or_resume_worktree(&repo, raw_slug, "HEAD").unwrap();

        let scanned = worktree::scan_worktrees(root).unwrap();
        let entry = scanned
            .into_iter()
            .find(|e| e.repo == "acme/proj")
            .expect("scan_worktrees must find the worktree just created");
        // Confirms the premise: what comes back off disk IS the flattened
        // form, not the raw slug.
        assert_eq!(entry.slug, "myfeat+x");

        let resumed_slug = unflatten_slug(&entry.slug);
        assert_eq!(resumed_slug, raw_slug);
        assert!(worktree::validate_worktree_slug(&resumed_slug).is_ok());

        // The actual regression: feeding the unflattened slug back through
        // `create_or_resume_worktree` must fast-resume the same path, not
        // error on a rejected '+' segment.
        let resumed = worktree::create_or_resume_worktree(&repo, &resumed_slug, "HEAD").unwrap();
        assert_eq!(resumed, created);
    }

    #[test]
    fn parse_owner_repo_rejects_malformed_spec() {
        assert!(parse_owner_repo("imabee0/cw").is_ok());
        assert!(parse_owner_repo("no-slash").is_err());
        assert!(parse_owner_repo("/missing-owner").is_err());
        assert!(parse_owner_repo("missing-name/").is_err());
    }

    #[test]
    fn expand_tilde_rewrites_leading_component_only() {
        // SAFETY: test-only env mutation, single-threaded within this test.
        unsafe { std::env::set_var("HOME", "/home/test-user") };
        let expanded = expand_tilde(Path::new("~/repos")).unwrap();
        assert_eq!(expanded, PathBuf::from("/home/test-user/repos"));

        let unchanged = expand_tilde(Path::new("/already/absolute")).unwrap();
        assert_eq!(unchanged, PathBuf::from("/already/absolute"));
        unsafe { std::env::remove_var("HOME") };
    }
}

fn    deliberately_unformatted(   )  {
    let x=1;
let y = 2;
}
