//! The single persistent split-pane dashboard that replaces `cw`'s three
//! sequential pickers (repo screen, then worktree+agent screen, then
//! sometimes a standalone agent screen). One `tui::run()` entry point per
//! `cw` invocation, `run()` below drives a suspend/resume loop around it so
//! the same screen survives running a hook or launching the agent CLI (both
//! need inherited stdio, which means leaving the alt-screen for the
//! duration) and comes back afterward instead of exiting to shell.
//!
//! This is the impure boundary the pure `tui::update`/`tui::model` code
//! deliberately stays out of: background threads (repo discovery, the
//! one-shot worktree scan, the clone/pull thread) are all spawned from
//! here, never from `tui::update::update_dashboard`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use anyhow::{bail, Result};
use ratatui::Frame;

use crate::agent;
use crate::cache;
use crate::cli::Cli;
use crate::config::{self, Config};
use crate::github;
use crate::gitstatus;
use crate::hooks;
use crate::sync::{self, CloneStdio};
use crate::tui::model::{DashboardModel, DashboardOutcome, Focus, Stage, SuspendReq};
use crate::tui::msg::{CloneOutcome, WorktreesLoad};
use crate::tui::{self, update, view, Msg, RepoLoad};
use crate::worktree;

/// What the dashboard is opened for — one variant per `cw` entry point,
/// matching the plan's caller table exactly (bare `cw` / `cw resume` /
/// `cw clean [--force]` / `cw scratch [SLUG]`).
pub enum Entry {
    /// Bare `cw`. `forced_repo` is set when `--repo OWNER/NAME` was passed
    /// (the interactive repo pane is seeded with just that one row, already
    /// selected, instead of the full discovered list — the repo choice
    /// itself is no longer a picker, only the worktree choice still needs
    /// one). `forced_slug` is an explicit positional SLUG.
    Browse {
        forced_repo: Option<github::Repo>,
        forced_slug: Option<String>,
    },
    /// `cw resume`.
    Resume,
    /// `cw clean [--force]`.
    Clean { force: bool },
    /// `cw scratch [SLUG]`.
    Scratch { forced_slug: Option<String> },
}

/// Runs the dashboard to completion: builds the initial model for `entry`,
/// then loops `tui::run()` — handling each `Suspend` outcome (a hook to run,
/// or the agent to launch) by leaving the terminal, doing the inherited-
/// stdio work, and re-entering with the same model — until the user cancels
/// (Esc/Ctrl-C with nothing left to back out of).
pub fn run(entry: Entry, cli: &Cli, config: &Config, root: &Path) -> Result<()> {
    if !tui::is_interactive() {
        // Per-Entry hint — `--repo OWNER/NAME` + an explicit SLUG is real
        // advice for `Browse`/`Scratch` (both have a non-interactive fast
        // path) but wrong for `Resume`/`Clean`, which take no such flags and
        // have no non-interactive form at all.
        let hint = match &entry {
            Entry::Browse { .. } => {
                "no interactive terminal available for cw's dashboard — pass --repo OWNER/NAME \
                 and an explicit SLUG instead"
            }
            Entry::Resume => {
                "no interactive terminal available for `cw resume` — it has no non-interactive \
                 form; use the default `cw --repo OWNER/NAME SLUG` flow instead"
            }
            Entry::Clean { .. } => {
                "no interactive terminal available for `cw clean` — it has no non-interactive \
                 form; remove a worktree by hand (git worktree remove) or run `cw clean` from a \
                 real terminal"
            }
            Entry::Scratch { .. } => {
                "no interactive terminal available for `cw scratch` — pass an explicit SLUG \
                 (and --dry-run to preview) instead"
            }
        };
        bail!("{hint}");
    }

    let mut screen = build_screen(entry, cli, config, root)?;
    loop {
        let (returned, outcome) = tui::run(screen)?;
        screen = returned;
        match outcome {
            DashboardOutcome::Cancelled => return Ok(()),
            DashboardOutcome::Suspend(req) => run_suspend_chain(&mut screen, req)?,
        }
    }
}

/// Resolves one `Suspend` request, and any further one `resume_after_hook`
/// immediately chains into (e.g. a clone-hook decline that lands straight on
/// a create-hook checkpoint with its own consent already on file) — without
/// re-entering `tui::run()` in between, since none of that needs the
/// terminal back. Only `LaunchAgent` is ever terminal in this chain: it's
/// always the pipeline's last stage.
fn run_suspend_chain(screen: &mut DashboardScreen, mut req: SuspendReq) -> Result<()> {
    loop {
        match req {
            SuspendReq::RunHook {
                resolved,
                kind,
                env,
            } => {
                let borrowed = env.as_borrowed();
                let result = hooks::exec_hook(&resolved, &borrowed)
                    .map(|_outcome| ())
                    .map_err(|e| format!("{e:#}"));
                match screen.model.resume_after_hook(kind, result) {
                    Some(DashboardOutcome::Suspend(next)) => req = next,
                    _ => return Ok(()),
                }
            }
            SuspendReq::LaunchAgent {
                agent: agent_cfg,
                worktree_path,
            } => {
                let result = agent::launch(&agent_cfg, &worktree_path);
                let dirty = gitstatus::is_dirty(&worktree_path).map_err(|e| format!("{e:#}"));
                // Routed through `update_dashboard` (not a direct
                // `apply_dirty_refresh` call) so a real `Msg::DirtyRefreshed`
                // is constructed here, same as every other background
                // result — the agent session may have left the worktree
                // dirty (or clean again), and the pane's cached flag needs
                // exactly this one fresh read to reflect it.
                update::update_dashboard(
                    &mut screen.model,
                    Msg::DirtyRefreshed(worktree_path.clone(), dirty),
                );
                screen.model.resume_after_launch();
                return result;
            }
        }
    }
}

