//! `DashboardModel`: the single composite Model backing `cw`'s persistent
//! split-pane dashboard (`dashboard.rs`). One screen, one model, for the
//! whole session, surviving suspend/resume round trips out to a hook or an
//! agent launch.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use super::msg::{CloneOutcome, RepoLoad, WorktreesLoad};
use super::widgets::filter_indices;
use crate::config::{self, AgentConfig, Config};
use crate::doctor;
use crate::github::Repo;
use crate::gitstatus::WorkState;
use crate::hooks::{self, HookConsent, HookEnv, ResolvedHook};
use crate::sync::{self, PullOutcome};
use crate::worktree::{self, WorktreeEntry as ScannedEntry, WorktreeSelection};
use crate::worktreeinclude;

/// The worktree pane's AGE cell: `"idle Nd"` once `threshold_days` is
/// reached (the `bool` is that idle flag, for dimming), otherwise a coarse
/// "Nm/Nh/Nd ago". `now` is a parameter (not `SystemTime::now()` inline) so
/// this stays testable without real elapsed time.
pub fn age_label(mtime: SystemTime, now: SystemTime, threshold_days: u64) -> (String, bool) {
    let secs = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
    let days = secs / 86_400;
    if days >= threshold_days {
        return (format!("idle {days}d"), true);
    }
    let label = if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{days}d ago")
    };
    (label, false)
}

/// Coarse "Nm/Nh/Nd ago" label for the repo pane's UPDATED column, computed
/// from `Repo.updated_at`'s raw ISO8601 string against `now`. A timestamp
/// that fails to parse (shouldn't happen against real `gh` output) falls
/// back to the raw string rather than panicking or blanking the column.
pub fn relative_time(updated_at: &str, now: DateTime<Utc>) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(updated_at) else {
        return updated_at.to_string();
    };
    let secs = now
        .signed_duration_since(parsed.with_timezone(&Utc))
        .num_seconds()
        .max(0);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Whether `updated_at` is under 24h old — drives the repo pane's green `★ `
/// prefix (visual style).
pub fn is_recently_updated(updated_at: &str, now: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(updated_at)
        .map(|parsed| {
            now.signed_duration_since(parsed.with_timezone(&Utc))
                .num_seconds()
                < 86_400
        })
        .unwrap_or(false)
}

/// Shared filterable-table state: the full item list, live filter query,
/// the filtered index subset (`filtered[i]` is an index into `items`), the
/// `TableState` (selection/scroll), and the table's last-rendered `Rect`s.
///
/// `table`/`table_rect`/`pane_rect` use interior mutability
/// (`RefCell`/`Cell`) deliberately: `Screen::draw` takes `&Model` (never
/// widened to `&mut self` just to let rendering write back state), yet
/// `ratatui::widgets::Table` is a `StatefulWidget` that mutates
/// `TableState.offset` during `render` to keep the selection scrolled into
/// view, and a later mouse click must hit-test against that up-to-date
/// offset. `query`/`filtered` stay plain fields — nothing renders into
/// them, only `update` ever touches them, through a plain `&mut Model`.
pub struct ListState<T> {
    pub items: Vec<T>,
    pub query: String,
    pub filtered: Vec<usize>,
    pub table: RefCell<TableState>,
    /// The table's content area (header row first) — row hit-testing.
    pub table_rect: Cell<Rect>,
    /// The whole bordered pane — a click anywhere inside focuses the pane
    /// even when it lands on the border, the title, or empty space.
    pub pane_rect: Cell<Rect>,
}

impl<T> ListState<T> {
    pub fn new(items: Vec<T>) -> Self {
        let filtered: Vec<usize> = (0..items.len()).collect();
        let mut table = TableState::default();
        if !filtered.is_empty() {
            table.select(Some(0));
        }
        Self {
            items,
            query: String::new(),
            filtered,
            table: RefCell::new(table),
            table_rect: Cell::new(Rect::default()),
            pane_rect: Cell::new(Rect::default()),
        }
    }

    /// Recomputes `filtered` against the current `query`, clamping the
    /// selection into range (or clearing it when nothing matches).
    pub fn refilter(&mut self, text_of: impl Fn(&T) -> &str) {
        self.filtered = filter_indices(&self.items, text_of, &self.query);
        let mut table = self.table.borrow_mut();
        match self.filtered.len() {
            0 => table.select(None),
            len => {
                let current = table.selected().unwrap_or(0);
                table.select(Some(current.min(len - 1)));
            }
        }
    }

    pub fn move_selection(&self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        let mut table = self.table.borrow_mut();
        let current = table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1);
        table.select(Some(next as usize));
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.table.borrow().selected()
    }

    pub fn select(&self, idx: Option<usize>) {
        self.table.borrow_mut().select(idx);
    }

    pub fn offset(&self) -> usize {
        self.table.borrow().offset()
    }

    /// The item under the current selection, or `None` when the filter
    /// matched nothing (an empty list, or every row filtered out).
    pub fn selected(&self) -> Option<&T> {
        self.selected_index()
            .and_then(|i| self.filtered.get(i))
            .and_then(|&idx| self.items.get(idx))
    }
}

// ---------------------------------------------------------------------
// Repo pane (Scope::Browse only)
// ---------------------------------------------------------------------

/// A repo, pre-annotated for the repo pane's table + fuzzy filter.
pub struct RepoRow {
    pub repo: Repo,
    /// Whether `root/owner/name/.git` already exists — the LOCAL column, so
    /// the user sees clone-vs-pull cost before picking.
    pub local: bool,
    pub filter_text: String,
}

impl RepoRow {
    fn new(repo: Repo, root: &Path) -> Self {
        let local = sync::resolve_local_path(root, &repo.owner, &repo.name)
            .join(".git")
            .exists();
        let filter_text = repo.full_name();
        Self {
            repo,
            local,
            filter_text,
        }
    }
}

fn build_repo_rows(repos: Vec<Repo>, root: &Path) -> Vec<RepoRow> {
    repos.into_iter().map(|r| RepoRow::new(r, root)).collect()
}

// ---------------------------------------------------------------------
// Worktree pane
// ---------------------------------------------------------------------

