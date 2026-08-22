//! `DashboardModel`: the single composite Model backing `cw`'s persistent
//! split-pane dashboard (`dashboard.rs`). Replaces the old
//! `RepoModel`/`WorktreeModel`/`AgentModel` triple — one screen, one model,
//! for the whole session, surviving suspend/resume round trips out to a
//! hook or an agent launch.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use super::msg::{CloneOutcome, RepoLoad, WorktreesLoad};
use super::widgets::filter_indices;
use crate::config::{self, AgentConfig, Config};
use crate::github::Repo;
use crate::hooks::{self, HookConsent, HookEnv, ResolvedHook};
use crate::sync::{self, PullOutcome};
use crate::worktree::{self, CleanCandidate, WorktreeEntry as ScannedEntry, WorktreeSelection};
use crate::worktreeinclude;

/// Idle-duration label (`"idle Nd"`), or `None` under `threshold_days` —
/// baked into a row's text once at construction, never recomputed on every
/// render. `now` is a parameter (not `SystemTime::now()` inline) so this
/// stays testable without real elapsed time.
pub fn humanize(mtime: SystemTime, now: SystemTime, threshold_days: u64) -> Option<String> {
    let days = now
        .duration_since(mtime)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    (days >= threshold_days).then(|| format!("idle {days}d"))
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
/// `TableState` (selection/scroll), and the table's last-rendered `Rect`.
///
/// `table`/`table_rect` use interior mutability (`RefCell`/`Cell`)
/// deliberately: `Screen::draw` takes `&Model` (never widened to `&mut
/// self` just to let rendering write back state), yet
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
    pub table_rect: Cell<Rect>,
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
/// idle/dirty state at construction. `dirty` is looked up from
/// `DashboardModel::dirty_cache` — never computed here via a live
/// `gitstatus::is_dirty` call, which is exactly the per-keystroke I/O bug
/// this rewrite fixes (see the plan's Context section).
pub struct WorktreeRow {
    pub selection: WorktreeSelection,
    pub dirty: bool,
    pub idle_label: Option<String>,
    pub repo_label: String,
    pub filter_text: String,
}

impl WorktreeRow {
    fn existing(
        entry: ScannedEntry,
        dirty: bool,
        idle_threshold_days: u64,
        now: SystemTime,
    ) -> Self {
        let repo_label = worktree::display_repo_label(&entry.repo);
        let idle_label = humanize(entry.mtime, now, idle_threshold_days);
        let filter_text = format!("{repo_label}/{}", entry.slug);
        Self {
            selection: WorktreeSelection::Existing(entry),
            dirty,
            idle_label,
            repo_label,
            filter_text,
        }
    }

    fn new_row() -> Self {
        Self {
            selection: WorktreeSelection::New,
            dirty: false,
            idle_label: None,
            repo_label: String::new(),
            filter_text: "+ new worktree".to_string(),
        }
    }
}

fn build_worktree_rows(
    entries: impl Iterator<Item = ScannedEntry>,
    dirty_cache: &HashMap<PathBuf, bool>,
    idle_threshold_days: u64,
    include_new: bool,
) -> Vec<WorktreeRow> {
    let now = SystemTime::now();
    let mut rows: Vec<WorktreeRow> = entries
        .map(|e| {
            let dirty = dirty_cache.get(&e.path).copied().unwrap_or(false);
            WorktreeRow::existing(e, dirty, idle_threshold_days, now)
        })
        .collect();
    if include_new {
        rows.push(WorktreeRow::new_row());
    }
    rows
}

/// An agent, annotated with its resolved command-line preview — the
/// footer's segmented control (`Ctrl-A` cycles, not a `ListState` — small
/// and fixed, never filtered).
pub struct AgentEntry {
    pub name: String,
    pub cmd_preview: String,
}

impl AgentEntry {
    fn new(name: String, cfg: &AgentConfig) -> Self {
        let mut cmd_preview = cfg.cmd.clone();
        for arg in &cfg.args {
            cmd_preview.push(' ');
            cmd_preview.push_str(arg);
        }
        Self { name, cmd_preview }
    }
}

fn build_agent_entries(agents: &HashMap<String, AgentConfig>) -> Vec<AgentEntry> {
    // Sorted, not `HashMap` iteration order — deterministic run to run.
    let mut names: Vec<&String> = agents.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| AgentEntry::new(name.clone(), &agents[name]))
        .collect()
}

