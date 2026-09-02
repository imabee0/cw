//! Pure, terminal-free state transitions — `update_dashboard` mutates a
//! `DashboardModel` in response to one `Msg` and optionally yields a
//! terminal `DashboardOutcome`. Every keybinding-gating rule lives here, so
//! it's exercised directly with fixture `Msg` sequences, no real terminal
//! needed (see the `tests` module below). Anything impure — spawning the
//! background clone thread, running a hook, launching the agent CLI — lives
//! in `dashboard.rs`, which inspects the model after each call here and
//! reacts; this file never touches a filesystem, subprocess, or thread.
//!
//! Keys and clicks converge on `perform`: a click resolves to an `Action`
//! (via the hotspots `view.rs` registered) and runs through the very same
//! function the equivalent key does, so nothing is reachable by only one
//! of the two.

use std::time::Instant;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use super::model::{
    Action, DashboardModel, DashboardOutcome, Focus, Modal, SuspendReq, DOUBLE_CLICK,
};
use super::msg::Msg;
use super::widgets;

/// `PageUp`/`PageDown` scroll amount. Not derived from the last-rendered
/// table height (that's `Rect::default()` — height 0 — before the first
/// frame) so the very first keypress of a session isn't a no-op.
const PAGE: isize = 10;

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_ctrl_a(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub fn update_dashboard(model: &mut DashboardModel, msg: Msg) -> Option<DashboardOutcome> {
    match msg {
        Msg::Key(key) => handle_key(model, key),
        Msg::Mouse(mouse) => handle_mouse(model, mouse),
        Msg::Resize => None,
        Msg::Tick => {
            model.ticks = model.ticks.wrapping_add(1);
            None
        }
        Msg::DataLoaded(load) => {
            model.apply_repo_load(load);
            None
        }
        Msg::WorktreesLoaded(result) => {
            model.apply_worktrees_load(result);
            None
        }
        Msg::CloneDone(result) => model.apply_clone_done(result),
        Msg::WorkRefreshed(path, result) => {
            model.apply_work_refresh(path, result);
            None
        }
        Msg::UpdateChecked(pending) => {
            model.apply_update_checked(pending);
            None
        }
    }
}

// ---------------------------------------------------------------------
// Actions — the one place keys and clicks converge
// ---------------------------------------------------------------------

fn perform(model: &mut DashboardModel, action: Action) -> Option<DashboardOutcome> {
    match action {
        Action::SelectAgent(idx) => {
            model.select_agent(idx);
            None
        }
        Action::CycleAgent => {
            model.cycle_agent(1);
            None
        }
        Action::ToggleFocus => {
            toggle_focus(model);
            None
        }
        Action::OpenFocused => handle_enter(model),
        Action::NewWorktree => model.new_worktree(),
        Action::ToggleMark(idx) => {
            model.worktrees.select(Some(idx));
            model.focus = Focus::Worktrees;
            model.toggle_checked_row(idx);
            None
        }
        Action::Delete => {
            model.open_delete_confirm();
            None
        }
        Action::Rescan => {
            model.rescan_requested = true;
            None
        }
        Action::ApplyUpdate => model
            .update_available
            .is_some()
            .then_some(DashboardOutcome::Suspend(SuspendReq::ApplyUpdate)),
        Action::Quit => Some(DashboardOutcome::Cancelled),
        Action::ModalConfirm => modal_confirm(model),
        Action::ModalCancel => modal_cancel(model),
        Action::ModalIncludeRisky => {
            model.toggle_include_risky();
            None
        }
    }
}

fn modal_confirm(model: &mut DashboardModel) -> Option<DashboardOutcome> {
    match model.modal.as_ref()? {
        Modal::HookConsent { .. } => model.resolve_hook_consent(true),
        Modal::ConfirmDelete { .. } => {
            model.confirm_delete();
            None
        }
        Modal::Error { .. } => {
            model.modal = None;
            None
        }
    }
}

fn modal_cancel(model: &mut DashboardModel) -> Option<DashboardOutcome> {
    match model.modal.as_ref()? {
        Modal::HookConsent { .. } => model.resolve_hook_consent(false),
        Modal::ConfirmDelete { .. } | Modal::Error { .. } => {
            model.modal = None;
            None
        }
    }
}

// ---------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------

fn handle_key(model: &mut DashboardModel, key: KeyEvent) -> Option<DashboardOutcome> {
    if is_ctrl_c(key) {
        return Some(DashboardOutcome::Cancelled);
    }
    if model.modal.is_some() {
        return handle_modal_key(model, key);
    }
    if is_ctrl_a(key) {
        return perform(model, Action::CycleAgent);
    }
    handle_pane_key(model, key)
}

fn handle_modal_key(model: &mut DashboardModel, key: KeyEvent) -> Option<DashboardOutcome> {
    match model.modal.as_ref()? {
        Modal::HookConsent { .. } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => perform(model, Action::ModalConfirm),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                perform(model, Action::ModalCancel)
            }
            _ => None,
        },
        Modal::ConfirmDelete { .. } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                perform(model, Action::ModalConfirm)
            }
            KeyCode::Char('f') | KeyCode::Char('F') => perform(model, Action::ModalIncludeRisky),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                perform(model, Action::ModalCancel)
            }
            _ => None,
        },
        Modal::Error { .. } => match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(' ') => {
                perform(model, Action::ModalCancel)
            }
            _ => None,
        },
    }
}

