use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use super::msg::RepoLoad;
use super::widgets::filter_indices;
use crate::config::AgentConfig;
use crate::github::Repo;
use crate::gitstatus;
use crate::picker::{CleanCandidate, WorktreeSelection};
use crate::sync;
use crate::worktree::{self, WorktreeEntry as ScannedEntry};

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

/// Coarse "Nm/Nh/Nd ago" label for the repo screen's UPDATED column,
/// computed from `Repo.updated_at`'s raw ISO8601 string against `now`. A
/// timestamp that fails to parse (shouldn't happen against real `gh`
/// output — same tolerance idiom as `github::parse_gh_output`) falls back to
/// the raw string rather than panicking or blanking the column.
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

/// Shared filterable-table state: the full item list, live filter query,
/// the filtered index subset (`filtered[i]` is an index into `items`), the
/// `TableState` (selection/scroll), and the table's last-rendered `Rect`.
///
/// `table`/`table_rect` use interior mutability (`RefCell`/`Cell`)
/// deliberately: `Screen::draw` takes `&Model` (see its doc comment — never
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
// Repo screen
// ---------------------------------------------------------------------

/// A repo, pre-annotated for the repo screen's table + fuzzy filter.
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

pub enum RepoOutcome {
    Selected(Repo),
    Cancelled,
}

/// The repo screen's `Model`: opens immediately with whatever's cached
/// (`initial`, possibly empty on a cold cache) while `main.rs`'s background
/// discovery thread streams a `RepoLoad` in via `Msg::DataLoaded` — see the
/// Rendering-conflicts note in `tui::mod` for why that thread never logs
/// directly. `root` is retained so a later `DataLoaded` can rebuild each
/// row's LOCAL column, not just replace the `Repo` list.
pub struct RepoModel {
    pub list: ListState<RepoRow>,
    pub root: PathBuf,
    pub loading: bool,
    pub status: Option<String>,
}

impl RepoModel {
    pub fn new(initial: Vec<Repo>, root: PathBuf) -> Self {
        let rows = build_repo_rows(initial, &root);
        Self {
            list: ListState::new(rows),
            root,
            loading: true,
            status: None,
        }
    }

    /// Applies a background-thread result: replaces the item list (unless
    /// the fetch came back empty — e.g. a total failure with no cache to
    /// fall back to — in which case whatever's already showing is kept
    /// rather than blanking the screen), re-applies the live filter query,
    /// and folds any warnings into the status line.
    pub fn apply_load(&mut self, load: RepoLoad) {
        self.loading = false;
        if !load.repos.is_empty() {
            let rows = build_repo_rows(load.repos, &self.root);
            let query = std::mem::take(&mut self.list.query);
            self.list = ListState::new(rows);
            self.list.query = query;
            self.list.refilter(|r| r.filter_text.as_str());
        }

        let mut messages = Vec::new();
        if let Some(w) = load.stale_warning {
            messages.push(w);
        }
        if !load.warnings.is_empty() {
            messages.push(load.warnings.join("; "));
        }
        self.status = (!messages.is_empty()).then(|| messages.join(" | "));
    }
}

// ---------------------------------------------------------------------
// Worktree(+agent) screen
// ---------------------------------------------------------------------

/// A worktree row (or the synthetic "+ new worktree" row, single-select
/// only), annotated with idle/dirty state at construction — same
/// annotation `cw clean`'s dirty/idle columns and the default flow's
/// resume picker both need, built in exactly one place.
pub struct WorktreeRow {
    pub selection: WorktreeSelection,
    pub dirty: bool,
    pub idle_label: Option<String>,
    pub repo_label: String,
    pub filter_text: String,
}

