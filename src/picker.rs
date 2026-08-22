use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::mpsc;

use anyhow::{anyhow, Result};
use ratatui::Frame;

use crate::config::AgentConfig;
use crate::github::Repo;
use crate::tui::{self, model, update, view, Msg, RepoLoad};
use crate::worktree::WorktreeEntry as ScannedEntry;

/// Outcome of any interactive pick: a real selection, an empty source list
/// (message already printed, the TUI never opened), or the user cancelling
/// out of the screen (Esc/Ctrl-C). Distinguishing `Empty` from `Cancelled`
/// lets callers skip printing their own redundant message for the former
/// while still reacting distinctly if they ever need to.
pub enum Pick<T> {
    Selected(T),
    Empty,
    Cancelled,
}

/// One of the two rows the worktree screen can return: a real,
/// previously-created worktree, or the synthetic "+ new worktree" row
/// (§0a) offered only when the caller passes `include_new: true`.
#[derive(Debug, Clone)]
pub enum WorktreeSelection {
    Existing(ScannedEntry),
    New,
}

/// One worktree selected out of `cw clean`'s multi-select screen, plus the
/// dirty flag already computed while annotating the row — so `clean.rs`
/// never needs to re-open the repo itself just to decide whether `--force`
/// is required.
#[derive(Debug, Clone)]
pub struct CleanCandidate {
    pub entry: ScannedEntry,
    pub dirty: bool,
}

/// Gate on `/dev/tty`, not `stdin().is_terminal()` (F35 — the TUI backend
/// renders straight to `/dev/stderr`, not through stdin/stdout — see
/// `tui::mod`'s `TerminalGuard` — so a piped stdin plausibly still reaches a
/// real controlling terminal; the case that actually matters is no
/// controlling terminal at all, e.g. a workflow agent). Unchanged from the
/// `skim`-backed picker: re-verified against the new backend, not just
/// carried over — see CLAUDE.md.
pub fn is_interactive() -> bool {
    std::fs::File::open("/dev/tty").is_ok() && std::io::stderr().is_terminal()
}

// ---------------------------------------------------------------------
// Repo screen
// ---------------------------------------------------------------------

struct RepoScreen {
    model: model::RepoModel,
    rx: mpsc::Receiver<RepoLoad>,
}

impl tui::Screen for RepoScreen {
    type Outcome = model::RepoOutcome;

    fn update(&mut self, msg: Msg) -> Option<Self::Outcome> {
        update::update_repo(&mut self.model, msg)
    }

    fn draw(&self, frame: &mut Frame) {
        view::draw_repo(frame, &self.model)
    }

    fn poll_background(&mut self) -> Option<Msg> {
        self.rx.try_recv().ok().map(Msg::DataLoaded)
    }
}

/// Opens the repo screen against `initial` (whatever `main.rs` already had
/// cached, possibly empty) and streams `rx` in as `Msg::DataLoaded` — see
/// `main.rs::pick_repo_interactive`, which owns the background fetch thread
/// this receiver is the other end of. `root` builds each row's LOCAL column
/// (`root/owner/name/.git` exists) both now and on every later
/// `Msg::DataLoaded` refresh.
///
/// Unlike the old `skim`-backed version, an empty *initial* list is never a
/// precondition failure here — it just means a cold cache, not "no repos" —
/// so there is no synchronous empty-list precheck before opening the
/// screen; a genuinely empty result (real total or `--org` filter with
/// nothing to show) is a state the screen itself renders, not a case this
/// function short-circuits on. That's this function's one signature/
/// behavior change from the pre-rewrite `pick_repo(repos: Vec<Repo>)`: it
/// gains `root` and `rx` because moving the fetch off the render path
/// requires them, not because callers changed. Nothing else in `Pick<T>` or
/// the public shape moved.
pub fn pick_repo(
    root: &Path,
    initial: Vec<Repo>,
    rx: mpsc::Receiver<RepoLoad>,
) -> Result<Pick<Repo>> {
    if !is_interactive() {
        return Err(anyhow!(
            "no interactive terminal available to pick a repo — pass --repo OWNER/NAME instead"
        ));
    }

    let screen = RepoScreen {
        model: model::RepoModel::new(initial, root.to_path_buf()),
        rx,
    };
    match tui::run(screen)? {
        model::RepoOutcome::Selected(repo) => Ok(Pick::Selected(repo)),
        model::RepoOutcome::Cancelled => Ok(Pick::Cancelled),
    }
}

// ---------------------------------------------------------------------
// Worktree(+agent) screen
// ---------------------------------------------------------------------

struct WorktreeScreen {
    model: model::WorktreeModel,
}

impl tui::Screen for WorktreeScreen {
    type Outcome = model::WorktreeOutcome;

    fn update(&mut self, msg: Msg) -> Option<Self::Outcome> {
        update::update_worktree(&mut self.model, msg)
    }