/// The Esc nesting order: modal (already routed away in `handle_key`
/// before reaching here) → the status message → the focused pane's filter
/// → the checked set → cancel the whole dashboard.
fn handle_pane_key(model: &mut DashboardModel, key: KeyEvent) -> Option<DashboardOutcome> {
    match key.code {
        KeyCode::Esc => {
            if model.status.take().is_some() {
                return None;
            }
            if !focused_query_empty(model) {
                clear_focused_query(model);
                return None;
            }
            if !model.checked.is_empty() {
                model.checked.clear();
                return None;
            }
            Some(DashboardOutcome::Cancelled)
        }
        KeyCode::Tab | KeyCode::BackTab => perform(model, Action::ToggleFocus),
        KeyCode::Enter => perform(model, Action::OpenFocused),
        // Agent selection is never typable — `←`/`→` have no filter meaning
        // (the query has no cursor), so they're always live, in every pane.
        KeyCode::Left => {
            model.cycle_agent(-1);
            None
        }
        KeyCode::Right => {
            model.cycle_agent(1);
            None
        }
        // Query-gated like `d`/`r` below, per CLAUDE.md: space is typable
        // filter text (a query can legitimately contain one), so it only
        // marks the focused row when the pane's own query is already empty —
        // otherwise it falls through to `type_into_focused` like any other
        // character.
        KeyCode::Char(' ') if model.focus == Focus::Worktrees && focused_query_empty(model) => {
            model.toggle_checked_focused();
            None
        }
        // Query-gated like `q` below, and for the same reason spelled out in
        // CLAUDE.md: an accidental match here is destructive (it opens the
        // removal confirm), so it gets the gate even in panes where a bare
        // `q` would too.
        KeyCode::Char('d') if model.focus == Focus::Worktrees && focused_query_empty(model) => {
            perform(model, Action::Delete)
        }
        // Not destructive, but gated anyway so `n`/`r` stay typable into an
        // active filter, same discipline as `d`.
        KeyCode::Char('n') if focused_query_empty(model) => perform(model, Action::NewWorktree),
        KeyCode::Char('r') if focused_query_empty(model) => perform(model, Action::Rescan),
        // Same query-gating discipline. Additionally only live once a
        // background check has actually found something pending
        // (`model.update_available`) — otherwise 'u' is just filter text.
        KeyCode::Char('u') if focused_query_empty(model) && model.update_available.is_some() => {
            perform(model, Action::ApplyUpdate)
        }
        KeyCode::Up => {
            move_focused(model, -1);
            None
        }
        KeyCode::Down => {
            move_focused(model, 1);
            None
        }
        KeyCode::PageUp => {
            move_focused(model, -PAGE);
            None
        }
        KeyCode::PageDown => {
            move_focused(model, PAGE);
            None
        }
        KeyCode::Home => {
            move_focused(model, isize::MIN / 2);
            None
        }
        KeyCode::End => {
            move_focused(model, isize::MAX / 2);
            None
        }
        // `j`/`k` are unconditional navigation, never typable into a filter —
        // the deliberate exception CLAUDE.md's gating rule calls out.
        KeyCode::Char('j') => {
            move_focused(model, 1);
            None
        }
        KeyCode::Char('k') => {
            move_focused(model, -1);
            None
        }
        KeyCode::Backspace => {
            backspace_focused(model);
            None
        }
        KeyCode::Char('q') if focused_query_empty(model) => perform(model, Action::Quit),
        KeyCode::Char(c) => {
            type_into_focused(model, c);
            None
        }
        _ => None,
    }
}

/// Enter (or a double-click) on a repo row starts a new worktree under it —
/// clone/pull, create, launch — since that's the only thing a repo row is
/// for; the worktree pane already lists every repo's worktrees. Enter on
/// the Worktrees pane starts the launch pipeline for the focused row
/// (resuming it, or creating one for the synthetic "+ new worktree" row) —
/// marking rows for removal is Space's job, never Enter's.
fn handle_enter(model: &mut DashboardModel) -> Option<DashboardOutcome> {
    match model.focus {
        Focus::Repos => model.new_worktree(),
        Focus::Worktrees => {
            let selection = model.worktrees.selected()?.selection.clone();
            model.start_pending(selection)
        }
    }
}

fn toggle_focus(model: &mut DashboardModel) {
    if model.repos.is_some() {
        model.focus = match model.focus {
            Focus::Repos => Focus::Worktrees,
            Focus::Worktrees => Focus::Repos,
        };
    }
}

fn focused_query_empty(model: &DashboardModel) -> bool {
    match model.focus {
        Focus::Repos => model
            .repos
            .as_ref()
            .is_none_or(|repos| repos.query.is_empty()),
        Focus::Worktrees => model.worktrees.query.is_empty(),
    }
}

fn move_focused(model: &mut DashboardModel, delta: isize) {
    move_pane(model, model.focus, delta);
}

fn move_pane(model: &mut DashboardModel, pane: Focus, delta: isize) {
    match pane {
        Focus::Repos => {
            if let Some(repos) = &model.repos {
                repos.move_selection(delta);
                model.repo_cursor_moved();
            }
        }
        Focus::Worktrees => model.worktrees.move_selection(delta),
    }
}

fn clear_focused_query(model: &mut DashboardModel) {
    match model.focus {
        Focus::Repos => {
            if let Some(repos) = &mut model.repos {
                repos.query.clear();
                repos.refilter(|r| r.filter_text.as_str());
                model.repo_cursor_moved();
            }
        }
        Focus::Worktrees => {
            model.worktrees.query.clear();
            model.worktrees.refilter(|r| r.filter_text.as_str());
        }
    }
}

fn backspace_focused(model: &mut DashboardModel) {
    match model.focus {
        Focus::Repos => {
            if let Some(repos) = &mut model.repos {
                if repos.query.pop().is_some() {
                    repos.refilter(|r| r.filter_text.as_str());
                    model.repo_cursor_moved();
                }
            }
        }
        Focus::Worktrees => {
            if model.worktrees.query.pop().is_some() {
                model.worktrees.refilter(|r| r.filter_text.as_str());
            }
        }
    }
}

fn type_into_focused(model: &mut DashboardModel, c: char) {
    match model.focus {
        Focus::Repos => {
            if let Some(repos) = &mut model.repos {
                repos.query.push(c);
                repos.refilter(|r| r.filter_text.as_str());
                model.repo_cursor_moved();
            }
        }
        Focus::Worktrees => {
            model.worktrees.query.push(c);
            model.worktrees.refilter(|r| r.filter_text.as_str());
        }
    }
}

// ---------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------

/// Left click: a modal, when open, owns every click (only its own buttons
/// respond). Otherwise, in order: a registered hotspot (agent segment,
/// help-line key, mark column), then a table row (focus + select; a second
/// click on the same row within `DOUBLE_CLICK` opens it, same as Enter),
/// then anywhere inside a pane (focus it). Scroll wheel moves the pane
/// under the cursor, falling back to the focused pane.
fn handle_mouse(model: &mut DashboardModel, mouse: MouseEvent) -> Option<DashboardOutcome> {
    let at = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => handle_click(model, at),
        MouseEventKind::ScrollDown => {
            if model.modal.is_none() {
                move_pane(model, pane_at(model, at), 1);
            }
            None
        }
        MouseEventKind::ScrollUp => {
            if model.modal.is_none() {
                move_pane(model, pane_at(model, at), -1);
            }
            None
        }
        _ => None,
    }
}

fn handle_click(model: &mut DashboardModel, at: Position) -> Option<DashboardOutcome> {
    let hit = hotspot_at(model, at);
    if model.modal.is_some() {
        return match hit {
            Some(a @ (Action::ModalConfirm | Action::ModalCancel | Action::ModalIncludeRisky)) => {
                perform(model, a)
            }
            _ => None,
        };
    }
    if let Some(action) = hit {
        return perform(model, action);
    }

    if let Some(idx) = model
        .repos
        .as_ref()
        .and_then(|repos| hit_test(repos.table_rect.get(), repos, at))
    {
        if let Some(repos) = &model.repos {
            repos.select(Some(idx));
        }
        model.focus = Focus::Repos;
        model.repo_cursor_moved();
        if is_double_click(model, Focus::Repos, idx) {
            return perform(model, Action::OpenFocused);
        }
        return None;
    }
    if let Some(idx) = hit_test(model.worktrees.table_rect.get(), &model.worktrees, at) {
        model.worktrees.select(Some(idx));
        model.focus = Focus::Worktrees;
        if is_double_click(model, Focus::Worktrees, idx) {
            return perform(model, Action::OpenFocused);
        }
        return None;
    }

    model.last_click = None;
    if model
        .repos
        .as_ref()
        .is_some_and(|repos| repos.pane_rect.get().contains(at))
    {
        model.focus = Focus::Repos;
    } else if model.worktrees.pane_rect.get().contains(at) {
        model.focus = Focus::Worktrees;
    }
    None
}