/// A worktree row (or the synthetic "+ new worktree" row, offered only when
/// the pane's scope has a concrete repo to create one under), annotated with
/// age and work state at construction. `state` is looked up from
/// `DashboardModel::work_cache` — never computed here via a live git call,
/// which is exactly the per-keystroke I/O bug the background scan exists
/// to avoid. `None` = the state couldn't be read (treated as valuable, not
/// disposable, by the removal confirm).
pub struct WorktreeRow {
    pub selection: WorktreeSelection,
    pub state: Option<WorkState>,
    pub age_label: String,
    pub idle: bool,
    pub repo_label: String,
    pub filter_text: String,
}

impl WorktreeRow {
    fn existing(
        entry: ScannedEntry,
        state: Option<WorkState>,
        idle_threshold_days: u64,
        now: SystemTime,
    ) -> Self {
        let repo_label = worktree::display_repo_label(&entry.repo);
        let (age_label, idle) = age_label(entry.mtime, now, idle_threshold_days);
        let filter_text = format!("{repo_label}/{}", entry.slug);
        Self {
            selection: WorktreeSelection::Existing(entry),
            state,
            age_label,
            idle,
            repo_label,
            filter_text,
        }
    }

    fn new_row(repo_label: &str) -> Self {
        Self {
            selection: WorktreeSelection::New,
            state: Some(WorkState::default()),
            age_label: String::new(),
            idle: false,
            repo_label: worktree::display_repo_label(repo_label),
            filter_text: "+ new worktree".to_string(),
        }
    }
}

fn build_worktree_rows(
    entries: impl Iterator<Item = ScannedEntry>,
    work_cache: &HashMap<PathBuf, Option<WorkState>>,
    idle_threshold_days: u64,
    new_under: Option<&str>,
) -> Vec<WorktreeRow> {
    let now = SystemTime::now();
    let mut rows: Vec<WorktreeRow> = entries
        .map(|e| {
            let state = work_cache.get(&e.path).copied().flatten();
            WorktreeRow::existing(e, state, idle_threshold_days, now)
        })
        .collect();
    if let Some(repo_label) = new_under {
        rows.push(WorktreeRow::new_row(repo_label));
    }
    rows
}

/// An agent, annotated with its resolved command-line preview and whether
/// its binary is actually on `PATH` right now — the footer's segmented
/// control (`←`/`→`/`Ctrl-A` or a click select; not a `ListState` — small
/// and fixed, never filtered).
pub struct AgentEntry {
    pub name: String,
    pub cmd_preview: String,
    pub installed: bool,
}

impl AgentEntry {
    fn new(name: String, cfg: &AgentConfig, installed: bool) -> Self {
        let mut cmd_preview = cfg.cmd.clone();
        for arg in &cfg.args {
            cmd_preview.push(' ');
            cmd_preview.push_str(arg);
        }
        Self {
            name,
            cmd_preview,
            installed,
        }
    }
}

fn build_agent_entries(config: &Config) -> Vec<AgentEntry> {
    // Sorted, not `HashMap` iteration order — deterministic run to run.
    let mut names: Vec<&String> = config.agents.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            // Resolved (not the raw `cmd`) so `$SHELL` is checked as the
            // real binary it expands to — same as `cw doctor`.
            let installed = config::resolve_agent(Some(name), config)
                .map(|a| doctor::check_binary_on_path(&a.cmd).is_ok())
                .unwrap_or(false);
            AgentEntry::new(name.clone(), &config.agents[name], installed)
        })
        .collect()
}

// ---------------------------------------------------------------------
// Scope / Focus / Actions
// ---------------------------------------------------------------------

/// What the dashboard is showing — the semantic mode only; the repo pane
/// itself is a top-level `DashboardModel` field (`Some` only in `Browse`,
/// see `DashboardModel::repos`), not carried in this enum, so `update.rs`
/// never has to re-destructure `Scope` just to reach it. `Browse` is bare
/// `cw`'s repo-pane-plus-worktree-pane split view; `AllWorktrees` backs both
/// `cw resume` and `cw clean` (`--force` is the only difference between
/// them — both render identically, worktree pane only); `SingleRepo` backs
/// `cw scratch`, scoped to the synthetic scratch repo with no repo pane at
/// all.
pub enum Scope {
    Browse,
    AllWorktrees,
    SingleRepo {
        repo_label: String,
        repo_root: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Repos,
    Worktrees,
}

enum PaneRepoFilter {
    All,
    One(String),
}

/// A clickable region's meaning. `view.rs` registers one `(Rect, Action)`
/// pair per clickable thing it draws (agent segments, help-line keys, modal
/// buttons, the mark column) into `DashboardModel::hotspots`; `update.rs`
/// resolves a click against them and performs the action through exactly
/// the same code path the equivalent key uses — so every action reachable
/// by keyboard is reachable by mouse, and neither can drift from the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SelectAgent(usize),
    CycleAgent,
    /// `Tab`: swap focus between the repo and worktree panes.
    ToggleFocus,
    /// Enter on the focused row.
    OpenFocused,
    NewWorktree,
    /// Toggle the mark on the given *filtered* worktree row index.
    ToggleMark(usize),
    Delete,
    Rescan,
    ApplyUpdate,
    Quit,
    ModalConfirm,
    ModalCancel,
    ModalIncludeRisky,
}

// ---------------------------------------------------------------------
// Modal
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Clone,
    Create,
}

/// One row of the removal confirm — resolved once when the modal opens, not
/// re-read from `checked` on confirm (which may have been empty the whole
/// time, in the single-focused-row case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteTarget {
    pub path: PathBuf,
    /// `"repo/slug"`, display form.
    pub label: String,
    pub state: Option<WorkState>,
}

impl DeleteTarget {
    /// Removing this would discard unsaved work — or a state that couldn't
    /// be read, which is treated the same way rather than as disposable.
    pub fn risky(&self) -> bool {
        self.state.is_none_or(|s| s.has_unsaved_work())
    }
}

pub enum Modal {
    HookConsent {
        resolved: ResolvedHook,
        kind: HookKind,
    },
    /// The two-step removal confirm. `y` only ever removes targets with
    /// nothing to lose; a risky target (unsaved work) is kept unless the
    /// user first flips `include_risky` (`f`, or the modal's own button) —
    /// one deliberate extra step, spelled out on screen, rather than a
    /// blanket refusal (the old "dirty entries need --force") or a blanket
    /// "y to delete everything".
    ConfirmDelete {
        targets: Vec<DeleteTarget>,
        include_risky: bool,
    },
    /// A failure the user has to read in full — clone/pull output, a
    /// worktree-creation error, an agent that isn't installed. Wrapped and
    /// dismissable, unlike the one-line status, and always also written to
    /// the day's log file (`DashboardModel::fail`).
    Error { title: String, detail: String },
}

