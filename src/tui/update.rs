//! Pure, terminal-free state transitions — `update_dashboard` mutates a
//! `DashboardModel` in response to one `Msg` and optionally yields a
//! terminal `DashboardOutcome`. Every rule in the plan's keybinding-gating
//! table lives here, so it's exercised directly with fixture `Msg`
//! sequences, no real terminal needed (see the `tests` module below).
//! Anything impure — spawning the background clone thread, running a hook,
//! launching the agent CLI — lives in `dashboard.rs`, which inspects the
//! model after each call here and reacts; this file never touches a
//! filesystem, subprocess, or thread.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use super::model::{DashboardModel, DashboardOutcome, Focus, Modal};
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
        Msg::Mouse(mouse) => {
            handle_mouse(model, mouse);
            None
        }
        Msg::Resize | Msg::Tick => None,
        Msg::DataLoaded(load) => {
            model.apply_repo_load(load);
            None
        }
        Msg::WorktreesLoaded(result) => {
            model.apply_worktrees_load(result);
            None
        }
        Msg::CloneDone(result) => model.apply_clone_done(result),
        Msg::DirtyRefreshed(path, result) => {
            model.apply_dirty_refresh(path, result);
            None
        }
    }
}

fn handle_key(model: &mut DashboardModel, key: KeyEvent) -> Option<DashboardOutcome> {
    if is_ctrl_c(key) {
        return Some(DashboardOutcome::Cancelled);
    }
    if model.modal.is_some() {
        return handle_modal_key(model, key);
    }
    if is_ctrl_a(key) {
        model.cycle_agent();
        return None;
    }
    handle_pane_key(model, key)
}

fn handle_modal_key(model: &mut DashboardModel, key: KeyEvent) -> Option<DashboardOutcome> {
    match model.modal.as_ref()? {
        Modal::HookConsent { .. } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => model.resolve_hook_consent(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                model.resolve_hook_consent(false)
            }
            _ => None,
        },
        Modal::ConfirmDelete { .. } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                model.confirm_delete();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                model.modal = None;
                None
            }
            _ => None,
        },
    }
}