fn hotspot_at(model: &DashboardModel, at: Position) -> Option<Action> {
    model
        .hotspots
        .borrow()
        .iter()
        .find(|(rect, _)| rect.contains(at))
        .map(|(_, action)| *action)
}

fn pane_at(model: &DashboardModel, at: Position) -> Focus {
    if model
        .repos
        .as_ref()
        .is_some_and(|repos| repos.pane_rect.get().contains(at))
    {
        Focus::Repos
    } else if model.worktrees.pane_rect.get().contains(at) {
        Focus::Worktrees
    } else {
        model.focus
    }
}

fn is_double_click(model: &mut DashboardModel, pane: Focus, idx: usize) -> bool {
    let now = Instant::now();
    let double = matches!(
        model.last_click,
        Some((p, i, t)) if p == pane && i == idx && now.duration_since(t) <= DOUBLE_CLICK
    );
    model.last_click = if double { None } else { Some((pane, idx, now)) };
    double
}

fn hit_test<T>(area: Rect, list: &super::model::ListState<T>, at: Position) -> Option<usize> {
    let idx = widgets::row_at(area, list.offset(), at.x, at.y)?;
    (idx < list.filtered.len()).then_some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config};
    use crate::gitstatus::WorkState;
    use crate::hooks::HookConsent;
    use crate::worktree::{WorktreeEntry as ScannedEntry, WorktreeSelection};
    use ratatui::layout::Rect;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn key(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_c() -> Msg {
        Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
    }

    fn click(column: u16, row: u16) -> Msg {
        Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn scanned_entry(repo: &str, slug: &str) -> ScannedEntry {
        ScannedEntry {
            repo: repo.to_string(),
            slug: slug.to_string(),
            path: PathBuf::from(format!("/nonexistent-root/{repo}/{slug}")),
            mtime: SystemTime::now(),
        }
    }

    fn agents_map(names: &[&str]) -> HashMap<String, AgentConfig> {
        names
            .iter()
            .map(|n| {
                (
                    (*n).to_string(),
                    AgentConfig {
                        cmd: (*n).to_string(),
                        args: vec![],
                    },
                )
            })
            .collect()
    }

    fn config_with_agents(names: &[&str]) -> Config {
        Config {
            agents: agents_map(names),
            ..Config::default()
        }
    }

    /// Same as `config_with_agents`, plus a `post_clone_hook` so
    /// `DashboardModel::checkpoint` has something to actually check —
    /// `resolve_post_clone_hook` only needs `Some(path)` to resolve (it
    /// joins the path, it doesn't stat it), so this never touches the
    /// filesystem.
    fn config_with_clone_hook(names: &[&str]) -> Config {
        Config {
            post_clone_hook: Some(PathBuf::from("hook.sh")),
            ..config_with_agents(names)
        }
    }

    fn load(entries: Vec<ScannedEntry>) -> super::super::msg::WorktreesLoad {
        super::super::msg::WorktreesLoad {
            work: entries
                .iter()
                .map(|e| (e.path.clone(), Some(WorkState::default())))
                .collect(),
            entries,
        }
    }

    /// Shared fixture: a `DashboardModel` in `Scope::AllWorktrees`, with a
    /// tempdir-backed hook-consent path — never `config::hook_consent_path()`
    /// (the real `~/.cache/cw` store), so a test that reaches
    /// `resolve_hook_consent`'s `save_consent` call never touches the user's
    /// actual machine state. Every entry starts clean (a known, empty
    /// `WorkState`).
    fn all_worktrees_model(entries: Vec<ScannedEntry>) -> DashboardModel {
        all_worktrees_model_force(entries, false)
    }

    fn all_worktrees_model_force(entries: Vec<ScannedEntry>, force: bool) -> DashboardModel {
        let consent_dir = tempfile::tempdir().expect("tempdir");
        let mut model = DashboardModel::new_all_worktrees(
            PathBuf::from("/nonexistent-root"),
            config_with_agents(&["claude", "grok"]),
            HookConsent::new(),
            consent_dir.path().join("hook-consent.json"),
            false,
            force,
        );
        // Leak the tempdir for the model's lifetime — test-only, avoids a
        // dangling consent path once `consent_dir` would otherwise drop.
        std::mem::forget(consent_dir);
        model.apply_worktrees_load(Ok(load(entries)));
        model
    }

    fn browse_model(repos: Vec<(&str, &str)>, entries: Vec<ScannedEntry>) -> DashboardModel {
        let repos = repos
            .into_iter()
            .map(|(owner, name)| crate::github::Repo {
                owner: owner.to_string(),
                name: name.to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .collect();
        let consent_dir = tempfile::tempdir().expect("tempdir");
        let mut model = DashboardModel::new_browse(
            repos,
            PathBuf::from("/nonexistent-root"),
            config_with_agents(&["claude"]),
            HookConsent::new(),
            consent_dir.path().join("hook-consent.json"),
            false,
            None,
        );
        std::mem::forget(consent_dir);
        model.apply_worktrees_load(Ok(load(entries)));
        model
    }

    fn focused_path(model: &DashboardModel) -> PathBuf {
        match &model
            .worktrees
            .selected()
            .expect("a row is focused")
            .selection
        {
            WorktreeSelection::Existing(entry) => entry.path.clone(),
            WorktreeSelection::New => panic!("expected an existing row"),
        }
    }

    // --- Esc nesting: status -> filter -> checked -> cancel ---------------

    #[test]
    fn esc_clears_status_then_query_then_checked_then_cancels() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.status = Some("cloned acme/proj".to_string());
        model.worktrees.query.push_str("abc");
        model.worktrees.refilter(|r| r.filter_text.as_str());
        model
            .checked
            .insert(PathBuf::from("/nonexistent-root/acme/proj/one"));

        assert!(update_dashboard(&mut model, key(KeyCode::Esc)).is_none());
        assert!(model.status.is_none(), "first Esc dismisses the status");
        assert_eq!(model.worktrees.query, "abc");

        assert!(update_dashboard(&mut model, key(KeyCode::Esc)).is_none());
        assert_eq!(model.worktrees.query, "", "second Esc clears the filter");
        assert_eq!(
            model.checked.len(),
            1,
            "clearing the filter must not also clear the checked set"
        );

        assert!(update_dashboard(&mut model, key(KeyCode::Esc)).is_none());
        assert!(model.checked.is_empty(), "third Esc clears the checked set");

        match update_dashboard(&mut model, key(KeyCode::Esc)) {
            Some(DashboardOutcome::Cancelled) => {}
            _ => panic!("fourth Esc, with nothing left to back out of, must cancel"),
        }
    }

    #[test]
    fn ctrl_c_cancels_immediately_regardless_of_state() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.worktrees.query.push('c');
        model
            .checked
            .insert(PathBuf::from("/nonexistent-root/acme/proj/one"));
        match update_dashboard(&mut model, ctrl_c()) {
            Some(DashboardOutcome::Cancelled) => {}
            _ => panic!("Ctrl-C must cancel even with filter text and a checked row"),
        }
    }

    // --- Per-pane query isolation across Tab -----------------------------

    #[test]
    fn tab_swaps_focus_without_clearing_either_panes_query() {
        let mut model = browse_model(
            vec![("acme", "proj")],
            vec![scanned_entry("acme/proj", "one")],
        );

        assert_eq!(
            model.focus,
            Focus::Worktrees,
            "bare cw opens focused on the worktree pane, not the repo pane"
        );
        update_dashboard(&mut model, key(KeyCode::Char('z')));
        assert_eq!(model.focus, Focus::Worktrees);

        update_dashboard(&mut model, key(KeyCode::Tab));
        assert_eq!(model.focus, Focus::Repos);
        update_dashboard(&mut model, key(KeyCode::Char('a')));

        update_dashboard(&mut model, key(KeyCode::Tab));
        assert_eq!(model.focus, Focus::Worktrees);

        let repos = model
            .repos
            .as_ref()
            .expect("Scope::Browse must carry a repo pane");
        assert_eq!(repos.query, "a", "repo pane's own query must survive Tab");
        assert_eq!(
            model.worktrees.query, "z",
            "worktree pane's own query must survive Tab"
        );
    }

    // --- Repo-cursor movement keeps the worktree pane's state ------------

    #[test]
    fn repo_cursor_move_preserves_checked_set_and_worktree_selection() {
        let mut model = browse_model(
            vec![("acme", "first"), ("acme", "second")],
            vec![
                scanned_entry("acme/first", "one"),
                scanned_entry("acme/second", "two"),
            ],
        );

        // The worktree pane shows every repo's worktrees regardless of the
        // repo pane's cursor, not just the selected repo's.
        let existing_count = model
            .worktrees
            .items
            .iter()
            .filter(|r| matches!(r.selection, WorktreeSelection::Existing(_)))
            .count();
        assert_eq!(existing_count, 2);

        update_dashboard(&mut model, key(KeyCode::Char(' '))); // check the focused row
        assert_eq!(model.checked.len(), 1);
        let checked_before = model.checked.clone();
        let selected_before = model.worktrees.selected_index();

        update_dashboard(&mut model, key(KeyCode::Tab)); // -> Repos
        update_dashboard(&mut model, key(KeyCode::Down)); // move the repo cursor

        assert_eq!(
            model.checked, checked_before,
            "repo-cursor movement must not touch the checked set"
        );
        assert_eq!(
            model.worktrees.selected_index(),
            selected_before,
            "repo-cursor movement must not reset the worktree pane's selection"
        );
        // ...but the synthetic "+ new worktree" row now targets the newly
        // selected repo.
        let new_row = model
            .worktrees
            .items
            .iter()
            .find(|r| matches!(r.selection, WorktreeSelection::New))
            .expect("Browse with a selected repo offers a + new worktree row");
        assert_eq!(new_row.repo_label, "acme/second");
    }

    #[test]
    fn refresh_worktree_pane_preserves_selection_by_path() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
            scanned_entry("acme/proj", "three"),
        ]);
        update_dashboard(&mut model, key(KeyCode::Down)); // focus row "two"
        let before = focused_path(&model);

        // Simulate an `r` rescan resolving — same entries, reordered, as a
        // fresh scan off disk would return.
        model.apply_worktrees_load(Ok(load(vec![
            scanned_entry("acme/proj", "three"),
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
        ])));

        assert_eq!(
            focused_path(&model),
            before,
            "a rescan must keep the same row focused by path, not reset to row zero"
        );
    }

    // --- Existing rows resume under their own repo ------------------------

    #[test]
    fn enter_on_existing_row_uses_that_rows_repo_not_the_repo_pane_cursor() {
        // Repo pane cursor sits on acme/first (row 0); the only worktree
        // belongs to acme/second. Resuming it must never clone/pull
        // acme/first — the bug this pins: "cloning/pulling <some other
        // repo>…" (and its failure) on every resume.
        let mut model = browse_model(
            vec![("acme", "first"), ("acme", "second")],
            vec![scanned_entry("acme/second", "two")],
        );
        assert_eq!(
            model.repos.as_ref().unwrap().selected().unwrap().repo.name,
            "first"
        );

        match update_dashboard(&mut model, key(KeyCode::Enter)) {
            Some(DashboardOutcome::Suspend(SuspendReq::LaunchAgent { worktree_path, .. })) => {
                assert_eq!(
                    worktree_path,
                    PathBuf::from("/nonexistent-root/acme/second/two")
                );
            }
            other => panic!(
                "an existing worktree must launch directly, no clone/pull, got {}",
                describe(&other)
            ),
        }
        let pending = model.pending.as_ref().expect("pipeline in flight");
        assert_eq!(pending.ctx.repo_label, "acme/second");
        assert_eq!(pending.ctx.owner, "acme");
        assert_eq!(pending.ctx.name, "second");
        assert_eq!(
            pending.repo_root,
            PathBuf::from("/nonexistent-root/acme/second")
        );
    }

    #[test]
    fn enter_on_repo_row_starts_a_new_worktree_under_it() {
        let mut model = browse_model(vec![("acme", "proj")], vec![]);
        update_dashboard(&mut model, key(KeyCode::Tab));
        assert_eq!(model.focus, Focus::Repos);

        assert!(update_dashboard(&mut model, key(KeyCode::Enter)).is_none());
        let pending = model
            .pending
            .as_ref()
            .expect("Enter on a repo row must start the clone/pull + create pipeline");
        assert_eq!(pending.ctx.repo_label, "acme/proj");
        assert_eq!(pending.stage, super::super::model::Stage::Cloning);
        assert!(pending.worktree_path.is_none());
    }

    #[test]
    fn n_without_a_repo_to_create_under_explains_instead_of_failing() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        assert!(update_dashboard(&mut model, key(KeyCode::Char('n'))).is_none());
        assert!(model.pending.is_none());
        assert!(
            model.status.is_some(),
            "must tell the user why nothing happened"
        );
        assert_eq!(
            model.worktrees.query, "",
            "'n' with an empty filter is a command, not text"
        );
    }

    // --- Agent selection ---------------------------------------------------

    #[test]
    fn left_right_and_ctrl_a_select_agents_and_wrap() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        let start = model.agent_index; // "claude" (sorted first)
        assert_eq!(model.agents[start].name, "claude");

        update_dashboard(&mut model, key(KeyCode::Right));
        assert_eq!(model.agents[model.agent_index].name, "grok");
        update_dashboard(&mut model, key(KeyCode::Right));
        assert_eq!(model.agents[model.agent_index].name, "claude", "wraps");
        update_dashboard(&mut model, key(KeyCode::Left));
        assert_eq!(
            model.agents[model.agent_index].name, "grok",
            "wraps backwards"
        );
        update_dashboard(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        );
        assert_eq!(model.agents[model.agent_index].name, "claude");
        assert_eq!(
            model.worktrees.query, "",
            "agent keys never type into the filter"
        );
    }

    #[test]
    fn click_on_agent_segment_selects_it() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model
            .hotspots
            .borrow_mut()
            .push((Rect::new(10, 20, 6, 1), Action::SelectAgent(1)));

        assert!(update_dashboard(&mut model, click(12, 20)).is_none());
        assert_eq!(model.agents[model.agent_index].name, "grok");
    }

    // --- Space marks, never types --------------------------------------

    #[test]
    fn space_marks_when_query_is_empty() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        update_dashboard(&mut model, key(KeyCode::Char(' ')));
        assert_eq!(model.worktrees.query, "");
        assert_eq!(model.checked.len(), 1);
    }

    #[test]
    fn space_types_into_an_active_filter_instead_of_marking() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.worktrees.query.push_str("ab");
        model.worktrees.refilter(|r| r.filter_text.as_str());

        update_dashboard(&mut model, key(KeyCode::Char(' ')));
        assert_eq!(model.worktrees.query, "ab ");
        assert!(model.checked.is_empty());
    }

    #[test]
    fn click_on_mark_column_toggles_that_row() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
        ]);
        let second = PathBuf::from("/nonexistent-root/acme/proj/two");
        model
            .hotspots
            .borrow_mut()
            .push((Rect::new(1, 3, 5, 1), Action::ToggleMark(1)));

        assert!(update_dashboard(&mut model, click(2, 3)).is_none());
        assert!(model.checked.contains(&second));
        assert_eq!(
            model.worktrees.selected_index(),
            Some(1),
            "also focuses the row"
        );
        assert!(update_dashboard(&mut model, click(2, 3)).is_none());
        assert!(model.checked.is_empty(), "second click unmarks");
    }

    // --- Worktree pane rebuilds purely from work_cache, no I/O ----------

    #[test]
    fn worktree_pane_reflects_work_cache_with_no_live_io() {
        // Both paths are nonexistent — a real git read against either would
        // error. A passing assertion here is real evidence the pane never
        // calls one: only `work_cache` decides the STATUS cell.
        let mut model = all_worktrees_model(vec![]);
        let clean_entry = scanned_entry("acme/proj", "clean-one");
        let dirty_entry = scanned_entry("acme/proj", "dirty-one");
        let mut work = HashMap::new();
        work.insert(clean_entry.path.clone(), Some(WorkState::default()));
        work.insert(
            dirty_entry.path.clone(),
            Some(WorkState {
                changed_files: 3,
                unpushed_commits: 0,
            }),
        );
        update_dashboard(
            &mut model,
            Msg::WorktreesLoaded(Ok(super::super::msg::WorktreesLoad {
                entries: vec![clean_entry.clone(), dirty_entry.clone()],
                work,
            })),
        );

        let row_for = |model: &DashboardModel, path: &PathBuf| -> bool {
            model
                .worktrees
                .items
                .iter()
                .find(|r| matches!(&r.selection, WorktreeSelection::Existing(e) if &e.path == path))
                .expect("entry must be present")
                .state
                .is_none_or(|s| s.has_unsaved_work())
        };
        assert!(row_for(&model, &dirty_entry.path));
        assert!(!row_for(&model, &clean_entry.path));

        // A later background refresh flips one entry purely via
        // `Msg::WorkRefreshed` — still no I/O inside `update_dashboard`; a
        // failed read lands as "unknown", which counts as risky.
        update_dashboard(
            &mut model,
            Msg::WorkRefreshed(clean_entry.path.clone(), Err("boom".to_string())),
        );
        assert!(row_for(&model, &clean_entry.path));
    }

    #[test]
    fn checked_set_survives_work_refresh() {
        let entry = scanned_entry("acme/proj", "one");
        let mut model = all_worktrees_model(vec![entry.clone()]);
        update_dashboard(&mut model, key(KeyCode::Char(' ')));
        assert_eq!(model.checked.len(), 1);

        update_dashboard(
            &mut model,
            Msg::WorkRefreshed(entry.path.clone(), Ok(WorkState::default())),
        );
        assert!(model.checked.contains(&entry.path));
    }

    // --- Mark-and-delete via `d` ------------------------------------------

    #[test]
    fn d_opens_confirm_for_checked_set() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
        ]);
        update_dashboard(&mut model, key(KeyCode::Char(' ')));
        assert_eq!(model.checked.len(), 1);

        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        assert!(matches!(model.modal, Some(Modal::ConfirmDelete { .. })));

        // 'n' backs out without clearing the checked set — only Esc or an
        // actual confirm does that.
        update_dashboard(&mut model, key(KeyCode::Char('n')));
        assert!(model.modal.is_none());
        assert_eq!(model.checked.len(), 1);
    }

    #[test]
    fn d_with_nothing_checked_targets_focused_row() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
        ]);
        let focused = focused_path(&model);

        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        match &model.modal {
            Some(Modal::ConfirmDelete { targets, .. }) => {
                assert_eq!(targets.len(), 1);
                assert_eq!(targets[0].path, focused);
            }
            _ => panic!("expected d to open the confirm modal for the focused row"),
        }
    }

    #[test]
    fn d_gated_on_active_filter_types_instead_of_opening_confirm() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.worktrees.query.push_str("abc");
        model.worktrees.refilter(|r| r.filter_text.as_str());

        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        assert_eq!(model.worktrees.query, "abcd");
        assert!(model.modal.is_none());
    }

    #[test]
    fn confirm_keeps_risky_targets_until_f_includes_them() {
        // Nonexistent paths: `clean::remove_one` fails on the clean one,
        // which proves it was *attempted*; the risky one must never be
        // attempted at all until `f` flips the switch.
        let clean = scanned_entry("acme/proj", "clean");
        let risky = scanned_entry("acme/proj", "risky");
        let mut model = all_worktrees_model(vec![clean.clone(), risky.clone()]);
        model.work_cache.insert(
            risky.path.clone(),
            Some(WorkState {
                changed_files: 2,
                unpushed_commits: 1,
            }),
        );
        model.checked.insert(clean.path.clone());
        model.checked.insert(risky.path.clone());

        update_dashboard(&mut model, key(KeyCode::Char('d')));
        match &model.modal {
            Some(Modal::ConfirmDelete {
                targets,
                include_risky,
            }) => {
                assert_eq!(targets.len(), 2);
                assert!(
                    !include_risky,
                    "bare cw never starts on the destructive side"
                );
                assert_eq!(targets.iter().filter(|t| t.risky()).count(), 1);
            }
            _ => panic!("expected the confirm modal"),
        }

        update_dashboard(&mut model, key(KeyCode::Char('y')));
        // The clean one was attempted (and failed, nonexistent) → error
        // modal; the risky one was kept, untouched, and is reported.
        assert!(matches!(model.modal, Some(Modal::Error { .. })));
        assert!(model.status.as_deref().unwrap_or("").contains("kept 1"));
        assert!(
            model.checked.is_empty(),
            "confirm always clears the checked set"
        );
        update_dashboard(&mut model, key(KeyCode::Esc)); // dismiss the error

        // Second pass: mark only the risky one, include it explicitly.
        model.checked.insert(risky.path.clone());
        update_dashboard(&mut model, key(KeyCode::Char('d')));
        update_dashboard(&mut model, key(KeyCode::Char('f')));
        assert!(matches!(
            model.modal,
            Some(Modal::ConfirmDelete {
                include_risky: true,
                ..
            })
        ));
        update_dashboard(&mut model, key(KeyCode::Char('f')));
        assert!(matches!(
            model.modal,
            Some(Modal::ConfirmDelete {
                include_risky: false,
                ..
            })
        ));
        update_dashboard(&mut model, key(KeyCode::Char('f')));
        update_dashboard(&mut model, key(KeyCode::Enter));
        // Attempted now (and failed on the nonexistent path) — the point is
        // that it was no longer skipped.
        assert!(matches!(model.modal, Some(Modal::Error { .. })));
        assert!(!model.status.as_deref().unwrap_or("").contains("kept"));
    }

    #[test]
    fn f_is_a_no_op_when_nothing_is_risky() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        update_dashboard(&mut model, key(KeyCode::Char('d')));
        update_dashboard(&mut model, key(KeyCode::Char('f')));
        assert!(matches!(
            model.modal,
            Some(Modal::ConfirmDelete {
                include_risky: false,
                ..
            })
        ));
    }

    #[test]
    fn clean_force_starts_with_risky_included() {
        let mut model = all_worktrees_model_force(vec![scanned_entry("acme/proj", "one")], true);
        model.work_cache.insert(
            PathBuf::from("/nonexistent-root/acme/proj/one"),
            Some(WorkState {
                changed_files: 1,
                unpushed_commits: 0,
            }),
        );
        update_dashboard(&mut model, key(KeyCode::Char('d')));
        assert!(matches!(
            model.modal,
            Some(Modal::ConfirmDelete {
                include_risky: true,
                ..
            })
        ));
    }

    // --- Self-update: Msg::UpdateChecked and the `u` key ------------------

    #[test]
    fn update_checked_msg_sets_and_clears_update_available() {
        let mut model = all_worktrees_model(vec![]);
        update_dashboard(&mut model, Msg::UpdateChecked(Some("9.9.9".to_string())));
        assert_eq!(model.update_available, Some("9.9.9".to_string()));
        update_dashboard(&mut model, Msg::UpdateChecked(None));
        assert_eq!(model.update_available, None);
    }

    #[test]
    fn u_is_a_no_op_without_a_pending_update() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        assert!(update_dashboard(&mut model, key(KeyCode::Char('u'))).is_none());
        assert_eq!(model.worktrees.query, "u");
    }

    #[test]
    fn u_suspends_to_apply_update_when_one_is_pending() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.apply_update_checked(Some("9.9.9".to_string()));
        match update_dashboard(&mut model, key(KeyCode::Char('u'))) {
            Some(DashboardOutcome::Suspend(SuspendReq::ApplyUpdate)) => {}
            other => panic!("expected ApplyUpdate, got {}", describe(&other)),
        }
    }

    #[test]
    fn u_gated_on_active_filter_types_instead_of_suspending() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.apply_update_checked(Some("9.9.9".to_string()));
        model.worktrees.query.push_str("ab");
        model.worktrees.refilter(|r| r.filter_text.as_str());
        assert!(update_dashboard(&mut model, key(KeyCode::Char('u'))).is_none());
        assert_eq!(model.worktrees.query, "abu");
    }

    // --- The list stops going stale on create -----------------------------

    /// Drives a real `Scope::SingleRepo` (`cw scratch`) worktree-creation
    /// pipeline against a tempdir-backed git repo — `Scope::SingleRepo`'s
    /// "+ new worktree" selection reaches `Stage::CreatingWorktree` directly
    /// (no clone/pull stage, unlike `Scope::Browse`), so this is the cheapest
    /// real path to `DashboardModel::do_create_worktree` without a
    /// background clone thread.
    #[test]
    fn created_worktree_lands_in_all_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo = git2::Repository::init(&repo_root).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        std::fs::write(repo_root.join("README.md"), "hi").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("README.md")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }

        let consent_dir = tempfile::tempdir().expect("tempdir");
        let mut model = DashboardModel::new_single_repo(
            "acme/proj".to_string(),
            repo_root,
            dir.path().to_path_buf(),
            config_with_agents(&["claude"]),
            HookConsent::new(),
            consent_dir.path().join("hook-consent.json"),
            false,
            Some("feature".to_string()),
        );
        std::mem::forget(consent_dir);

        assert!(model.all_entries.is_empty());
        // `n` is the keyboard route to the same pipeline the "+ new
        // worktree" row and a repo-row Enter use.
        match update_dashboard(&mut model, key(KeyCode::Char('n'))) {
            Some(DashboardOutcome::Suspend(SuspendReq::LaunchAgent { .. })) => {}
            other => panic!(
                "expected the pipeline to reach a launch once the worktree was created, got {}",
                describe(&other)
            ),
        }

        assert_eq!(model.all_entries.len(), 1);
        let created = &model.all_entries[0];
        assert_eq!(created.repo, "acme/proj");
        assert_eq!(created.slug, "feature");
        assert!(created.path.join(".git").exists());
        assert_eq!(
            model.work_cache.get(&created.path),
            Some(&Some(WorkState::default())),
            "a just-created worktree is known clean without waiting for a rescan"
        );
    }

    // --- Hook-consent-once-per-repo ---------------------------------------

    #[test]
    fn hook_consent_declined_is_recorded_and_not_reprompted() {
        let mut model = all_worktrees_model(vec![]);
        model.pending = Some(super::super::model::PendingLaunch {
            ctx: super::super::model::LaunchContext {
                repo_label: "acme/proj".to_string(),
                owner: String::new(),
                name: String::new(),
                slug: "feature".to_string(),
                agent: "claude".to_string(),
            },
            repo_root: PathBuf::from("/nonexistent-root/acme/proj"),
            worktree_path: Some(PathBuf::from(
                "/nonexistent-root/acme/proj/.claude/worktrees/feature",
            )),
            stage: super::super::model::Stage::CloneHook,
            freshly_created: false,
        });
        model.modal = Some(Modal::HookConsent {
            resolved: crate::hooks::ResolvedHook {
                program: "true".to_string(),
                args: vec![],
                cwd: PathBuf::from("/nonexistent-root"),
            },
            kind: super::super::model::HookKind::Clone,
        });

        update_dashboard(&mut model, key(KeyCode::Char('n')));
        assert_eq!(model.hook_consent.get("acme/proj"), Some(&false));
        // Declining skips the hook and continues the pipeline; this fixture's
        // repo_root does not exist, so the next stage fails into Modal::Error
        // rather than leaving the dashboard blank. The consent modal itself
        // must be gone either way.
        assert!(
            !matches!(model.modal, Some(Modal::HookConsent { .. })),
            "declining must close the consent modal, got a still-open HookConsent"
        );

        // The "not reprompted" half: a second worktree from the SAME repo
        // hits `Stage::CloneHook`'s `checkpoint` again — this time it must
        // consult the map entry just recorded (`Some(false)` ->
        // `HookCheckpoint::Skip`) instead of opening a fresh
        // `Modal::HookConsent`. Driven via a real `Msg::CloneDone(Cloned)`
        // for a config that actually has a `post_clone_hook`.
        let consent_dir = tempfile::tempdir().expect("tempdir");
        let mut model = DashboardModel::new_all_worktrees(
            PathBuf::from("/nonexistent-root"),
            config_with_clone_hook(&["claude"]),
            model.hook_consent,
            consent_dir.path().join("hook-consent.json"),
            false,
            false,
        );
        std::mem::forget(consent_dir);
        model.pending = Some(super::super::model::PendingLaunch {
            ctx: super::super::model::LaunchContext {
                repo_label: "acme/proj".to_string(),
                owner: "acme".to_string(),
                name: "proj".to_string(),
                slug: "second".to_string(),
                agent: "claude".to_string(),
            },
            repo_root: PathBuf::from("/nonexistent-root/acme/proj"),
            worktree_path: None,
            stage: super::super::model::Stage::Cloning,
            freshly_created: false,
        });

        update_dashboard(
            &mut model,
            Msg::CloneDone(Ok(super::super::msg::CloneOutcome {
                repo_label: "acme/proj".to_string(),
                pull_outcome: crate::sync::PullOutcome::Cloned,
            })),
        );
        assert!(
            !matches!(model.modal, Some(Modal::HookConsent { .. })),
            "a repo with a recorded decline must not reopen Modal::HookConsent on a later checkpoint"
        );
        assert_eq!(model.hook_consent.get("acme/proj"), Some(&false));
    }

    // --- PendingLaunch stage advancement on a resumed hook -----------------

    #[test]
    fn resume_after_hook_ok_advances_stage_and_continues_the_pipeline() {
        let mut model = all_worktrees_model(vec![]);
        model.pending = Some(super::super::model::PendingLaunch {
            ctx: super::super::model::LaunchContext {
                repo_label: "acme/proj".to_string(),
                owner: String::new(),
                name: String::new(),
                slug: "feature".to_string(),
                agent: "claude".to_string(),
            },
            repo_root: PathBuf::from("/nonexistent-root/acme/proj"),
            worktree_path: Some(PathBuf::from(
                "/nonexistent-root/acme/proj/.claude/worktrees/feature",
            )),
            stage: super::super::model::Stage::CreateHook,
            freshly_created: true,
        });

        let outcome = model.resume_after_hook(super::super::model::HookKind::Create, Ok(()));
        assert!(model.status.is_none());
        match outcome {
            Some(DashboardOutcome::Suspend(SuspendReq::LaunchAgent { .. })) => {}
            other => panic!(
                "CreateHook -> Ok must suspend into a launch, got {}",
                describe(&other)
            ),
        }
    }

    #[test]
    fn resume_after_hook_err_records_status_but_still_advances() {
        let mut model = all_worktrees_model(vec![]);
        model.pending = Some(super::super::model::PendingLaunch {
            ctx: super::super::model::LaunchContext {
                repo_label: "acme/proj".to_string(),
                owner: String::new(),
                name: String::new(),
                slug: "feature".to_string(),
                agent: "claude".to_string(),
            },
            repo_root: PathBuf::from("/nonexistent-root/acme/proj"),
            worktree_path: Some(PathBuf::from(
                "/nonexistent-root/acme/proj/.claude/worktrees/feature",
            )),
            stage: super::super::model::Stage::CreateHook,
            freshly_created: true,
        });

        let outcome = model.resume_after_hook(
            super::super::model::HookKind::Create,
            Err("permission denied".to_string()),
        );
        assert_eq!(
            model.status.as_deref(),
            Some("hook failed: permission denied")
        );
        // Warn-and-continue: a failed setup script must not block getting
        // into the agent session.
        match outcome {
            Some(DashboardOutcome::Suspend(SuspendReq::LaunchAgent { .. })) => {}
            other => panic!(
                "a failed hook must still advance the pipeline, got {}",
                describe(&other)
            ),
        }
    }

    // --- Mouse: click focuses, double-click opens ---------------------------

    #[test]
    fn mouse_click_focuses_worktree_row_and_double_click_opens_it() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
            scanned_entry("acme/proj", "three"),
        ]);
        model.worktrees.table_rect.set(Rect::new(0, 0, 40, 4)); // header + 3 rows

        // Row 2 is the second visible row (row 1 is the header).
        assert!(
            update_dashboard(&mut model, click(2, 2)).is_none(),
            "a single click only focuses"
        );
        assert_eq!(model.worktrees.selected_index(), Some(1));
        assert_eq!(model.focus, Focus::Worktrees);

        // A different row right after: still a single click.
        assert!(update_dashboard(&mut model, click(2, 3)).is_none());
        assert_eq!(model.worktrees.selected_index(), Some(2));

        // Same row again within the window: an open.
        match update_dashboard(&mut model, click(2, 3)) {
            Some(DashboardOutcome::Suspend(SuspendReq::LaunchAgent { worktree_path, .. })) => {
                assert_eq!(
                    worktree_path,
                    PathBuf::from("/nonexistent-root/acme/proj/three")
                );
            }
            other => panic!("a double-click must open the row, got {}", describe(&other)),
        }
    }

    #[test]
    fn click_inside_pane_but_off_any_row_only_focuses_the_pane() {
        let mut model = browse_model(
            vec![("acme", "proj")],
            vec![scanned_entry("acme/proj", "one")],
        );
        model
            .repos
            .as_ref()
            .unwrap()
            .pane_rect
            .set(Rect::new(0, 0, 30, 20));
        model
            .repos
            .as_ref()
            .unwrap()
            .table_rect
            .set(Rect::new(1, 1, 28, 18));
        model.worktrees.pane_rect.set(Rect::new(30, 0, 40, 20));
        model.worktrees.table_rect.set(Rect::new(31, 1, 38, 18));
        assert_eq!(model.focus, Focus::Worktrees);

        // Below the only repo row, inside the repo pane.
        assert!(update_dashboard(&mut model, click(5, 15)).is_none());
        assert_eq!(model.focus, Focus::Repos);
        assert_eq!(
            model.repos.as_ref().unwrap().selected_index(),
            Some(0),
            "selection untouched"
        );
    }

    #[test]
    fn clicks_while_a_modal_is_open_only_reach_the_modal_buttons() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.worktrees.table_rect.set(Rect::new(0, 0, 40, 4));
        update_dashboard(&mut model, key(KeyCode::Char('d')));
        assert!(matches!(model.modal, Some(Modal::ConfirmDelete { .. })));
        model
            .hotspots
            .borrow_mut()
            .push((Rect::new(5, 5, 8, 1), Action::ModalCancel));
        // Would be a row click without a modal; must be ignored.
        assert!(update_dashboard(&mut model, click(2, 2)).is_none());
        assert!(matches!(model.modal, Some(Modal::ConfirmDelete { .. })));
        // The modal's own cancel button works.
        assert!(update_dashboard(&mut model, click(6, 5)).is_none());
        assert!(model.modal.is_none());
    }

    // --- PendingLaunch stage advancement on synthetic CloneDone -----------

    #[test]
    fn clone_done_up_to_date_skips_clone_hook_goes_to_launching() {
        let mut model = all_worktrees_model(vec![]);
        model.pending = Some(super::super::model::PendingLaunch {
            ctx: super::super::model::LaunchContext {
                repo_label: "acme/proj".to_string(),
                owner: "acme".to_string(),
                name: "proj".to_string(),
                slug: "feature".to_string(),
                agent: "claude".to_string(),
            },
            repo_root: PathBuf::from("/nonexistent-root/acme/proj"),
            worktree_path: Some(PathBuf::from(
                "/nonexistent-root/acme/proj/.claude/worktrees/feature",
            )),
            stage: super::super::model::Stage::Cloning,
            freshly_created: false,
        });

        let outcome = update_dashboard(
            &mut model,
            Msg::CloneDone(Ok(super::super::msg::CloneOutcome {
                repo_label: "acme/proj".to_string(),
                pull_outcome: crate::sync::PullOutcome::UpToDate,
            })),
        );
        match outcome {
            Some(DashboardOutcome::Suspend(SuspendReq::LaunchAgent { .. })) => {}
            other => panic!(
                "an UpToDate pull with a known worktree_path must launch, got {}",
                describe(&other)
            ),
        }
    }

    #[test]
    fn clone_done_failure_clears_pending_and_opens_error_modal() {
        let mut model = all_worktrees_model(vec![]);
        model.pending = Some(super::super::model::PendingLaunch {
            ctx: super::super::model::LaunchContext {
                repo_label: "acme/proj".to_string(),
                owner: "acme".to_string(),
                name: "proj".to_string(),
                slug: "feature".to_string(),
                agent: "claude".to_string(),
            },
            repo_root: PathBuf::from("/nonexistent-root/acme/proj"),
            worktree_path: None,
            stage: super::super::model::Stage::Cloning,
            freshly_created: false,
        });

        let outcome = update_dashboard(
            &mut model,
            Msg::CloneDone(Err(
                "Cloning into '/x'...\nremote: Repository not found.".to_string()
            )),
        );
        assert!(outcome.is_none());
        assert!(model.pending.is_none());
        match &model.modal {
            Some(Modal::Error { title, detail }) => {
                assert!(title.contains("acme/proj"));
                assert!(
                    detail.contains("Repository not found"),
                    "the full multi-line output survives"
                );
            }
            _ => panic!("a clone failure must open the error modal, not a one-line status"),
        }
        // Enter dismisses it, and the dashboard is usable again.
        update_dashboard(&mut model, key(KeyCode::Enter));
        assert!(model.modal.is_none());
    }

    #[test]
    fn launch_failure_is_reported_not_fatal() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.pending = Some(super::super::model::PendingLaunch {
            ctx: super::super::model::LaunchContext {
                repo_label: "acme/proj".to_string(),
                owner: String::new(),
                name: String::new(),
                slug: "one".to_string(),
                agent: "grok".to_string(),
            },
            repo_root: PathBuf::from("/nonexistent-root/acme/proj"),
            worktree_path: Some(PathBuf::from("/nonexistent-root/acme/proj/one")),
            stage: super::super::model::Stage::Launching,
            freshly_created: false,
        });
        model.resume_after_launch();
        model.report_launch_failure("grok", "launching 'grok' — is it on PATH?".to_string());
        assert!(model.pending.is_none());
        assert!(matches!(&model.modal, Some(Modal::Error { title, .. }) if title.contains("grok")));
    }

    // --- start_pending must not clobber an already in-flight pending ------

    #[test]
    fn enter_while_pending_in_flight_does_not_overwrite_it() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/first", "one"),
            scanned_entry("acme/second", "two"),
        ]);
        model.pending = Some(super::super::model::PendingLaunch {
            ctx: super::super::model::LaunchContext {
                repo_label: "acme/inflight".to_string(),
                owner: String::new(),
                name: String::new(),
                slug: "inflight".to_string(),
                agent: "claude".to_string(),
            },
            repo_root: PathBuf::from("/nonexistent-root/acme/inflight"),
            worktree_path: None,
            stage: super::super::model::Stage::Cloning,
            freshly_created: false,
        });

        model.worktrees.move_selection(1);
        let outcome = update_dashboard(&mut model, key(KeyCode::Enter));
        assert!(outcome.is_none(), "got {}", describe(&outcome));
        let pending = model
            .pending
            .as_ref()
            .expect("the in-flight pipeline must survive");
        assert_eq!(pending.ctx.repo_label, "acme/inflight");
        assert_eq!(pending.ctx.slug, "inflight");
    }

    fn describe(outcome: &Option<DashboardOutcome>) -> &'static str {
        match outcome {
            Some(DashboardOutcome::Cancelled) => "Cancelled",
            Some(DashboardOutcome::Suspend(_)) => "Suspend",
            None => "None",
        }
    }
}