// ---------------------------------------------------------------------
// In-flight clone/hook/create/launch pipeline
// ---------------------------------------------------------------------

/// The repo/slug/agent identity a `PendingLaunch` pipeline is carrying
/// end-to-end. `owner`/`name` are only meaningful when the pipeline starts
/// at `Stage::Cloning` (needed for `sync::clone_or_pull_ex`) — empty
/// otherwise.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub repo_label: String,
    pub owner: String,
    pub name: String,
    pub slug: String,
    pub agent: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Cloning,
    CloneHook,
    CreatingWorktree,
    CreateHook,
    Launching,
}

pub struct PendingLaunch {
    pub ctx: LaunchContext,
    pub repo_root: PathBuf,
    pub worktree_path: Option<PathBuf>,
    pub stage: Stage,
    /// Set only once a fresh worktree was actually just created by this
    /// pipeline (never for a fast-resumed/existing selection) — gates
    /// `post_create_hook`, mirroring `main.rs`'s old `was_existing` check.
    pub freshly_created: bool,
}

/// Owned env-var payload for a hook run — `hooks::HookEnv` borrows, which
/// can't cross the suspend boundary (the model that owns the borrowed data
/// is dropped/moved between `Screen::update` returning and the driver loop
/// actually running the hook).
#[derive(Debug, Clone)]
pub struct HookEnvOwned {
    pub repo: String,
    pub worktree_path: PathBuf,
    pub slug: String,
    pub agent: String,
}

impl HookEnvOwned {
    pub fn as_borrowed(&self) -> HookEnv<'_> {
        HookEnv {
            repo: &self.repo,
            worktree_path: &self.worktree_path,
            slug: &self.slug,
            agent: &self.agent,
        }
    }
}

/// What `Screen::update` asks `dashboard.rs`'s driver loop to do outside the
/// terminal — running a hook (inherited stdio) or launching the agent CLI
/// (a real interactive terminal). Both tear the alt-screen down for the
/// duration; `tui::run`'s `(S, S::Outcome)` return shape is what lets the
/// driver hand the same model straight back into a fresh `run()` call once
/// the subprocess finishes.
pub enum SuspendReq {
    RunHook {
        resolved: ResolvedHook,
        kind: HookKind,
        env: HookEnvOwned,
    },
    LaunchAgent {
        agent: AgentConfig,
        worktree_path: PathBuf,
    },
    /// `u`: apply the pending self-update and end the session — see
    /// `dashboard.rs::run_suspend_chain`'s `ApplyUpdate` arm.
    ApplyUpdate,
}

pub enum DashboardOutcome {
    Cancelled,
    Suspend(SuspendReq),
}

enum HookCheckpoint {
    Skip,
    NeedsModal(ResolvedHook),
    Run(ResolvedHook),
}

// ---------------------------------------------------------------------
// DashboardModel
// ---------------------------------------------------------------------

pub struct DashboardModel {
    pub scope: Scope,
    pub focus: Focus,
    /// The repo pane's own state — `Some` only in `Scope::Browse`, `None` in
    /// `AllWorktrees`/`SingleRepo`, which have no repo pane at all.
    pub repos: Option<ListState<RepoRow>>,
    pub worktrees: ListState<WorktreeRow>,
    pub all_entries: Vec<ScannedEntry>,
    /// Per-worktree work state from the last background scan (`None` = the
    /// read failed). The single source of truth for both the pane's STATUS
    /// column and the removal confirm's risky/safe split.
    pub work_cache: HashMap<PathBuf, Option<WorkState>>,
    pub agents: Vec<AgentEntry>,
    pub agent_index: usize,
    /// Worktree paths marked for removal — keyed by path, not list index:
    /// an index is invalidated by every `refresh_worktree_pane` rebuild, a
    /// path is stable across one (and across a rescan — see
    /// `rescan_requested` below).
    pub checked: HashSet<PathBuf>,
    pub modal: Option<Modal>,
    pub pending: Option<PendingLaunch>,
    /// One-line informational message (rendered in the worktree pane's
    /// bottom border; `Esc` clears it). Failures go to `Modal::Error`
    /// instead — see `fail`.
    pub status: Option<String>,
    /// The pending version string from a completed background self-update
    /// check (`Msg::UpdateChecked`), or `None` when unchecked/up to date.
    /// Drives the "update available" segment (`view.rs`) and gates the `u`
    /// key (`tui/update.rs`).
    pub update_available: Option<String>,
    pub root: PathBuf,
    pub idle_threshold_days: u64,
    pub auto_yes: bool,
    pub loading: bool,
    /// Set by the `r` key (`tui/update.rs`); consumed by `dashboard.rs`'s
    /// `DashboardScreen`, which spawns a fresh worktree-scan thread when it
    /// sees this true and no scan is already in flight, then clears it.
    /// `update_dashboard` itself never touches the filesystem — this flag is
    /// the request, not the scan.
    pub rescan_requested: bool,
    /// Every clickable region the last frame drew — see `Action`. Cleared
    /// and refilled by `view::draw_dashboard` on every frame (interior
    /// mutability for the same reason as `ListState::table_rect`).
    pub hotspots: RefCell<Vec<(Rect, Action)>>,
    /// The previous left click's (pane, filtered row, time) — a second
    /// click on the same row inside `DOUBLE_CLICK` is an open, same as
    /// Enter.
    pub last_click: Option<(Focus, usize, Instant)>,
    /// `Msg::Tick` counter — drives the in-flight pipeline's spinner.
    pub ticks: u64,

    // `config` is a snapshot (not a reference — `DashboardModel` outlives
    // any one `Config` borrow across suspend/resume) supplying hook paths/
    // symlink_dirs/the agents map; `force_delete` is `cw clean --force`;
    // `forced_slug` is an explicit SLUG given on the command line that
    // bypasses the worktree-choice step entirely once a repo is committed
    // to; `hook_consent`/`hook_consent_path` back the in-TUI consent modal
    // the same way `main.rs`'s fast path uses `hooks::load_consent`/
    // `save_consent`.
    config: Config,
    force_delete: bool,
    forced_slug: Option<String>,
    // `pub(crate)`, not private: `tui/update.rs`'s tests assert directly
    // against a recorded decline/accept to confirm the confirm-once-per-repo
    // gate, without needing a getter method for one field.
    pub(crate) hook_consent: HookConsent,
    hook_consent_path: PathBuf,
}

