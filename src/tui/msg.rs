use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyEvent, MouseEvent};

use crate::github::Repo;
use crate::sync::PullOutcome;
use crate::worktree::WorktreeEntry as ScannedEntry;

/// Every event the dashboard's pure `update_dashboard` function reacts to.
#[derive(Debug, Clone)]
pub enum Msg {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    /// Repo-discovery background thread result (`Scope::Browse` only),
    /// polled off an `mpsc::Receiver` once per event-loop tick.
    DataLoaded(RepoLoad),
    /// Full worktree-scan-plus-dirty-status background thread result.
    /// Spawned once at dashboard construction, and re-armed on demand by the
    /// `r` key (`DashboardModel::rescan_requested`, handled in
    /// `dashboard.rs`) — see `WorktreesLoad`.
    WorktreesLoaded(Result<WorktreesLoad, String>),
    /// Background clone/pull thread result for a repo the user committed to
    /// (Enter on a worktree row) — see `CloneOutcome`.
    CloneDone(Result<CloneOutcome, String>),
    /// A fresh `gitstatus::is_dirty` read for one worktree path, reported by
    /// `dashboard.rs`'s driver loop after resuming from a suspended hook run
    /// or agent launch (either could have changed the worktree's dirty
    /// state). `Err` only on a real I/O failure reading the worktree's git
    /// status — folded into `dirty_cache` as "unknown, assume clean" rather
    /// than surfaced as a hard error, same tolerance idiom
    /// `WorktreeRow::existing` used before this rewrite.
    DirtyRefreshed(PathBuf, Result<bool, String>),
    /// Background self-update check result (`selfupdate::spawn_check`) —
    /// `Some(version)` when a newer release is pending, `None` when up to
    /// date. Never sent at all on a failed check (no install receipt,
    /// offline, rate-limited — see `selfupdate.rs`), so this simply never
    /// arrives on those runs, same as if the check hadn't happened.
    UpdateChecked(Option<String>),
    /// Fired once per event-loop tick (background-poll cadence; also drives
    /// the clone spinner and "loading…" states).
    Tick,
}

/// What the repo-discovery background thread reports back: a fetched (or
/// cached) repo list, any per-org discovery warnings, and a stale-cache
/// notice when a live fetch failed but a cached list covered the gap. All
/// three route into the dashboard's status line instead of
/// `tracing::warn!`/`eprintln!`, which would otherwise corrupt the frame
/// (see the Rendering-conflicts note in `tui::mod`).
#[derive(Debug, Clone, Default)]
pub struct RepoLoad {
    pub repos: Vec<Repo>,
    pub warnings: Vec<String>,
    pub stale_warning: Option<String>,
}

/// Everything the worktree pane needs from one full background scan: every
/// worktree found across every repo, plus a `gitstatus::is_dirty` pass over
/// each entry's path (`DashboardModel::dirty_cache`). Computed off the
/// render thread — at dashboard startup, and again on each `r`-triggered
/// rescan — so a repo-cursor move or a filter keystroke is always a pure
/// in-memory read, never an `is_dirty` call.
#[derive(Debug, Clone, Default)]
pub struct WorktreesLoad {
    pub entries: Vec<ScannedEntry>,
    pub dirty: HashMap<PathBuf, bool>,
}

/// Background clone/pull thread result — carries only plain `Clone`-able
/// data, never a `git2::Repository` (not worth making `Send` do the work of
/// crossing the channel when the dashboard just reopens the repo locally on
/// the main thread the moment this lands — see `dashboard.rs`).
#[derive(Debug, Clone)]
pub struct CloneOutcome {
    pub repo_label: String,
    pub pull_outcome: PullOutcome,
}