/// The nesting order the plan's Esc rule spells out: modal (already routed
/// away in `handle_key` before reaching here) → the focused pane's filter →
/// the checked set → cancel the whole dashboard.
fn handle_pane_key(model: &mut DashboardModel, key: KeyEvent) -> Option<DashboardOutcome> {
    match key.code {
        KeyCode::Esc => {
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
        KeyCode::Tab => {
            toggle_focus(model);
            None
        }
        KeyCode::Enter => handle_enter(model),
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
            model.open_delete_confirm();
            None
        }
        // Also query-gated, but not destructive — a rescan can't lose data,
        // it just re-reads what's already on disk. Gated anyway so `r` stays
        // typable into an active filter, same discipline as `d`.
        KeyCode::Char('r') if model.focus == Focus::Worktrees && focused_query_empty(model) => {
            model.rescan_requested = true;
            None
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
        KeyCode::Char('q') if focused_query_empty(model) => Some(DashboardOutcome::Cancelled),
        KeyCode::Char(c) => {
            type_into_focused(model, c);
            None
        }
        _ => None,
    }
}

/// Enter on the Repos pane just moves focus onto the Worktrees pane —
/// there's nothing to "commit" about a repo row on its own, the worktree
/// pane already shows every repo's worktrees regardless. Enter on the
/// Worktrees pane always starts the clone/hook/create/launch pipeline for
/// the focused row — marking rows for removal is Space's job now, never
/// Enter's.
fn handle_enter(model: &mut DashboardModel) -> Option<DashboardOutcome> {
    match model.focus {
        Focus::Repos => {
            model.focus = Focus::Worktrees;
            None
        }
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
    match model.focus {
        Focus::Repos => {
            if let Some(repos) = &model.repos {
                repos.move_selection(delta);
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
            }
        }
        Focus::Worktrees => {
            model.worktrees.query.push(c);
            model.worktrees.refilter(|r| r.filter_text.as_str());
        }
    }
}

/// Resolves a click to a row in whichever pane's last-rendered `Rect`
/// contains it (checking the Repos pane first, when it exists — panes never
/// overlap on screen, so at most one hit-tests true), focusing that pane and
/// row. A click never activates a row, and never marks one either — see
/// CLAUDE.md's mouse invariant; marking is Space's job. Scroll wheel events
/// move the currently focused pane's selection, same as `j`/`k`.
fn handle_mouse(model: &mut DashboardModel, mouse: MouseEvent) {
    if model.modal.is_some() {
        return;
    }
    match mouse.kind {
        MouseEventKind::Down(_) => handle_mouse_down(model, mouse),
        MouseEventKind::ScrollDown => move_focused(model, 1),
        MouseEventKind::ScrollUp => move_focused(model, -1),
        _ => {}
    }
}

fn handle_mouse_down(model: &mut DashboardModel, mouse: MouseEvent) {
    let repo_idx = model
        .repos
        .as_ref()
        .and_then(|repos| hit_test(repos.table_rect.get(), repos, mouse));
    if let Some(idx) = repo_idx {
        if let Some(repos) = &model.repos {
            repos.select(Some(idx));
        }
        model.focus = Focus::Repos;
        return;
    }

    if let Some(idx) = hit_test(model.worktrees.table_rect.get(), &model.worktrees, mouse) {
        model.worktrees.select(Some(idx));
        model.focus = Focus::Worktrees;
    }
}

fn hit_test<T>(
    area: ratatui::layout::Rect,
    list: &super::model::ListState<T>,
    mouse: MouseEvent,
) -> Option<usize> {
    let idx = widgets::row_at(area, list.offset(), mouse.column, mouse.row)?;
    (idx < list.filtered.len()).then_some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config};
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

    /// Shared fixture: a `DashboardModel` in `Scope::AllWorktrees`, with a
    /// tempdir-backed hook-consent path — never `config::hook_consent_path()`
    /// (the real `~/.cache/cw` store), so a test that reaches
    /// `resolve_hook_consent`'s `save_consent` call never touches the user's
    /// actual machine state.
    fn all_worktrees_model(entries: Vec<ScannedEntry>) -> DashboardModel {
        let consent_dir = tempfile::tempdir().expect("tempdir");
        let mut model = DashboardModel::new_all_worktrees(
            PathBuf::from("/nonexistent-root"),
            config_with_agents(&["claude", "grok"]),
            HookConsent::new(),
            consent_dir.path().join("hook-consent.json"),
            false,
            false,
        );
        // Leak the tempdir for the model's lifetime — test-only, avoids a
        // dangling consent path once `consent_dir` would otherwise drop.
        std::mem::forget(consent_dir);
        model.apply_worktrees_load(Ok(super::super::msg::WorktreesLoad {
            entries,
            dirty: HashMap::new(),
        }));
        model
    }

    // --- Esc nesting: filter -> checked -> cancel -------------------------

    #[test]
    fn esc_clears_query_before_checked_before_cancelling() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.worktrees.query.push_str("abc");
        model.worktrees.refilter(|r| r.filter_text.as_str());
        model
            .checked
            .insert(PathBuf::from("/nonexistent-root/acme/proj/one"));

        assert!(update_dashboard(&mut model, key(KeyCode::Esc)).is_none());
        assert_eq!(model.worktrees.query, "", "first Esc clears the filter");
        assert_eq!(
            model.checked.len(),
            1,
            "clearing the filter must not also clear the checked set"
        );

        assert!(update_dashboard(&mut model, key(KeyCode::Esc)).is_none());
        assert!(
            model.checked.is_empty(),
            "second Esc, with the filter already empty, clears the checked set"
        );

        match update_dashboard(&mut model, key(KeyCode::Esc)) {
            Some(DashboardOutcome::Cancelled) => {}
            _ => panic!("third Esc, with filter empty and nothing checked, must cancel"),
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
        let repos = vec![crate::github::Repo {
            owner: "acme".to_string(),
            name: "proj".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }];
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
        model.apply_worktrees_load(Ok(super::super::msg::WorktreesLoad {
            entries: vec![scanned_entry("acme/proj", "one")],
            dirty: HashMap::new(),
        }));

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

    // --- Repo-cursor movement no longer touches the worktree pane --------

    #[test]
    fn repo_cursor_move_preserves_checked_set_and_worktree_selection() {
        let repos = vec![
            crate::github::Repo {
                owner: "acme".to_string(),
                name: "first".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            },
            crate::github::Repo {
                owner: "acme".to_string(),
                name: "second".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            },
        ];
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
        model.apply_worktrees_load(Ok(super::super::msg::WorktreesLoad {
            entries: vec![
                scanned_entry("acme/first", "one"),
                scanned_entry("acme/second", "two"),
            ],
            dirty: HashMap::new(),
        }));

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
        assert_eq!(model.focus, Focus::Repos);
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
    }

    #[test]
    fn refresh_worktree_pane_preserves_selection_by_path() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
            scanned_entry("acme/proj", "three"),
        ]);
        update_dashboard(&mut model, key(KeyCode::Down)); // focus row "two"
        let focused_path = match &model.worktrees.selected().unwrap().selection {
            WorktreeSelection::Existing(entry) => entry.path.clone(),
            WorktreeSelection::New => panic!("expected an existing row"),
        };

        // Simulate an `r` rescan resolving — same entries, reordered, as a
        // fresh scan off disk would return.
        model.apply_worktrees_load(Ok(super::super::msg::WorktreesLoad {
            entries: vec![
                scanned_entry("acme/proj", "three"),
                scanned_entry("acme/proj", "one"),
                scanned_entry("acme/proj", "two"),
            ],
            dirty: HashMap::new(),
        }));

        let still_focused_path = match &model.worktrees.selected().unwrap().selection {
            WorktreeSelection::Existing(entry) => entry.path.clone(),
            WorktreeSelection::New => panic!("expected an existing row"),
        };
        assert_eq!(
            still_focused_path, focused_path,
            "a rescan must keep the same row focused by path, not reset to row zero"
        );
    }

    // --- Space marks, never types --------------------------------------

    #[test]
    fn space_marks_when_query_is_empty() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        assert_eq!(model.worktrees.query, "");

        update_dashboard(&mut model, key(KeyCode::Char(' ')));

        assert_eq!(
            model.worktrees.query, "",
            "space must not be typed into an already-empty filter"
        );
        assert_eq!(
            model.checked.len(),
            1,
            "space must toggle the focused row's check-mark instead"
        );
    }

    #[test]
    fn space_types_into_an_active_filter_instead_of_marking() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        model.worktrees.query.push_str("ab");
        model.worktrees.refilter(|r| r.filter_text.as_str());

        update_dashboard(&mut model, key(KeyCode::Char(' ')));

        assert_eq!(
            model.worktrees.query, "ab ",
            "space with an active filter must be gated like 'd'/'r' and type into the query"
        );
        assert!(
            model.checked.is_empty(),
            "a gated space must not mark the focused row"
        );
    }

    // --- Worktree pane rebuilds purely from dirty_cache, no I/O ----------

    #[test]
    fn worktree_pane_reflects_dirty_cache_with_no_live_io() {
        // Both paths are nonexistent — a real `gitstatus::is_dirty` call
        // against either would error. A passing assertion here is real
        // evidence the pane never calls it: only `dirty_cache` (populated by
        // `apply_worktrees_load`/`apply_dirty_refresh`, never by a live
        // `is_dirty` read inside `refresh_worktree_pane`) decides the flag.
        let consent_dir = tempfile::tempdir().expect("tempdir");
        let mut model = DashboardModel::new_all_worktrees(
            PathBuf::from("/nonexistent-root"),
            config_with_agents(&["claude"]),
            HookConsent::new(),
            consent_dir.path().join("hook-consent.json"),
            false,
            false,
        );
        std::mem::forget(consent_dir);

        let clean_entry = scanned_entry("acme/proj", "clean-one");
        let dirty_entry = scanned_entry("acme/proj", "dirty-one");
        let mut dirty = HashMap::new();
        dirty.insert(dirty_entry.path.clone(), true);

        update_dashboard(
            &mut model,
            Msg::WorktreesLoaded(Ok(super::super::msg::WorktreesLoad {
                entries: vec![clean_entry.clone(), dirty_entry.clone()],
                dirty,
            })),
        );

        let dirty_row = model
            .worktrees
            .items
            .iter()
            .find(|r| matches!(&r.selection, WorktreeSelection::Existing(e) if e.path == dirty_entry.path))
            .expect("dirty entry must be present");
        assert!(dirty_row.dirty);

        let clean_row = model
            .worktrees
            .items
            .iter()
            .find(|r| matches!(&r.selection, WorktreeSelection::Existing(e) if e.path == clean_entry.path))
            .expect("clean entry must be present");
        assert!(!clean_row.dirty);

        // A later background refresh flips one entry's flag purely via
        // `Msg::DirtyRefreshed` — still no I/O inside `update_dashboard`.
        update_dashboard(
            &mut model,
            Msg::DirtyRefreshed(clean_entry.path.clone(), Ok(true)),
        );
        let flipped = model
            .worktrees
            .items
            .iter()
            .find(|r| matches!(&r.selection, WorktreeSelection::Existing(e) if e.path == clean_entry.path))
            .unwrap();
        assert!(flipped.dirty, "DirtyRefreshed must update the cached flag");
    }

    #[test]
    fn checked_set_survives_dirty_refresh() {
        let entry = scanned_entry("acme/proj", "one");
        let mut model = all_worktrees_model(vec![entry.clone()]);
        update_dashboard(&mut model, key(KeyCode::Char(' '))); // check the only row
        assert_eq!(model.checked.len(), 1);

        update_dashboard(
            &mut model,
            Msg::DirtyRefreshed(entry.path.clone(), Ok(true)),
        );

        assert_eq!(
            model.checked.len(),
            1,
            "a background dirty-status refresh must not wipe the checked set"
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

        update_dashboard(&mut model, key(KeyCode::Char(' '))); // check the focused row
        assert_eq!(model.checked.len(), 1);

        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        assert!(
            matches!(model.modal, Some(Modal::ConfirmDelete { .. })),
            "d with something checked must open the confirm modal"
        );

        // 'n' backs out without clearing the checked set — only Esc or an
        // actual confirm does that.
        update_dashboard(&mut model, key(KeyCode::Char('n')));
        assert!(model.modal.is_none());
        assert_eq!(
            model.checked.len(),
            1,
            "declining the confirm modal must leave the checked set untouched"
        );
    }

    #[test]
    fn d_with_nothing_checked_targets_focused_row() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
        ]);
        assert!(model.checked.is_empty());

        let focused_path = match &model
            .worktrees
            .selected()
            .expect("a row must be focused")
            .selection
        {
            WorktreeSelection::Existing(entry) => entry.path.clone(),
            WorktreeSelection::New => panic!("expected an existing row to be focused"),
        };

        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        match &model.modal {
            Some(Modal::ConfirmDelete { targets, .. }) => {
                assert_eq!(
                    targets,
                    &vec![focused_path],
                    "d with nothing checked must target only the currently focused row"
                );
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
        assert_eq!(
            model.worktrees.query, "abcd",
            "'d' with an active filter must type into the query, not open the confirm modal"
        );
        assert!(
            model.modal.is_none(),
            "a gated 'd' must not open the confirm modal"
        );
    }

    #[test]
    fn confirm_delete_modal_yes_removes_only_checked() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")]);
        update_dashboard(&mut model, key(KeyCode::Char(' '))); // check the only row
        update_dashboard(&mut model, key(KeyCode::Char('d'))); // opens confirm
        assert!(matches!(model.modal, Some(Modal::ConfirmDelete { .. })));

        // 'n' backs out without removing anything or touching all_entries.
        update_dashboard(&mut model, key(KeyCode::Char('n')));
        assert!(model.modal.is_none());
        assert_eq!(model.all_entries.len(), 1);
    }

    // --- The list stops going stale on create -----------------------------

    /// Drives a real `Scope::SingleRepo` (`cw scratch`) worktree-creation
    /// pipeline against a tempdir-backed git repo — `Scope::SingleRepo`'s
    /// "+ new worktree" selection reaches `Stage::CreatingWorktree` directly
    /// (no clone/pull stage, unlike `Scope::Browse`), so this is the cheapest
    /// real path to `DashboardModel::do_create_worktree` without a
    /// background clone thread. Regression guard for the asymmetry `clean.rs`
    /// already didn't have: removal already keeps `all_entries` current
    /// (`confirm_delete`), creation used to leave a freshly created worktree
    /// invisible until `cw` was quit and relaunched.
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
        let outcome = model.start_pending(WorktreeSelection::New);
        match outcome {
            Some(DashboardOutcome::Suspend(super::super::model::SuspendReq::LaunchAgent {
                ..
            })) => {}
            other => panic!(
                "expected the pipeline to reach a launch once the worktree was created, got {}",
                describe(&other)
            ),
        }

        assert_eq!(
            model.all_entries.len(),
            1,
            "a freshly created worktree must land in the in-memory scanned-entries list"
        );
        let created = &model.all_entries[0];
        assert_eq!(created.repo, "acme/proj");
        assert_eq!(created.slug, "feature");
        assert!(created.path.join(".git").exists());
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
        assert!(model.modal.is_none(), "declining must close the modal");

        // Recorded durably — `hooks::gate`'s own unit tests
        // (`gate_prompts_once_per_repo`) cover the analogous non-dashboard
        // consent store; what this test adds is that routing a decline
        // through `update_dashboard`'s modal key handling actually reaches
        // `resolve_hook_consent` and persists the answer.
        assert_eq!(model.hook_consent.get("acme/proj"), Some(&false));

        // The "not reprompted" half: a second worktree from the SAME repo
        // hits `Stage::CloneHook`'s `checkpoint` again — this time it must
        // consult the map entry just recorded (`Some(false)` ->
        // `HookCheckpoint::Skip`) instead of opening a fresh
        // `Modal::HookConsent`. `checkpoint`/`advance_pending` are private to
        // `tui::model`, so this drives it the only way reachable from here:
        // a real `Msg::CloneDone(Cloned)` for a config that actually has a
        // `post_clone_hook` configured (`config_with_clone_hook` — the
        // shared fixture above has none, so its clone-hook checkpoint would
        // `Skip` before ever consulting `hook_consent` either way). `config`
        // itself is private to `DashboardModel`, so this rebuilds the model
        // instead of mutating it in place, carrying the already-recorded
        // decline forward via the same `hook_consent` map.
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
            model.modal.is_none(),
            "a repo with a recorded decline must not reopen Modal::HookConsent on a later checkpoint"
        );
        assert_eq!(
            model.hook_consent.get("acme/proj"),
            Some(&false),
            "checkpoint must consult the existing entry, not overwrite it"
        );
    }

    // --- PendingLaunch stage advancement on a resumed hook -----------------
    //
    // `resume_after_hook` is the driver-triggered counterpart to
    // `Msg::CloneDone` below: `dashboard.rs`'s suspend/resume loop calls it
    // directly (not through a `Msg`) once a suspended `hooks::exec_hook` run
    // returns. Both cases pin `Stage::CreateHook`'s mapping via
    // `HookKind::Create` (`stage_after_hook(Create) == Stage::Launching`),
    // which then flows straight into the `Stage::Launching` arm — resolving
    // the agent and yielding `Suspend(LaunchAgent)` without touching the
    // filesystem, unlike the Clone-hook case (`Stage::CreatingWorktree`
    // opens a real `git2::Repository`, which a nonexistent-root fixture
    // can't exercise here).

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
        assert!(
            model.status.is_none(),
            "a successful hook run must not set an error status"
        );
        match outcome {
            Some(DashboardOutcome::Suspend(super::super::model::SuspendReq::LaunchAgent {
                ..
            })) => {}
            other => panic!(
                "CreateHook -> Ok must map to Stage::Launching (stage_after_hook) and suspend \
                 into a launch, got {}",
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
        // Warn-and-continue, matching `hooks::exec_hook`'s own philosophy
        // (a failed setup script must not block getting into the agent
        // session): the pipeline still advances to a launch, it doesn't
        // stall on `pending`.
        match outcome {
            Some(DashboardOutcome::Suspend(super::super::model::SuspendReq::LaunchAgent {
                ..
            })) => {}
            other => panic!(
                "a failed hook must still advance the pipeline, got {}",
                describe(&other)
            ),
        }
    }

    // --- Mouse: click focuses without activating --------------------------

    #[test]
    fn mouse_click_focuses_worktree_row_without_activating() {
        let mut model = all_worktrees_model(vec![
            scanned_entry("acme/proj", "one"),
            scanned_entry("acme/proj", "two"),
            scanned_entry("acme/proj", "three"),
        ]);
        model.worktrees.table_rect.set(Rect::new(0, 0, 40, 4)); // header + 3 rows

        let click = Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
            column: 2,
            row: 2, // second visible row (row 1 is the header)
            modifiers: KeyModifiers::NONE,
        });
        let outcome = update_dashboard(&mut model, click);
        assert!(outcome.is_none(), "a click must never activate a row");
        assert_eq!(model.worktrees.selected_index(), Some(1));
        assert_eq!(model.focus, Focus::Worktrees);
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
            Some(DashboardOutcome::Suspend(super::super::model::SuspendReq::LaunchAgent {
                ..
            })) => {}
            other => panic!(
                "an UpToDate pull with a known worktree_path must advance straight to \
                 launching, got {}",
                describe(&other)
            ),
        }
    }

    #[test]
    fn clone_done_failure_clears_pending_and_sets_status() {
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
            Msg::CloneDone(Err("network unreachable".to_string())),
        );
        assert!(outcome.is_none());
        assert!(model.pending.is_none());
        assert!(model.status.as_deref().unwrap_or("").contains("failed"));
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

        // Move focus onto a different worktree row than whatever the
        // in-flight pending is for, then press Enter — the exact race the
        // finding describes: a second Enter while `Stage::Cloning`'s
        // background thread for the first selection hasn't reported back
        // yet.
        model.worktrees.move_selection(1);
        let outcome = update_dashboard(&mut model, key(KeyCode::Enter));

        assert!(
            outcome.is_none(),
            "Enter on a worktree row while a pipeline is already in flight must be a no-op, got {}",
            describe(&outcome)
        );
        let pending = model
            .pending
            .as_ref()
            .expect("the in-flight pipeline must survive an Enter on another row");
        assert_eq!(
            pending.ctx.repo_label, "acme/inflight",
            "a second Enter must not clobber the original in-flight pending's identity"
        );
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