/// Two clicks on the same row within this window open it.
pub const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

impl DashboardModel {
    #[allow(clippy::too_many_arguments)]
    fn base(
        scope: Scope,
        focus: Focus,
        repos: Option<ListState<RepoRow>>,
        root: PathBuf,
        config: Config,
        hook_consent: HookConsent,
        hook_consent_path: PathBuf,
        auto_yes: bool,
        force_delete: bool,
        forced_slug: Option<String>,
    ) -> Self {
        let agents = build_agent_entries(&config);
        let agent_index = agents
            .iter()
            .position(|a| a.name == config.default_agent)
            .unwrap_or(0);
        let idle_threshold_days = config.idle_threshold_days;
        Self {
            scope,
            focus,
            repos,
            worktrees: ListState::new(Vec::new()),
            all_entries: Vec::new(),
            work_cache: HashMap::new(),
            agents,
            agent_index,
            checked: HashSet::new(),
            modal: None,
            pending: None,
            status: None,
            update_available: None,
            root,
            idle_threshold_days,
            auto_yes,
            loading: true,
            rescan_requested: false,
            hotspots: RefCell::new(Vec::new()),
            last_click: None,
            ticks: 0,
            config,
            force_delete,
            forced_slug,
            hook_consent,
            hook_consent_path,
        }
    }