impl WorktreeRow {
    fn existing(entry: ScannedEntry, idle_threshold_days: u64, now: SystemTime) -> Self {
        // Best-effort: a worktree whose status can't be read (permissions, a
        // race with concurrent deletion) is annotated as clean rather than
        // aborting the whole screen over one entry — same tolerance idiom
        // `scan_worktrees` itself uses.
        let dirty = gitstatus::is_dirty(&entry.path).unwrap_or(false);
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
    entries: Vec<ScannedEntry>,
    idle_threshold_days: u64,
    include_new: bool,
) -> Vec<WorktreeRow> {
    let now = SystemTime::now();
    let mut rows: Vec<WorktreeRow> = entries
        .into_iter()
        .map(|e| WorktreeRow::existing(e, idle_threshold_days, now))
        .collect();
    if include_new {
        rows.push(WorktreeRow::new_row());
    }
    rows
}

/// An agent, annotated with its resolved command line — the agent
/// sub-panel's row.
pub struct AgentRow {
    pub name: String,
    pub cmd_preview: String,
    pub filter_text: String,
}

impl AgentRow {
    fn new(name: String, cfg: &AgentConfig) -> Self {
        let mut cmd_preview = cfg.cmd.clone();
        for arg in &cfg.args {
            cmd_preview.push(' ');
            cmd_preview.push_str(arg);
        }
        let filter_text = name.clone();
        Self {
            name,
            cmd_preview,
            filter_text,
        }
    }
}

fn build_agent_rows(agents: &HashMap<String, AgentConfig>) -> Vec<AgentRow> {
    // Sorted, not `HashMap` iteration order — deterministic run to run,
    // same discipline the old `picker::pick_agent` applied.
    let mut names: Vec<&String> = agents.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| AgentRow::new(name.clone(), &agents[name]))
        .collect()
}

/// A standalone, top-level agent picker (no worktree involved) — backs
/// `picker::pick_agent`, the fallback used whenever agent resolution is
/// needed independently of a worktree choice (an explicit slug, or no
/// worktrees yet for a repo). Shares `AgentRow`/`build_agent_rows` with the
/// worktree screen's inline sub-panel so the two never drift apart.
pub struct AgentModel {
    pub list: ListState<AgentRow>,
}

pub enum AgentOutcome {
    Selected(String),
    Cancelled,
}

impl AgentModel {
    pub fn new(agents: &HashMap<String, AgentConfig>) -> Self {
        Self {
            list: ListState::new(build_agent_rows(agents)),
        }
    }
}

/// Single-select mode's sub-state: the agent sub-panel (built once, up
/// front, from the same `agents` map every time — never re-fetched) and
/// whether it's currently the active overlay. `pending` holds the worktree
/// selection Enter was pressed on, waiting for the overlay to resolve an
/// agent name before the screen can finish.
pub enum WorktreeMode {
    Single {
        agent_needed: bool,
        // Boxed: `Multi` carries no data at all, so an unboxed
        // `ListState<AgentRow>` here would size the whole enum (every
        // `WorktreeModel`, including every `Multi`/`cw clean` one) to this
        // variant's much larger footprint (clippy::large_enum_variant).
        agents: Box<ListState<AgentRow>>,
        agent_overlay: bool,
        pending: Option<WorktreeSelection>,
    },
    Multi,
}

pub enum WorktreeOutcome {
    Single {
        selection: WorktreeSelection,
        agent: Option<String>,
    },
    Multi(Vec<CleanCandidate>),
    Cancelled,
}

pub struct WorktreeModel {
    pub list: ListState<WorktreeRow>,
    pub mode: WorktreeMode,
    /// Item indices (into `list.items`, not `list.filtered`) currently
    /// checked — survives the filter changing which rows are visible.
    /// `Multi` mode only.
    pub checked: HashSet<usize>,
    pub status: Option<String>,
}

impl WorktreeModel {
    pub fn new_single(
        entries: Vec<ScannedEntry>,
        idle_threshold_days: u64,
        include_new: bool,
        agents: &HashMap<String, AgentConfig>,
        agent_needed: bool,
    ) -> Self {
        let rows = build_worktree_rows(entries, idle_threshold_days, include_new);
        Self {
            list: ListState::new(rows),
            mode: WorktreeMode::Single {
                agent_needed,
                agents: Box::new(ListState::new(build_agent_rows(agents))),
                agent_overlay: false,
                pending: None,
            },
            checked: HashSet::new(),
            status: None,
        }
    }

    pub fn new_multi(entries: Vec<ScannedEntry>, idle_threshold_days: u64) -> Self {
        let rows = build_worktree_rows(entries, idle_threshold_days, false);
        Self {
            list: ListState::new(rows),
            mode: WorktreeMode::Multi,
            checked: HashSet::new(),
            status: None,
        }
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
        // Boundary: exactly at the threshold counts as idle ("N >= threshold").
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
}
