//! Model/Msg/Update/View TUI subsystem (bubbletea-shaped) backing `cw`'s
//! single persistent dashboard (`dashboard.rs`). `run` owns the terminal
//! lifecycle for one screen at a time, but a screen now survives a
//! suspend/resume round trip (e.g. to run a hook or launch an agent with
//! inherited stdio) instead of being torn down for good — see `run`'s
//! `(S, S::Outcome)` return shape and `dashboard.rs`'s driver loop.

pub mod event;
pub mod model;
pub mod msg;
pub mod update;
pub mod view;
pub mod widgets;

use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::{Frame, Terminal};

pub use msg::{Msg, RepoLoad};

/// Gate on `/dev/tty`, not `stdin().is_terminal()` — the TUI backend renders
/// straight to `/dev/stderr`, not through stdin/stdout (see `TerminalGuard`
/// below), so a piped stdin plausibly still reaches a real controlling
/// terminal; the case that actually matters is no controlling terminal at
/// all, e.g. a workflow agent. Moved here from the old `picker.rs` (now
/// `dashboard.rs`) — this is the TUI backend's own precondition, not a
/// picker-specific one, so it belongs next to the backend it gates.
pub fn is_interactive() -> bool {
    std::fs::File::open("/dev/tty").is_ok() && io::stderr().is_terminal()
}

/// Set for the lifetime of any `run()` call, cleared on every return path
/// (including panic). `main.rs::init_logging`'s stderr writer checks this
/// before every log line: the TUI backend renders straight to stderr (see
/// the module-level rationale in `main.rs` and CLAUDE.md), so a
/// `tracing::warn!`/`tracing::error!` firing mid-session would interleave
/// raw log text with in-progress terminal escape codes and corrupt the
/// frame. While this is `true`, log lines still reach the day's log file
/// (`tracing_appender`'s file writer is never gated) — only the live
/// stderr tee is suppressed.
pub static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// A screen's Model/Update/View triple, wired into the shared event loop.
/// `Model` state and the pure `update`/`draw` functions live in
/// `tui::model`/`tui::update`/`tui::view`; concrete implementors here are
/// thin adapters that hold a `Model` and forward into those pure functions
/// (kept pure and directly unit-testable, independent of any real
/// terminal — see the module docs on `update`).
pub trait Screen {
    type Outcome;

    /// Mutates the screen's own state in response to `msg`; returns
    /// `Some(outcome)` only once the screen is done (selected/cancelled),
    /// at which point `run` tears the terminal down and returns it.
    fn update(&mut self, msg: Msg) -> Option<Self::Outcome>;

    /// Renders the current state. Never mutates screen state directly —
    /// geometry a later mouse click needs (e.g. the table's rendered `Rect`)
    /// is cached via interior mutability (`Cell`) inside the `Model`, not by
    /// widening this signature to `&mut self`.
    fn draw(&self, frame: &mut Frame);

    /// Non-blocking poll for work that isn't a terminal event — currently
    /// only the repo screen's background discovery thread. Default: no
    /// background work, so most screens never override this.
    fn poll_background(&mut self) -> Option<Msg> {
        None
    }
}

/// RAII terminal lifecycle: raw mode + alternate screen + mouse capture, all
/// on **stderr**, restored on every return path (`?`-early-return included)
/// via `Drop` — same idiom `main.rs` already uses for `WorkerGuard` around
/// the log appender. Rendering to stderr, not stdout, keeps `cw | cat`'s
/// stdout free of UI escape codes and matches this module's own
/// `is_interactive()`'s documented `stderr().is_terminal()` invariant
/// (CLAUDE.md).
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
}

impl TerminalGuard {
    /// `enable_raw_mode()` first, with nothing to unwind if it's the step
    /// that fails. Only once it succeeds is `TUI_ACTIVE` set and the
    /// remaining fallible setup (`execute!`, `Terminal::new`) attempted — if
    /// either of those errors, `restore()` runs before returning `Err`,
    /// because at that point no `Self` exists yet for `Drop` to clean up
    /// after. Getting this ordering wrong left a real gap: raw mode enabled
    /// with `TUI_ACTIVE` stuck `true` and no `Self` to `Drop`, which also
    /// silently swallowed the setup error itself — `main.rs`'s
    /// `tracing::error!` on the error path is stderr-gated by `TUI_ACTIVE`,
    /// so the user would see nothing but "details logged to ...".
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        TUI_ACTIVE.store(true, Ordering::Relaxed);
        match execute!(io::stderr(), EnterAlternateScreen, EnableMouseCapture)
            .map_err(anyhow::Error::from)
            .and_then(|()| Ok(Terminal::new(CrosstermBackend::new(io::stderr()))?))
        {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                Self::restore();
                Err(e)
            }
        }
    }

    /// Shared by `Drop` and the panic hook — both need the exact same
    /// best-effort teardown. A Drop/panic-hook can't propagate an error, and
    /// a session that's already ending is exactly the wrong place to start
    /// one, so each call is independent: one failing (e.g. raw mode already
    /// off) doesn't skip the others.
    fn restore() {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), LeaveAlternateScreen, DisableMouseCapture);
        TUI_ACTIVE.store(false, Ordering::Relaxed);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

static PANIC_HOOK_INIT: Once = Once::new();

/// Wraps the previous panic hook with one that restores the terminal first —
/// a panic mid-render must never leave the user's shell in raw
/// mode/alternate-screen. Installed at most once per process: `run()` is
/// called twice per `cw` invocation in the common case (repo screen, then
/// the worktree+agent screen), and re-wrapping on every call would nest an
/// unbounded chain of hooks.
fn install_panic_hook() {
    PANIC_HOOK_INIT.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            TerminalGuard::restore();
            previous(info);
        }));
    });
}

/// Runs `screen`'s event loop to completion: enters the terminal, redraws on
/// every message, and returns once `Screen::update` yields an `Outcome`.
/// Background-thread results (`poll_background`) are drained before every
/// blocking read so a repo-discovery result already sitting in the channel
/// is applied — and redrawn — without waiting a full tick.
///
/// Returns the screen back alongside the outcome (not just the outcome) —
/// `dashboard.rs`'s driver loop needs it to survive a suspend/resume round
/// trip: an `Outcome::Suspend` means the caller is about to run a hook or
/// launch an agent with inherited stdio (tearing this terminal down via
/// `TerminalGuard`'s `Drop`), then call `run` again on the SAME screen once
/// that's done, rather than losing all accumulated model state.
pub fn run<S: Screen>(mut screen: S) -> Result<(S, S::Outcome)> {
    install_panic_hook();
    let mut guard = TerminalGuard::enter()?;
    // Forces a full repaint on entry — including a resume after a suspend,
    // where ratatui's diff buffer from before the alt-screen toggle would
    // otherwise think large parts of the frame are already correct and skip
    // redrawing them.
    guard.terminal.clear()?;

    loop {
        guard.terminal.draw(|frame| screen.draw(frame))?;

        if let Some(msg) = screen.poll_background() {
            if let Some(outcome) = screen.update(msg) {
                return Ok((screen, outcome));
            }
            continue;
        }

        match event::poll_next()? {
            Some(msg) => {
                if let Some(outcome) = screen.update(msg) {
                    return Ok((screen, outcome));
                }
            }
            None => {
                if let Some(outcome) = screen.update(Msg::Tick) {
                    return Ok((screen, outcome));
                }
            }
        }
    }
}