// ---------------------------------------------------------------------
// Scope / Focus
// ---------------------------------------------------------------------

/// What the dashboard is showing. `Browse` is bare `cw`'s repo-picker-plus-
/// worktree-pane split view; `AllWorktrees` backs both `cw resume` (pick to
/// launch) and `cw clean` (`delete_mode: true`, pick to remove); `SingleRepo`
/// backs `cw scratch`, scoped to the synthetic scratch repo with no repo
/// pane at all.
pub enum Scope {
    Browse {
        repos: ListState<RepoRow>,
    },
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
    NothingSelected,
}

// ---------------------------------------------------------------------
// Modal
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Clone,
    Create,
}

pub enum Modal {
    HookConsent {
        resolved: ResolvedHook,
        kind: HookKind,
    },
    ConfirmDelete {
        total_count: usize,
        dirty_count: usize,
        force: bool,
    },
}

// ---------------------------------------------------------------------
// In-flight clone/hook/create/launch pipeline
// ---------------------------------------------------------------------

/// The repo/slug/agent identity a `PendingLaunch` pipeline is carrying
/// end-to-end. `owner`/`name` are only meaningful in `Scope::Browse` (needed
/// for `sync::clone_or_pull_ex`) — empty in `Scope::AllWorktrees`/
/// `Scope::SingleRepo`, which never clone.
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
    pub worktrees: ListState<WorktreeRow>,
    pub all_entries: Vec<ScannedEntry>,
    pub dirty_cache: HashMap<PathBuf, bool>,
    pub agents: Vec<AgentEntry>,
    pub agent_index: usize,
    pub delete_mode: bool,
    pub checked: HashSet<usize>,
    pub modal: Option<Modal>,
    pub pending: Option<PendingLaunch>,
    pub status: Option<String>,
    pub root: PathBuf,
    pub idle_threshold_days: u64,
    pub auto_yes: bool,
    pub loading: bool,

    // Not in the plan's illustrative struct sketch, but required to make
    // the pipeline above actually work: `config` is a snapshot (not a
    // reference — `DashboardModel` outlives any one `Config` borrow across
    // suspend/resume) supplying hook paths/symlink_dirs/the agents map;
    // `force_delete` is `cw clean --force`; `forced_slug` is an explicit
    // SLUG given on the command line that bypasses the worktree-choice step
    // entirely once a repo is committed to; `hook_consent`/
    // `hook_consent_path` back the in-TUI consent modal the same way
    // `main.rs`'s fast path uses `hooks::load_consent`/`save_consent`.
    config: Config,
    force_delete: bool,
    forced_slug: Option<String>,
    // `pub(crate)`, not private: `tui/update.rs`'s tests assert directly
    // against a recorded decline/accept to confirm the confirm-once-per-repo
    // gate, without needing a getter method for one field.
    pub(crate) hook_consent: HookConsent,
    hook_consent_path: PathBuf,
}