// ---------------------------------------------------------------------
// Screen adapter — the impure boundary
// ---------------------------------------------------------------------

struct DashboardScreen {
    model: DashboardModel,
    /// `--no-pull`: kept on the screen, not the model — the plan's
    /// `DashboardModel` sketch has no field for it, and it only ever affects
    /// how `maybe_spawn_clone` calls `clone_or_pull_ex`, which is impure
    /// dashboard.rs territory anyway.
    no_pull: bool,
    repo_rx: Option<mpsc::Receiver<RepoLoad>>,
    worktrees_rx: mpsc::Receiver<Result<WorktreesLoad, String>>,
    clone_rx: Option<mpsc::Receiver<Result<CloneOutcome, String>>>,
}

impl tui::Screen for DashboardScreen {
    type Outcome = DashboardOutcome;

    fn update(&mut self, msg: Msg) -> Option<Self::Outcome> {
        let is_clone_done = matches!(msg, Msg::CloneDone(_));
        let outcome = update::update_dashboard(&mut self.model, msg);
        if is_clone_done {
            // Whether it succeeded or failed, this attempt is over — a
            // later Enter on a (possibly different) worktree row must be
            // able to spawn a fresh clone thread.
            self.clone_rx = None;
        }
        self.maybe_spawn_clone();
        outcome
    }

    fn draw(&self, frame: &mut Frame) {
        view::draw_dashboard(frame, &self.model)
    }

    fn poll_background(&mut self) -> Option<Msg> {
        if let Some(rx) = &self.repo_rx {
            if let Ok(load) = rx.try_recv() {
                return Some(Msg::DataLoaded(load));
            }
        }
        if let Ok(result) = self.worktrees_rx.try_recv() {
            return Some(Msg::WorktreesLoaded(result));
        }
        if let Some(rx) = &self.clone_rx {
            if let Ok(result) = rx.try_recv() {
                return Some(Msg::CloneDone(result));
            }
        }
        None
    }
}

impl DashboardScreen {
    /// Reactive, not pre-spawned (unlike the repo/worktree background
    /// threads, which start once at construction): `pending.stage` only
    /// becomes `Cloning` in response to an in-session Enter on a worktree
    /// row, so this checks after every `update()` call whether that just
    /// happened and, if so, kicks off the background `gh repo clone`/pull —
    /// captured stdio (`CloneStdio::Capture`), since inherited stdio here
    /// would corrupt the still-active alt-screen. Mirrors `github.rs`'s
    /// existing `.output()`-based convention for a subprocess run
    /// concurrently with an active TUI.
    fn maybe_spawn_clone(&mut self) {
        let needs_clone = matches!(
            self.model.pending.as_ref().map(|p| p.stage),
            Some(Stage::Cloning)
        );
        if !needs_clone || self.clone_rx.is_some() {
            return;
        }
        let ctx = self
            .model
            .pending
            .as_ref()
            .expect("needs_clone confirmed pending is Some")
            .ctx
            .clone();
        let root = self.model.root.clone();
        let pull = !self.no_pull;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result =
                sync::clone_or_pull_ex(&root, &ctx.owner, &ctx.name, pull, CloneStdio::Capture)
                    .map(|(_repo, pull_outcome)| CloneOutcome {
                        repo_label: ctx.repo_label.clone(),
                        pull_outcome,
                    })
                    .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.clone_rx = Some(rx);
    }
}

// ---------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------