    /// Bare `cw`. Opens focused on the worktree pane, not the repo pane —
    /// you land on everything you're working on; `Tab` reaches the repo
    /// pane only when you want to create a new worktree under a specific
    /// repo.
    #[allow(clippy::too_many_arguments)]
    pub fn new_browse(
        initial_repos: Vec<Repo>,
        root: PathBuf,
        config: Config,
        hook_consent: HookConsent,
        hook_consent_path: PathBuf,
        auto_yes: bool,
        forced_slug: Option<String>,
    ) -> Self {
        let repos = ListState::new(build_repo_rows(initial_repos, &root));
        let mut model = Self::base(
            Scope::Browse,
            Focus::Worktrees,
            Some(repos),
            root,
            config,
            hook_consent,
            hook_consent_path,
            auto_yes,
            false,
            forced_slug,
        );
        model.refresh_worktree_pane();
        model
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_all_worktrees(
        root: PathBuf,
        config: Config,
        hook_consent: HookConsent,
        hook_consent_path: PathBuf,
        auto_yes: bool,
        force_delete: bool,
    ) -> Self {
        let mut model = Self::base(
            Scope::AllWorktrees,
            Focus::Worktrees,
            None,
            root,
            config,
            hook_consent,
            hook_consent_path,
            auto_yes,
            force_delete,
            None,
        );
        model.refresh_worktree_pane();
        model
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_single_repo(
        repo_label: String,
        repo_root: PathBuf,
        root: PathBuf,
        config: Config,
        hook_consent: HookConsent,
        hook_consent_path: PathBuf,
        auto_yes: bool,
        forced_slug: Option<String>,
    ) -> Self {
        let mut model = Self::base(
            Scope::SingleRepo {
                repo_label,
                repo_root,
            },
            Focus::Worktrees,
            None,
            root,
            config,
            hook_consent,
            hook_consent_path,
            auto_yes,
            false,
            forced_slug,
        );
        model.refresh_worktree_pane();
        model
    }

    // -- background-load application -----------------------------------

    pub fn apply_repo_load(&mut self, load: RepoLoad) {
        self.loading = false;
        let Some(repos) = self.repos.as_mut() else {
            return;
        };
        let query = std::mem::take(&mut repos.query);
        *repos = ListState::new(build_repo_rows(load.repos, &self.root));
        repos.query = query;
        repos.refilter(|r| r.filter_text.as_str());

        let mut messages = Vec::new();
        if let Some(w) = load.stale_warning {
            messages.push(w);
        }
        if !load.warnings.is_empty() {
            messages.push(load.warnings.join("; "));
        }
        self.status = (!messages.is_empty()).then(|| messages.join(" | "));
        self.refresh_worktree_pane();
    }

    pub fn apply_worktrees_load(&mut self, result: Result<WorktreesLoad, String>) {
        match result {
            Ok(load) => {
                self.all_entries = load.entries;
                self.work_cache = load.work;
            }
            Err(e) => {
                self.status = Some(format!("worktree scan failed: {e}"));
            }
        }
        self.refresh_worktree_pane();
    }

    pub fn apply_work_refresh(&mut self, path: PathBuf, result: Result<WorkState, String>) {
        self.work_cache.insert(path, result.ok());
        self.refresh_worktree_pane();
    }

    pub fn apply_update_checked(&mut self, pending: Option<String>) {
        self.update_available = pending;
    }

    /// Pure in-memory filter over `all_entries`/`work_cache` — no I/O. The
    /// pane shows every repo's worktrees regardless of the repo pane's
    /// cursor (see `pane_repo_filter`) — call sites are purely data-driven:
    /// after a worktree-scan load, a work-state refresh, a delete
    /// confirmed, an agent launch resumed, a repo-list reload, or an `r`
    /// rescan. The rebuild is selection-stable across all of these, same
    /// reasoning as `checked`: the focused row's path (if any) is looked up
    /// again in the new rows and reselected, so a rescan or a background
    /// refresh never bounces the cursor back to row zero.
    pub fn refresh_worktree_pane(&mut self) {
        let selected_path = self
            .worktrees
            .selected()
            .and_then(|row| match &row.selection {
                WorktreeSelection::Existing(entry) => Some(entry.path.clone()),
                WorktreeSelection::New => None,
            });

        let entries: Vec<ScannedEntry> = match self.pane_repo_filter() {
            PaneRepoFilter::All => self.all_entries.clone(),
            PaneRepoFilter::One(label) => self
                .all_entries
                .iter()
                .filter(|e| e.repo == label)
                .cloned()
                .collect(),
        };
        let new_under = self.new_worktree_repo();

        let query = std::mem::take(&mut self.worktrees.query);
        let rows = build_worktree_rows(
            entries.into_iter(),
            &self.work_cache,
            self.idle_threshold_days,
            new_under.as_deref(),
        );
        self.worktrees = ListState::new(rows);
        self.worktrees.query = query;
        self.worktrees.refilter(|r| r.filter_text.as_str());

        if let Some(path) = selected_path {
            let restored = self.worktrees.filtered.iter().position(|&idx| {
                matches!(
                    &self.worktrees.items[idx].selection,
                    WorktreeSelection::Existing(entry) if entry.path == path
                )
            });
            if restored.is_some() {
                self.worktrees.select(restored);
            }
        }
    }

    /// Which worktrees the pane displays: every repo's, except in
    /// `Scope::SingleRepo`, which is pinned to just its own repo.
    fn pane_repo_filter(&self) -> PaneRepoFilter {
        match &self.scope {
            Scope::Browse | Scope::AllWorktrees => PaneRepoFilter::All,
            Scope::SingleRepo { repo_label, .. } => PaneRepoFilter::One(repo_label.clone()),
        }
    }

    /// Which repo a new worktree would be created under, if any. `None`
    /// when there's no concrete repo to create one under:
    /// `Scope::AllWorktrees` (`cw resume`/`cw clean`, which never create),
    /// or `Scope::Browse` before anything is selected in the repo pane yet.
    pub fn new_worktree_repo(&self) -> Option<String> {
        match &self.scope {
            Scope::Browse => self
                .repos
                .as_ref()
                .and_then(|repos| repos.selected())
                .map(|row| row.repo.full_name()),
            Scope::AllWorktrees => None,
            Scope::SingleRepo { repo_label, .. } => Some(repo_label.clone()),
        }
    }

    /// The repo pane's cursor moved (key or click): the synthetic "+ new
    /// worktree" row targets the newly selected repo, so its label has to be
    /// rebuilt — a pure in-memory rebuild, no I/O.
    pub fn repo_cursor_moved(&mut self) {
        if self.repos.is_some() {
            self.refresh_worktree_pane();
        }
    }

    // -- agent footer -----------------------------------------------------

    pub fn cycle_agent(&mut self, delta: isize) {
        let len = self.agents.len() as isize;
        if len > 0 {
            self.agent_index = (self.agent_index as isize + delta).rem_euclid(len) as usize;
        }
    }

    pub fn select_agent(&mut self, idx: usize) {
        if idx < self.agents.len() {
            self.agent_index = idx;
        }
    }

    fn current_agent_name(&self) -> Option<String> {
        self.agents.get(self.agent_index).map(|a| a.name.clone())
    }

    // -- failures ----------------------------------------------------------

    /// Every pipeline failure lands here: the in-flight pipeline is
    /// abandoned, the full detail opens in `Modal::Error` (wrapped, not
    /// truncated to one status line), and the same text is written to the
    /// day's log file — `tracing`'s stderr half is gated off while the TUI
    /// owns the screen, so this never corrupts the frame.
    pub fn fail(&mut self, title: &str, detail: String) {
        tracing::warn!("{title}: {}", detail.replace('\n', " | "));
        self.pending = None;
        self.modal = Some(Modal::Error {
            title: title.to_string(),
            detail,
        });
    }

    /// Driver-triggered: `agent::launch` returned an error (the binary
    /// isn't on `PATH`, most likely). Reported like any other pipeline
    /// failure instead of tearing the whole dashboard down.
    pub fn report_launch_failure(&mut self, agent: &str, detail: String) {
        self.fail(&format!("could not launch {agent}"), detail);
    }

    // -- mark-and-delete flow (every scope alike) -------------------------

    /// `Space` on a focused worktree row: always toggles its check-mark,
    /// never falls through to the filter query (unlike every other typable
    /// character). A no-op on the synthetic "+ new worktree" row — nothing
    /// on disk yet to mark for removal.
    pub fn toggle_checked_focused(&mut self) {
        if let Some(idx) = self.worktrees.selected_index() {
            self.toggle_checked_row(idx);
        }
    }

    /// Toggles the mark on a *filtered* row index (a click on the mark
    /// column, or `Space` via `toggle_checked_focused`).
    pub fn toggle_checked_row(&mut self, filtered_idx: usize) {
        let Some(&item_idx) = self.worktrees.filtered.get(filtered_idx) else {
            return;
        };
        let Some(WorktreeSelection::Existing(entry)) =
            self.worktrees.items.get(item_idx).map(|r| &r.selection)
        else {
            return;
        };
        let path = entry.path.clone();
        if !self.checked.remove(&path) {
            self.checked.insert(path);
        }
    }

    /// `d` outside a confirm modal: opens `Modal::ConfirmDelete` targeting
    /// the checked set when it's non-empty, or just the focused row when
    /// nothing is checked (dropping single-worktree delete to two
    /// keystrokes — `d`, `y`). A no-op when nothing is checked AND nothing
    /// removable is focused (an empty pane, or the synthetic "+ new
    /// worktree" row). `include_risky` starts true only under `cw clean
    /// --force`; every other entry point starts on the safe side.
    pub fn open_delete_confirm(&mut self) {
        let paths: Vec<PathBuf> = if self.checked.is_empty() {
            match self.worktrees.selected().map(|r| &r.selection) {
                Some(WorktreeSelection::Existing(entry)) => vec![entry.path.clone()],
                _ => return,
            }
        } else {
            self.checked.iter().cloned().collect()
        };

        // Reads `work_cache` directly — the same canonical source
        // `confirm_delete` checks against — rather than re-deriving state
        // from the rendered `self.worktrees.items`, which would be a second,
        // divergence-prone copy of the same fact.
        let mut targets: Vec<DeleteTarget> = paths
            .into_iter()
            .filter_map(|path| {
                let entry = self.all_entries.iter().find(|e| e.path == path)?;
                Some(DeleteTarget {
                    label: format!(
                        "{}/{}",
                        worktree::display_repo_label(&entry.repo),
                        entry.slug
                    ),
                    state: self.work_cache.get(&path).copied().flatten(),
                    path,
                })
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        targets.sort_by(|a, b| a.label.cmp(&b.label));
        self.modal = Some(Modal::ConfirmDelete {
            targets,
            include_risky: self.force_delete,
        });
    }

    /// `f` on `Modal::ConfirmDelete`: flips whether targets with unsaved
    /// work are removed too. A no-op when no target is risky — there's
    /// nothing to include.
    pub fn toggle_include_risky(&mut self) {
        if let Some(Modal::ConfirmDelete {
            targets,
            include_risky,
        }) = self.modal.as_mut()
        {
            if targets.iter().any(DeleteTarget::risky) {
                *include_risky = !*include_risky;
            }
        }
    }

    /// Confirms `Modal::ConfirmDelete`: actually removes each targeted
    /// entry via `clean::remove_one` (pure `git2`/`fs`, no subprocess — runs
    /// inline, no suspend needed). Risky targets are kept unless
    /// `include_risky`. Always clears `checked` — even a target that came
    /// from the single-focused-row case (never added to `checked` in the
    /// first place) leaves it empty either way.
    pub fn confirm_delete(&mut self) {
        let Some(Modal::ConfirmDelete {
            targets,
            include_risky,
        }) = self.modal.take()
        else {
            return;
        };
        self.checked.clear();

        let mut removed = 0usize;
        let mut kept = 0usize;
        let mut failures = Vec::new();
        for target in targets {
            let Some(entry) = self
                .all_entries
                .iter()
                .find(|e| e.path == target.path)
                .cloned()
            else {
                continue; // already gone — stale target, nothing to do
            };
            if target.risky() && !include_risky {
                kept += 1;
                continue;
            }
            match crate::clean::remove_one(&self.root, &entry) {
                Ok(()) => {
                    self.all_entries.retain(|e| e != &entry);
                    self.work_cache.remove(&entry.path);
                    removed += 1;
                }
                Err(err) => failures.push(format!("{}: {err:#}", target.label)),
            }
        }

        let mut parts = Vec::new();
        if removed > 0 {
            parts.push(format!("removed {removed} worktree{}", plural(removed)));
        }
        if kept > 0 {
            parts.push(format!(
                "kept {kept} with unsaved work (press d, then f to include)"
            ));
        }
        self.status = (!parts.is_empty()).then(|| parts.join(" · "));
        if !failures.is_empty() {
            self.fail("removal failed", failures.join("\n"));
        }
        self.refresh_worktree_pane();
    }

    // -- launch pipeline ----------------------------------------------------

    /// `n`, Enter on a repo row, or the "+ new worktree" row: starts a fresh
    /// worktree under `new_worktree_repo()`, or explains why it can't.
    pub fn new_worktree(&mut self) -> Option<DashboardOutcome> {
        if self.new_worktree_repo().is_none() {
            self.status = Some(match self.scope {
                Scope::Browse => "pick a repo first (tab, or click one)".to_string(),
                _ => "run bare `cw` to create a worktree under a repo".to_string(),
            });
            return None;
        }
        self.start_pending(WorktreeSelection::New)
    }

    /// Enter on a worktree-pane row: starts the clone/hook/create/launch
    /// pipeline for that selection. A no-op while a pipeline is already in
    /// flight (`self.pending` already `Some`) — most importantly
    /// `Stage::Cloning`, the one stage that leaves the terminal event loop
    /// live while a background thread runs. Without this guard, an Enter on
    /// a different row during that window would replace `self.pending`
    /// outright, and the original background clone's result would later
    /// land via `apply_clone_done` and get applied to the new (unrelated)
    /// pending instead.
    ///
    /// An existing worktree always resumes under its *own* repo (read off
    /// the scanned entry) and launches directly — never a clone/pull, which
    /// only touches the main checkout and can't affect the worktree's own
    /// branch anyway. A new worktree in `Scope::Browse` is created under
    /// the repo pane's selected repo, after a clone/pull of that repo.
    pub fn start_pending(&mut self, selection: WorktreeSelection) -> Option<DashboardOutcome> {
        if self.pending.is_some() {
            return None;
        }
        let agent = self.current_agent_name()?;
        if let WorktreeSelection::Existing(entry) = selection {
            let (owner, name) = entry
                .repo
                .split_once('/')
                .map(|(o, n)| (o.to_string(), n.to_string()))
                .unwrap_or_default();
            self.pending = Some(PendingLaunch {
                ctx: LaunchContext {
                    repo_label: entry.repo.clone(),
                    owner,
                    name,
                    slug: worktree::unflatten_slug(&entry.slug),
                    agent,
                },
                repo_root: self.root.join(&entry.repo),
                worktree_path: Some(entry.path),
                stage: Stage::Launching,
                freshly_created: false,
            });
            return self.advance_pending();
        }

        match &self.scope {
            Scope::Browse => {
                let repo = self.repos.as_ref()?.selected()?.repo.clone();
                let repo_label = repo.full_name();
                let repo_root = sync::resolve_local_path(&self.root, &repo.owner, &repo.name);
                let slug = self
                    .forced_slug
                    .take()
                    .unwrap_or_else(worktree::generate_timestamp_slug);
                self.pending = Some(PendingLaunch {
                    ctx: LaunchContext {
                        repo_label,
                        owner: repo.owner,
                        name: repo.name,
                        slug,
                        agent,
                    },
                    repo_root,
                    worktree_path: None,
                    // A fresh worktree branches from HEAD, so the main
                    // checkout is pulled first (matches the old
                    // `run_default`'s unconditional `clone_or_pull`).
                    stage: Stage::Cloning,
                    freshly_created: false,
                });
            }
            Scope::AllWorktrees => return None,
            Scope::SingleRepo {
                repo_label,
                repo_root,
            } => {
                let repo_label = repo_label.clone();
                let repo_root = repo_root.clone();
                let slug = self
                    .forced_slug
                    .take()
                    .unwrap_or_else(worktree::generate_timestamp_slug);
                self.pending = Some(PendingLaunch {
                    ctx: LaunchContext {
                        repo_label,
                        owner: String::new(),
                        name: String::new(),
                        slug,
                        agent,
                    },
                    repo_root,
                    worktree_path: None,
                    stage: Stage::CreatingWorktree,
                    freshly_created: false,
                });
            }
        }
        self.advance_pending()
    }

    /// Background clone/pull thread result — advances the pipeline past
    /// `Stage::Cloning`. A pull of an already-cloned repo (not a fresh
    /// clone) skips the clone-hook checkpoint entirely, mirroring the old
    /// `run_default`'s `if matches!(pull_outcome, PullOutcome::Cloned)`
    /// guard.
    pub fn apply_clone_done(
        &mut self,
        result: Result<CloneOutcome, String>,
    ) -> Option<DashboardOutcome> {
        match result {
            Ok(outcome) => {
                self.status = match outcome.pull_outcome {
                    PullOutcome::Cloned => Some(format!("cloned {}", outcome.repo_label)),
                    PullOutcome::FastForwarded => {
                        Some(format!("pulled latest changes for {}", outcome.repo_label))
                    }
                    PullOutcome::Diverged => Some(format!(
                        "{}'s local branch has diverged from origin — left untouched",
                        outcome.repo_label
                    )),
                    PullOutcome::DirtyLocalChanges => Some(format!(
                        "{} has uncommitted local changes — skipped pull to avoid discarding them",
                        outcome.repo_label
                    )),
                    PullOutcome::UpToDate | PullOutcome::Skipped => None,
                };
                if let Some(p) = self.pending.as_mut() {
                    p.stage = if outcome.pull_outcome == PullOutcome::Cloned {
                        Stage::CloneHook
                    } else if p.worktree_path.is_some() {
                        Stage::Launching
                    } else {
                        Stage::CreatingWorktree
                    };
                }
                self.advance_pending()
            }
            Err(e) => {
                let label = self
                    .pending
                    .as_ref()
                    .map(|p| p.ctx.repo_label.clone())
                    .unwrap_or_default();
                self.fail(&format!("clone/pull of {label} failed"), e);
                None
            }
        }
    }

    /// `y`/`n` on `Modal::HookConsent`: records consent for this repo
    /// (once — a later hook checkpoint for the same repo never re-prompts,
    /// same confirm-once-per-repo discipline as `hooks::gate`), then either
    /// suspends to run the hook or skips straight past it.
    pub fn resolve_hook_consent(&mut self, accepted: bool) -> Option<DashboardOutcome> {
        let Some(Modal::HookConsent { resolved, kind }) = self.modal.take() else {
            return None;
        };
        let repo_label = self.pending.as_ref()?.ctx.repo_label.clone();
        self.hook_consent.insert(repo_label, accepted);
        // In-memory consent (just above) still governs this session even if
        // the disk write fails, so this is non-fatal — but silently
        // discarding the error means a permissions/disk problem would
        // re-prompt every future session with zero visibility into why.
        if let Err(e) = hooks::save_consent(&self.hook_consent_path, &self.hook_consent) {
            self.status = Some(format!(
                "failed to save hook consent to disk: {e:#} (will re-prompt next run)"
            ));
        }

        if !accepted {
            if let Some(p) = self.pending.as_mut() {
                p.stage = Self::stage_after_hook(kind);
            }
            return self.advance_pending();
        }
        let env = self.hook_env(kind)?;
        Some(DashboardOutcome::Suspend(SuspendReq::RunHook {
            resolved,
            kind,
            env,
        }))
    }

    /// Driver-triggered: a suspended hook run finished. Direct method call
    /// from `dashboard.rs`'s loop (not routed through a `Msg`).
    pub fn resume_after_hook(
        &mut self,
        kind: HookKind,
        result: Result<(), String>,
    ) -> Option<DashboardOutcome> {
        if let Err(e) = result {
            self.status = Some(format!("hook failed: {e}"));
        }
        if let Some(p) = self.pending.as_mut() {
            p.stage = Self::stage_after_hook(kind);
        }
        self.advance_pending()
    }

    /// Driver-triggered: the suspended agent launch finished. Clears
    /// `pending`/`modal`/`status` — `work_cache` for the launched worktree
    /// is refreshed separately via `Msg::WorkRefreshed`, once the driver
    /// has a fresh `gitstatus::work_state` read.
    pub fn resume_after_launch(&mut self) {
        self.pending = None;
        self.modal = None;
        self.status = None;
        self.refresh_worktree_pane();
    }

    fn stage_after_hook(kind: HookKind) -> Stage {
        match kind {
            HookKind::Clone => Stage::CreatingWorktree,
            HookKind::Create => Stage::Launching,
        }
    }

    /// Advances `pending.stage` as far as it can go without external input:
    /// loops through stages that resolve themselves purely (a hook that's
    /// unconfigured, or already consented) and stops the moment something
    /// needs the background clone thread (`Stage::Cloning` — the driver
    /// spawns it, see `dashboard.rs`), a human decision (`Modal::
    /// HookConsent`), or a suspend (`DashboardOutcome::Suspend`).
    fn advance_pending(&mut self) -> Option<DashboardOutcome> {
        loop {
            let stage = self.pending.as_ref()?.stage;
            match stage {
                Stage::Cloning => return None,
                Stage::CloneHook => match self.checkpoint(HookKind::Clone) {
                    HookCheckpoint::Skip => {
                        self.pending.as_mut()?.stage = Stage::CreatingWorktree;
                    }
                    HookCheckpoint::NeedsModal(resolved) => {
                        self.modal = Some(Modal::HookConsent {
                            resolved,
                            kind: HookKind::Clone,
                        });
                        return None;
                    }
                    HookCheckpoint::Run(resolved) => {
                        let env = self.hook_env(HookKind::Clone)?;
                        return Some(DashboardOutcome::Suspend(SuspendReq::RunHook {
                            resolved,
                            kind: HookKind::Clone,
                            env,
                        }));
                    }
                },
                Stage::CreatingWorktree => match self.do_create_worktree() {
                    Ok(()) => {
                        self.pending.as_mut()?.stage = Stage::CreateHook;
                    }
                    Err(e) => {
                        self.fail("worktree creation failed", format!("{e:#}"));
                        return None;
                    }
                },
                Stage::CreateHook => {
                    if !self.pending.as_ref()?.freshly_created {
                        self.pending.as_mut()?.stage = Stage::Launching;
                        continue;
                    }
                    match self.checkpoint(HookKind::Create) {
                        HookCheckpoint::Skip => {
                            self.pending.as_mut()?.stage = Stage::Launching;
                        }
                        HookCheckpoint::NeedsModal(resolved) => {
                            self.modal = Some(Modal::HookConsent {
                                resolved,
                                kind: HookKind::Create,
                            });
                            return None;
                        }
                        HookCheckpoint::Run(resolved) => {
                            let env = self.hook_env(HookKind::Create)?;
                            return Some(DashboardOutcome::Suspend(SuspendReq::RunHook {
                                resolved,
                                kind: HookKind::Create,
                                env,
                            }));
                        }
                    }
                }
                Stage::Launching => {
                    let pending = self.pending.as_ref()?;
                    let agent_name = pending.ctx.agent.clone();
                    let agent_cfg = match config::resolve_agent(Some(&agent_name), &self.config) {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            self.report_launch_failure(&agent_name, format!("{e:#}"));
                            return None;
                        }
                    };
                    let worktree_path = pending.worktree_path.clone()?;
                    return Some(DashboardOutcome::Suspend(SuspendReq::LaunchAgent {
                        agent: agent_cfg,
                        worktree_path,
                    }));
                }
            }
        }
    }

    fn checkpoint(&self, kind: HookKind) -> HookCheckpoint {
        let Some(pending) = self.pending.as_ref() else {
            return HookCheckpoint::Skip;
        };
        let resolved = match kind {
            HookKind::Clone => hooks::resolve_post_clone_hook(
                &pending.repo_root,
                self.config.post_clone_hook.as_deref(),
            ),
            HookKind::Create => pending.worktree_path.as_ref().and_then(|wt| {
                hooks::resolve_post_create_hook(
                    &pending.repo_root,
                    wt,
                    self.config.post_create_hook.as_deref(),
                )
            }),
        };
        let Some(resolved) = resolved else {
            return HookCheckpoint::Skip;
        };
        if self.auto_yes {
            return HookCheckpoint::Run(resolved);
        }
        match self.hook_consent.get(&pending.ctx.repo_label) {
            Some(true) => HookCheckpoint::Run(resolved),
            Some(false) => HookCheckpoint::Skip,
            None => HookCheckpoint::NeedsModal(resolved),
        }
    }

    /// Inline `git2`/`fs` worktree creation — no subprocess, so it runs
    /// synchronously here rather than through the suspend/resume driver.
    /// Symlinks + `.worktreeinclude` best-effort, mirroring `main.rs`'s
    /// `finish_worktree_creation`; skipped entirely on a fast-resume
    /// (`freshly_created` stays `false`), matching `create_or_resume_
    /// worktree`'s own contract.
    fn do_create_worktree(&mut self) -> Result<()> {
        let pending = self
            .pending
            .as_mut()
            .context("CreatingWorktree stage requires pending")?;
        let (_, was_existing) =
            worktree::worktree_path_and_exists(&pending.repo_root, &pending.ctx.slug);
        let repo = git2::Repository::open(&pending.repo_root)
            .with_context(|| format!("opening {}", pending.repo_root.display()))?;
        let path = worktree::create_or_resume_worktree(&repo, &pending.ctx.slug, "HEAD")?;
        pending.worktree_path = Some(path.clone());
        pending.freshly_created = !was_existing;

        if !was_existing {
            // `?`, not `let _ =`/`if let Ok(..)` — matches main.rs's
            // `finish_worktree_creation` fast path exactly: a hard failure in
            // either call surfaces via `advance_pending`'s `Err(e)` branch
            // instead of being silently discarded. Per-file copy failures
            // inside a successful `apply_worktreeinclude` call remain
            // non-fatal, logged as warnings — that's the deliberate
            // continue-on-error behavior `worktreeinclude.rs` documents.
            worktree::symlink_shared_dirs(&pending.repo_root, &path, &self.config.symlink_dirs)?;
            let failures = worktreeinclude::apply_worktreeinclude(&pending.repo_root, &path)?;
            for f in &failures {
                tracing::warn!(
                    file = %f.path.display(),
                    error = %f.error,
                    "worktreeinclude: failed to copy file, continuing"
                );
            }

            // Without this, a worktree created this session never shows up
            // in the pane until `cw` is quit and relaunched. `entry.slug`
            // must be the flattened on-disk form — the same shape
            // `scan_worktrees` reads back off disk — not the raw (possibly
            // `/`-containing) slug the pipeline carries.
            let entry = ScannedEntry {
                repo: pending.ctx.repo_label.clone(),
                slug: worktree::flatten_slug(&pending.ctx.slug),
                path: path.clone(),
                mtime: SystemTime::now(),
            };
            self.work_cache
                .insert(path.clone(), Some(WorkState::default()));
            self.all_entries.push(entry);
            self.all_entries.sort_by_key(|e| std::cmp::Reverse(e.mtime));
            self.refresh_worktree_pane();
        }
        Ok(())
    }

    fn hook_env(&self, kind: HookKind) -> Option<HookEnvOwned> {
        let pending = self.pending.as_ref()?;
        let worktree_path = match kind {
            HookKind::Clone => pending.worktree_path.clone().unwrap_or_else(|| {
                worktree::worktree_path_and_exists(&pending.repo_root, &pending.ctx.slug).0
            }),
            HookKind::Create => pending.worktree_path.clone()?,
        };
        Some(HookEnvOwned {
            repo: pending.ctx.repo_label.clone(),
            worktree_path,
            slug: pending.ctx.slug.clone(),
            agent: pending.ctx.agent.clone(),
        })
    }
}

pub fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn age_label_idle_and_relative() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(20 * 86_400);
        let recent = now - Duration::from_secs(2 * 3_600);

        assert_eq!(age_label(old, now, 14), ("idle 20d".to_string(), true));
        assert_eq!(age_label(recent, now, 14), ("2h ago".to_string(), false));
        assert_eq!(age_label(old, now, 20), ("idle 20d".to_string(), true));
        assert_eq!(age_label(old, now, 21), ("20d ago".to_string(), false));
    }

    #[test]
    fn relative_time_buckets() {
        let now = Utc::now();
        let ten_min_ago = (now - chrono::Duration::minutes(10)).to_rfc3339();
        let two_hours_ago = (now - chrono::Duration::hours(2)).to_rfc3339();
        let three_days_ago = (now - chrono::Duration::days(3)).to_rfc3339();

        assert_eq!(relative_time(&ten_min_ago, now), "10m ago");
        assert_eq!(relative_time(&two_hours_ago, now), "2h ago");
        assert_eq!(relative_time(&three_days_ago, now), "3d ago");
        assert_eq!(relative_time("not-a-timestamp", now), "not-a-timestamp");
    }

    #[test]
    fn recently_updated_boundary() {
        let now = Utc::now();
        let recent = (now - chrono::Duration::hours(1)).to_rfc3339();
        let old = (now - chrono::Duration::hours(25)).to_rfc3339();
        assert!(is_recently_updated(&recent, now));
        assert!(!is_recently_updated(&old, now));
    }

    #[test]
    fn delete_target_unknown_state_is_risky() {
        let t = DeleteTarget {
            path: PathBuf::from("/x"),
            label: "a/b/c".into(),
            state: None,
        };
        assert!(t.risky());
        let clean = DeleteTarget {
            state: Some(WorkState::default()),
            ..t.clone()
        };
        assert!(!clean.risky());
        let unpushed = DeleteTarget {
            state: Some(WorkState {
                changed_files: 0,
                unpushed_commits: 2,
            }),
            ..t
        };
        assert!(unpushed.risky());
    }
}