impl DashboardModel {
    #[allow(clippy::too_many_arguments)]
    fn base(
        scope: Scope,
        focus: Focus,
        root: PathBuf,
        config: Config,
        hook_consent: HookConsent,
        hook_consent_path: PathBuf,
        auto_yes: bool,
        delete_mode: bool,
        force_delete: bool,
        forced_slug: Option<String>,
    ) -> Self {
        let agents = build_agent_entries(&config.agents);
        let agent_index = agents
            .iter()
            .position(|a| a.name == config.default_agent)
            .unwrap_or(0);
        let idle_threshold_days = config.idle_threshold_days;
        Self {
            scope,
            focus,
            worktrees: ListState::new(Vec::new()),
            all_entries: Vec::new(),
            dirty_cache: HashMap::new(),
            agents,
            agent_index,
            delete_mode,
            checked: HashSet::new(),
            modal: None,
            pending: None,
            status: None,
            root,
            idle_threshold_days,
            auto_yes,
            loading: true,
            config,
            force_delete,
            forced_slug,
            hook_consent,
            hook_consent_path,
        }
    }

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
            Scope::Browse { repos },
            Focus::Repos,
            root,
            config,
            hook_consent,
            hook_consent_path,
            auto_yes,
            false,
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
        delete_mode: bool,
        force_delete: bool,
    ) -> Self {
        let mut model = Self::base(
            Scope::AllWorktrees,
            Focus::Worktrees,
            root,
            config,
            hook_consent,
            hook_consent_path,
            auto_yes,
            delete_mode,
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
            root,
            config,
            hook_consent,
            hook_consent_path,
            auto_yes,
            false,
            false,
            forced_slug,
        );
        model.refresh_worktree_pane();
        model
    }

    // -- background-load application -----------------------------------

    pub fn apply_repo_load(&mut self, load: RepoLoad) {
        self.loading = false;
        let Scope::Browse { repos } = &mut self.scope else {
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
                self.dirty_cache = load.dirty;
            }
            Err(e) => {
                self.status = Some(format!("worktree scan failed: {e}"));
            }
        }
        self.refresh_worktree_pane();
    }

    pub fn apply_dirty_refresh(&mut self, path: PathBuf, result: Result<bool, String>) {
        self.dirty_cache.insert(path, result.unwrap_or(false));
        self.refresh_worktree_pane();
    }

    /// Pure in-memory filter over `all_entries`/`dirty_cache` — no I/O.
    /// Called on every repo-cursor move in `Scope::Browse` and whenever the
    /// background data backing it changes; never calls `gitstatus::is_dirty`
    /// itself (that per-keystroke storm is exactly what this rewrite fixes).
    pub fn refresh_worktree_pane(&mut self) {
        let filter = self.pane_repo_filter();
        let include_new = !matches!(
            filter,
            PaneRepoFilter::All | PaneRepoFilter::NothingSelected
        );
        let entries: Vec<ScannedEntry> = match &filter {
            PaneRepoFilter::All => self.all_entries.clone(),
            PaneRepoFilter::One(label) => self
                .all_entries
                .iter()
                .filter(|e| &e.repo == label)
                .cloned()
                .collect(),
            PaneRepoFilter::NothingSelected => Vec::new(),
        };

        let query = std::mem::take(&mut self.worktrees.query);
        let rows = build_worktree_rows(
            entries.into_iter(),
            &self.dirty_cache,
            self.idle_threshold_days,
            include_new,
        );
        self.worktrees = ListState::new(rows);
        self.worktrees.query = query;
        self.worktrees.refilter(|r| r.filter_text.as_str());
        self.checked.clear();
    }

    fn pane_repo_filter(&self) -> PaneRepoFilter {
        match &self.scope {
            Scope::Browse { repos } => match repos.selected() {
                Some(row) => PaneRepoFilter::One(row.repo.full_name()),
                None => PaneRepoFilter::NothingSelected,
            },
            Scope::AllWorktrees => PaneRepoFilter::All,
            Scope::SingleRepo { repo_label, .. } => PaneRepoFilter::One(repo_label.clone()),
        }
    }

    // -- agent footer -----------------------------------------------------

    pub fn cycle_agent(&mut self) {
        if !self.agents.is_empty() {
            self.agent_index = (self.agent_index + 1) % self.agents.len();
        }
    }

    fn current_agent_name(&self) -> Option<String> {
        self.agents.get(self.agent_index).map(|a| a.name.clone())
    }

    // -- delete flow (Scope::AllWorktrees, delete_mode) -------------------

    pub fn toggle_checked_focused(&mut self) {
        let Some(filtered_pos) = self.worktrees.selected_index() else {
            return;
        };
        let Some(&item_idx) = self.worktrees.filtered.get(filtered_pos) else {
            return;
        };
        if !self.checked.remove(&item_idx) {
            self.checked.insert(item_idx);
        }
    }

    /// `d` outside a confirm modal: opens `Modal::ConfirmDelete` when
    /// something's checked, does nothing when the checked set is empty (the
    /// dashboard stays open either way — unlike the old picker's `d`, there
    /// is no longer a "cancel the whole screen" outcome for this).
    pub fn open_delete_confirm(&mut self) {
        if self.checked.is_empty() {
            return;
        }
        let mut dirty_count = 0;
        for &idx in &self.checked {
            if self.worktrees.items.get(idx).is_some_and(|r| r.dirty) {
                dirty_count += 1;
            }
        }
        self.modal = Some(Modal::ConfirmDelete {
            total_count: self.checked.len(),
            dirty_count,
            force: self.force_delete,
        });
    }

    /// Confirms `Modal::ConfirmDelete`: actually removes each checked entry
    /// via `clean::remove_one` (pure `git2`/`fs`, no subprocess — runs
    /// inline, no suspend needed, same tolerance as worktree creation).
    /// Dirty entries are skipped unless `--force`, exactly like the old
    /// `clean.rs::run_clean`.
    pub fn confirm_delete(&mut self) {
        self.modal = None;
        let mut idxs: Vec<usize> = self.checked.drain().collect();
        idxs.sort_unstable();

        let mut candidates: Vec<CleanCandidate> = Vec::with_capacity(idxs.len());
        for idx in idxs {
            if let Some(row) = self.worktrees.items.get(idx) {
                if let WorktreeSelection::Existing(entry) = &row.selection {
                    candidates.push(CleanCandidate {
                        entry: entry.clone(),
                        dirty: row.dirty,
                    });
                }
            }
        }

        let mut messages = Vec::new();
        for candidate in candidates {
            let entry = &candidate.entry;
            if candidate.dirty && !self.force_delete {
                messages.push(format!(
                    "skipped {}/{} — has uncommitted changes",
                    entry.repo, entry.slug
                ));
                continue;
            }
            match crate::clean::remove_one(&self.root, entry) {
                Ok(()) => {
                    self.all_entries.retain(|e| e != entry);
                    messages.push(format!("removed {}/{}", entry.repo, entry.slug));
                }
                Err(err) => messages.push(format!(
                    "failed to remove {}/{}: {err:#}",
                    entry.repo, entry.slug
                )),
            }
        }
        self.status = (!messages.is_empty()).then(|| messages.join(" | "));
        self.refresh_worktree_pane();
    }

    // -- launch pipeline ----------------------------------------------------

    /// Enter on a worktree-pane row: starts the clone/hook/create/launch
    /// pipeline for that selection, scoped by `self.scope`'s rules (see
    /// each match arm). A no-op while a pipeline is already in flight
    /// (`self.pending` already `Some`) — most importantly `Stage::Cloning`,
    /// the one stage that leaves the terminal event loop live while a
    /// background thread runs (every other stage either resolves
    /// synchronously inside `advance_pending` or suspends the whole TUI).
    /// Without this guard, an Enter on a different row during that window
    /// would replace `self.pending` outright, and the original background
    /// clone's result would later land via `apply_clone_done` and get
    /// applied to the new (unrelated) pending instead — silently skipping
    /// the new selection's own clone/pull.
    pub fn start_pending(&mut self, selection: WorktreeSelection) -> Option<DashboardOutcome> {
        if self.pending.is_some() {
            return None;
        }
        let agent = self.current_agent_name()?;
        match &self.scope {
            Scope::Browse { repos } => {
                let repo = repos.selected()?.repo.clone();
                let repo_label = repo.full_name();
                let repo_root = sync::resolve_local_path(&self.root, &repo.owner, &repo.name);
                let (slug, worktree_path) = match selection {
                    WorktreeSelection::Existing(entry) => {
                        (worktree::unflatten_slug(&entry.slug), Some(entry.path))
                    }
                    WorktreeSelection::New => (
                        self.forced_slug
                            .take()
                            .unwrap_or_else(worktree::generate_timestamp_slug),
                        None,
                    ),
                };
                self.pending = Some(PendingLaunch {
                    ctx: LaunchContext {
                        repo_label,
                        owner: repo.owner,
                        name: repo.name,
                        slug,
                        agent,
                    },
                    repo_root,
                    worktree_path,
                    // Bare `cw` always pulls before resolving the worktree,
                    // whether resuming or creating (matches the old
                    // `run_default`'s unconditional `clone_or_pull`).
                    stage: Stage::Cloning,
                    freshly_created: false,
                });
            }
            Scope::AllWorktrees => {
                // `cw resume`: launch directly, no clone/create/hooks —
                // matches the old `run_resume`, which never pulled.
                let WorktreeSelection::Existing(entry) = selection else {
                    return None;
                };
                self.pending = Some(PendingLaunch {
                    ctx: LaunchContext {
                        repo_label: entry.repo.clone(),
                        owner: String::new(),
                        name: String::new(),
                        slug: worktree::unflatten_slug(&entry.slug),
                        agent,
                    },
                    repo_root: PathBuf::new(),
                    worktree_path: Some(entry.path),
                    stage: Stage::Launching,
                    freshly_created: false,
                });
            }
            Scope::SingleRepo {
                repo_label,
                repo_root,
            } => {
                let repo_label = repo_label.clone();
                let repo_root = repo_root.clone();
                let (slug, worktree_path, stage) = match selection {
                    WorktreeSelection::Existing(entry) => (
                        worktree::unflatten_slug(&entry.slug),
                        Some(entry.path),
                        Stage::Launching,
                    ),
                    WorktreeSelection::New => (
                        self.forced_slug
                            .take()
                            .unwrap_or_else(worktree::generate_timestamp_slug),
                        None,
                        Stage::CreatingWorktree,
                    ),
                };
                self.pending = Some(PendingLaunch {
                    ctx: LaunchContext {
                        repo_label,
                        owner: String::new(),
                        name: String::new(),
                        slug,
                        agent,
                    },
                    repo_root,
                    worktree_path,
                    stage,
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
                self.status = Some(format!("clone/pull failed: {e}"));
                self.pending = None;
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
    /// from `dashboard.rs`'s loop (not routed through a `Msg`) — mirrors the
    /// plan's driver pseudocode exactly.
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
    /// `pending`/`modal`/`status` — `dirty_cache` for the launched worktree
    /// is refreshed separately via `Msg::DirtyRefreshed`, once the driver
    /// has a fresh `gitstatus::is_dirty` read.
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
                        self.status = Some(format!("worktree creation failed: {e:#}"));
                        self.pending = None;
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
                    let agent_cfg =
                        match config::resolve_agent(Some(&pending.ctx.agent), &self.config) {
                            Ok(cfg) => cfg,
                            Err(e) => {
                                self.status = Some(format!("{e:#}"));
                                self.pending = None;
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
    /// Symlinks + `.worktreeinclude` best-effort, mirroring `main.rs`'s old
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
            // either call surfaces via `advance_pending`'s `Err(e) =>
            // self.status = ...` branch instead of being silently discarded.
            // Per-file copy failures inside a successful `apply_worktreeinclude`
            // call remain non-fatal, logged as warnings — that's the
            // deliberate continue-on-error behavior `worktreeinclude.rs`
            // documents, not part of what this propagates.
            worktree::symlink_shared_dirs(&pending.repo_root, &path, &self.config.symlink_dirs)?;
            let failures = worktreeinclude::apply_worktreeinclude(&pending.repo_root, &path)?;
            for f in &failures {
                tracing::warn!(
                    file = %f.path.display(),
                    error = %f.error,
                    "worktreeinclude: failed to copy file, continuing"
                );
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn idle_annotation_formatting() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(20 * 86_400);
        let recent = now - Duration::from_secs(2 * 86_400);

        assert_eq!(humanize(old, now, 14), Some("idle 20d".to_string()));
        assert_eq!(humanize(recent, now, 14), None);
        assert_eq!(humanize(old, now, 20), Some("idle 20d".to_string()));
        assert_eq!(humanize(old, now, 21), None);
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
}
