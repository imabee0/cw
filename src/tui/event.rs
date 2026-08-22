use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event as CEvent};

use super::msg::Msg;

/// Event-loop poll cadence: how long `poll_next` blocks waiting for a
/// terminal event before giving up and letting the caller treat it as a
/// `Msg::Tick`. Also the background-`mpsc::Receiver` poll/spinner cadence —
/// see `tui::mod::run`.
pub const TICK: Duration = Duration::from_millis(100);

/// Blocks up to `TICK` for a terminal event and converts it to a `Msg`.
/// `Ok(None)` means either the poll timed out (caller synthesizes
/// `Msg::Tick`) or the underlying event was one this TUI never acts on
/// (e.g. a raw cursor-position report) — both are "nothing to do this
/// round", collapsed into one variant rather than forcing every caller to
/// match a `Msg` this module would otherwise never construct.
pub fn poll_next() -> Result<Option<Msg>> {
    if !event::poll(TICK)? {
        return Ok(None);
    }
    Ok(match event::read()? {
        CEvent::Key(key) => Some(Msg::Key(key)),
        CEvent::Mouse(mouse) => Some(Msg::Mouse(mouse)),
        CEvent::Resize(_, _) => Some(Msg::Resize),
        _ => None,
    })
}
