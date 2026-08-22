use ratatui::crossterm::event::{KeyEvent, MouseEvent};

use crate::github::Repo;

/// Every event a screen's pure `update` function reacts to. Deliberately
/// flat, not per-screen — a screen with no background work simply never
/// receives `DataLoaded` (only the repo screen spawns a background thread;
/// see `tui::mod::run`'s `Screen::poll_background`).
#[derive(Debug, Clone)]
pub enum Msg {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    /// Repo-discovery background thread result, polled off an
    /// `mpsc::Receiver` once per event-loop tick.
    DataLoaded(RepoLoad),
    /// Fired once per event-loop tick (background-poll cadence; also drives
    /// the "loading…"/"refreshing…" spinner). Carries nothing — screens that
    /// don't animate anything simply ignore it.
    Tick,
}

/// What the repo-discovery background thread reports back — mirrors what
/// `main.rs`'s formerly-synchronous `pick_repo_interactive` computed inline:
/// a fetched (or cached) repo list, any per-org discovery warnings, and a
/// stale-cache notice when a live fetch failed but a cached list covered the
/// gap. All three route into the repo screen's status line instead of
/// `tracing::warn!`/`eprintln!`, which would otherwise corrupt the frame
/// (see the Rendering-conflicts note in `tui::mod`).
#[derive(Debug, Clone, Default)]
pub struct RepoLoad {
    pub repos: Vec<Repo>,
    pub warnings: Vec<String>,
    pub stale_warning: Option<String>,
}