fn build_screen(entry: Entry, cli: &Cli, config: &Config, root: &Path) -> Result<DashboardScreen> {
    let consent_path = config::hook_consent_path()?;
    let hook_consent = hooks::load_consent(&consent_path);
    let auto_yes = cli.yes;

    let mut repo_rx = None;
    let mut model = match entry {
        Entry::Browse {
            forced_repo,
            forced_slug,
        } => {
            let forced = forced_repo.is_some();
            let initial = if let Some(repo) = forced_repo {
                vec![repo]
            } else {
                repo_rx = Some(spawn_repo_thread(cli, config)?);
                load_cached_repos()?
            };
            let mut m = DashboardModel::new_browse(
                initial,
                root.to_path_buf(),
                config.clone(),
                hook_consent,
                consent_path,
                auto_yes,
                forced_slug,
            );
            if forced {
                // Nothing left to pick in the repo pane — start the user
                // straight on the worktree choice, which is the only real
                // decision remaining.
                m.focus = Focus::Worktrees;
                m.loading = false;
            }
            m
        }
        Entry::Resume => DashboardModel::new_all_worktrees(
            root.to_path_buf(),
            config.clone(),
            hook_consent,
            consent_path,
            auto_yes,
            false,
            false,
        ),
        Entry::Clean { force } => DashboardModel::new_all_worktrees(
            root.to_path_buf(),
            config.clone(),
            hook_consent,
            consent_path,
            auto_yes,
            true,
            force,
        ),
        Entry::Scratch { forced_slug } => {
            let repo_root = worktree::ensure_scratch_repo(root)?;
            let repo_label = format!("{}/{}", worktree::SCRATCH_OWNER, worktree::SCRATCH_REPO);
            DashboardModel::new_single_repo(
                repo_label,
                repo_root,
                root.to_path_buf(),
                config.clone(),
                hook_consent,
                consent_path,
                auto_yes,
                forced_slug,
            )
        }
    };

    if let Some(name) = cli.agent.as_deref() {
        match model.agents.iter().position(|a| a.name == name) {
            Some(pos) => model.agent_index = pos,
            None => {
                let known: Vec<&str> = model.agents.iter().map(|a| a.name.as_str()).collect();
                bail!(
                    "unknown agent '{name}' — configured agents: {}",
                    known.join(", ")
                );
            }
        }
    }

    Ok(DashboardScreen {
        model,
        no_pull: cli.no_pull,
        repo_rx,
        worktrees_rx: spawn_worktrees_thread(root.to_path_buf()),
        clone_rx: None,
    })
}

/// Mirrors the pre-rewrite `main.rs::pick_repo_interactive`'s background
/// fetch: streams a `cache::refresh_if_needed` + `github::discover_repos`
/// result in as `Msg::DataLoaded` so the dashboard opens immediately against
/// whatever's cached (`load_cached_repos`, possibly empty on a cold cache)
/// rather than blocking on the network first.
fn spawn_repo_thread(cli: &Cli, config: &Config) -> Result<mpsc::Receiver<RepoLoad>> {
    let cache_path = config::cache_path()?;
    let org_filter = cli.org.clone();
    let ttl = config.cache_ttl_minutes;
    let force_refresh = cli.refresh;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let warnings = std::cell::RefCell::new(Vec::new());
        let result = cache::refresh_if_needed(&cache_path, ttl, force_refresh, || {
            let discovered = github::discover_repos(&org_filter)?;
            *warnings.borrow_mut() = discovered.warnings;
            Ok(discovered.repos)
        });

        let load = match result {
            Ok((repos, outcome)) => {
                let repos = cache::RepoCache {
                    repos,
                    fetched_at: chrono::Utc::now(),
                }
                .sorted_repos();
                let stale_warning = match outcome {
                    cache::RefreshOutcome::Stale { warning } => Some(warning),
                    cache::RefreshOutcome::Cached | cache::RefreshOutcome::Fresh => None,
                };
                RepoLoad {
                    repos,
                    warnings: warnings.into_inner(),
                    stale_warning,
                }
            }
            // No usable cache and the live fetch failed outright — shown in
            // the dashboard's status line, not propagated as a hard process
            // error: by this point the dashboard already owns the screen
            // (possibly with an empty "loading…" repo pane).
            Err(e) => RepoLoad {
                repos: Vec::new(),
                warnings: Vec::new(),
                stale_warning: Some(format!("repo discovery failed: {e:#}")),
            },
        };
        let _ = tx.send(load);
    });
    Ok(rx)
}

fn load_cached_repos() -> Result<Vec<github::Repo>> {
    let cache_path = config::cache_path()?;
    Ok(cache::load(&cache_path)?
        .map(|c| c.sorted_repos())
        .unwrap_or_default())
}

/// Spawned once at dashboard construction regardless of `Entry`: every
/// worktree across every repo, plus a one-time `gitstatus::is_dirty` pass
/// over each — the fix for the per-keystroke `is_dirty` I/O storm
/// `WorktreeRow::existing` used to cause on every repo-cursor move (see
/// CLAUDE.md and `tui::model`'s module doc). Every later repo-cursor move is
/// a pure in-memory filter over this snapshot plus `Msg::DirtyRefreshed`
/// updates, never a fresh scan.
fn spawn_worktrees_thread(root: PathBuf) -> mpsc::Receiver<Result<WorktreesLoad, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = worktree::scan_worktrees(&root)
            .map_err(|e| format!("{e:#}"))
            .map(|entries| {
                let mut dirty = HashMap::new();
                for e in &entries {
                    let is_dirty = gitstatus::is_dirty(&e.path).unwrap_or(false);
                    dirty.insert(e.path.clone(), is_dirty);
                }
                WorktreesLoad { entries, dirty }
            });
        let _ = tx.send(result);
    });
    rx
}