    fn draw(&self, frame: &mut Frame) {
        view::draw_worktree(frame, &self.model)
    }
}

/// Single-select worktree-plus-agent screen — replaces the old back-to-back
/// `pick_worktree` + `pick_agent` call pair with one screen. `include_new:
/// true` appends the "+ new worktree" row (§0a's default-flow existing-
/// worktrees-first check); `false` (`cw resume`) offers only real
/// worktrees. When `agent_needed` is `true` and the user commits a worktree
/// row, the same screen opens an inline agent sub-panel instead of tearing
/// down and relaunching a second full-screen picker; the returned tuple's
/// agent is `Some` only when that sub-panel resolved one. When
/// `agent_needed` is `false` the sub-panel never opens — the returned agent
/// is always `None`, and the caller resolves the name the same way it
/// always has (`--agent`/`default_agent`).
pub fn pick_worktree_and_agent(
    entries: Vec<ScannedEntry>,
    idle_threshold_days: u64,
    include_new: bool,
    agents: &HashMap<String, AgentConfig>,
    agent_needed: bool,
) -> Result<Pick<(WorktreeSelection, Option<String>)>> {
    if entries.is_empty() {
        println!("no worktrees yet");
        return Ok(Pick::Empty);
    }
    if !is_interactive() {
        return Err(anyhow!(
            "no interactive terminal available to pick a worktree"
        ));
    }

    let screen = WorktreeScreen {
        model: model::WorktreeModel::new_single(
            entries,
            idle_threshold_days,
            include_new,
            agents,
            agent_needed,
        ),
    };
    match tui::run(screen)? {
        model::WorktreeOutcome::Single { selection, agent } => {
            Ok(Pick::Selected((selection, agent)))
        }
        model::WorktreeOutcome::Multi(_) => {
            unreachable!("the single-select screen never returns a Multi outcome")
        }
        model::WorktreeOutcome::Cancelled => Ok(Pick::Cancelled),
    }
}

/// Multi-select worktree screen backing `cw clean` — never offers the
/// synthetic "+ new worktree" row (nothing to "clean" about creating one).
pub fn pick_worktrees_multi(
    entries: Vec<ScannedEntry>,
    idle_threshold_days: u64,
) -> Result<Pick<Vec<CleanCandidate>>> {
    if entries.is_empty() {
        println!("no worktrees yet");
        return Ok(Pick::Empty);
    }
    if !is_interactive() {
        return Err(anyhow!(
            "no interactive terminal available to pick worktrees to clean"
        ));
    }

    let screen = WorktreeScreen {
        model: model::WorktreeModel::new_multi(entries, idle_threshold_days),
    };
    match tui::run(screen)? {
        model::WorktreeOutcome::Multi(candidates) => Ok(Pick::Selected(candidates)),
        model::WorktreeOutcome::Single { .. } => {
            unreachable!("the multi-select screen never returns a Single outcome")
        }
        model::WorktreeOutcome::Cancelled => Ok(Pick::Cancelled),
    }
}

// ---------------------------------------------------------------------
// Agent-only screen
// ---------------------------------------------------------------------

struct AgentScreen {
    model: model::AgentModel,
}

impl tui::Screen for AgentScreen {
    type Outcome = model::AgentOutcome;

    fn update(&mut self, msg: Msg) -> Option<Self::Outcome> {
        update::update_agent(&mut self.model, msg)
    }

    fn draw(&self, frame: &mut Frame) {
        view::draw_agent(frame, &self.model)
    }
}

/// Picks an agent by name out of `config.toml`'s `[agents]` table, with no
/// worktree involved. `pick_worktree_and_agent`'s inline sub-panel handles
/// the common case (a worktree is also being chosen), but agent resolution
/// is independent of worktree resolution — an explicit slug, or auto-
/// generating a fresh timestamp slug because no worktree exists yet for a
/// repo, both skip the worktree screen entirely while an agent may still
/// need picking. This is that fallback (`main.rs::resolve_agent_name`'s
/// picker of last resort), reimplemented against `crate::tui` in place of
/// `skim` like every other screen here.
pub fn pick_agent(agents: &HashMap<String, AgentConfig>) -> Result<Pick<String>> {
    if agents.is_empty() {
        println!("no agents configured — check [agents] in config.toml");
        return Ok(Pick::Empty);
    }
    if !is_interactive() {
        return Err(anyhow!(
            "no interactive terminal available to pick an agent — pass --agent NAME instead"
        ));
    }

    let screen = AgentScreen {
        model: model::AgentModel::new(agents),
    };
    match tui::run(screen)? {
        model::AgentOutcome::Selected(name) => Ok(Pick::Selected(name)),
        model::AgentOutcome::Cancelled => Ok(Pick::Cancelled),
    }
}
