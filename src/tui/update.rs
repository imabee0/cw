//! Pure, terminal-free state transitions — `update_repo`/`update_worktree`
//! mutate a `Model` in response to one `Msg` and optionally yield a terminal
//! `Outcome`. Every rule in the plan's Interaction model lives here, so it's
//! exercised directly with fixture `Msg` sequences, no real terminal needed
//! (see the `tests` module below).

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use super::model::{
    AgentModel, AgentOutcome, ListState, RepoModel, RepoOutcome, WorktreeMode, WorktreeModel,
    WorktreeOutcome,
};
use super::msg::Msg;
use super::widgets;
use crate::picker::{CleanCandidate, WorktreeSelection};

/// `PageUp`/`PageDown` scroll amount. Not derived from the last-rendered
/// table height (that's `Rect::default()` — height 0 — before the first
/// frame) so the very first keypress of a session isn't a no-op.
const PAGE: isize = 10;

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

// ---------------------------------------------------------------------
// Repo screen
// ---------------------------------------------------------------------

pub fn update_repo(model: &mut RepoModel, msg: Msg) -> Option<RepoOutcome> {
    match msg {
        Msg::Key(key) => handle_repo_key(model, key),
        Msg::Mouse(mouse) => {
            handle_list_mouse(&model.list, mouse);
            None
        }
        Msg::DataLoaded(load) => {
            model.apply_load(load);
            None
        }
        Msg::Resize | Msg::Tick => None,
    }
}

fn handle_repo_key(model: &mut RepoModel, key: KeyEvent) -> Option<RepoOutcome> {
    if is_ctrl_c(key) {
        return Some(RepoOutcome::Cancelled);
    }

    match key.code {
        KeyCode::Esc => {
            if !model.list.query.is_empty() {
                model.list.query.clear();
                model.list.refilter(|r| r.filter_text.as_str());
                return None;
            }
            return Some(RepoOutcome::Cancelled);
        }
        KeyCode::Enter => {
            if let Some(row) = model.list.selected() {
                return Some(RepoOutcome::Selected(row.repo.clone()));
            }
        }
        KeyCode::Up => model.list.move_selection(-1),
        KeyCode::Down => model.list.move_selection(1),
        KeyCode::PageUp => model.list.move_selection(-PAGE),
        KeyCode::PageDown => model.list.move_selection(PAGE),
        // `j`/`k` are unconditional navigation, never typable into the
        // filter — same deliberate trade-off the plan calls out for `q`
        // ("bound to quit only when the filter is empty, for the same
        // reason [as Ctrl-C]"): a fixed, small set of single-letter keys
        // stay control keys rather than searchable text.
        KeyCode::Char('j') => model.list.move_selection(1),
        KeyCode::Char('k') => model.list.move_selection(-1),
        KeyCode::Backspace if model.list.query.pop().is_some() => {
            model.list.refilter(|r| r.filter_text.as_str());
        }
        KeyCode::Char('q') if model.list.query.is_empty() => {
            return Some(RepoOutcome::Cancelled);
        }
        KeyCode::Char(c) => {
            model.list.query.push(c);
            model.list.refilter(|r| r.filter_text.as_str());
        }
        _ => {}
    }
    None
}

// ---------------------------------------------------------------------
// Agent-only screen (`picker::pick_agent`'s fallback — see its doc comment)
// ---------------------------------------------------------------------

pub fn update_agent(model: &mut AgentModel, msg: Msg) -> Option<AgentOutcome> {
    match msg {
        Msg::Key(key) => handle_agent_key(model, key),
        Msg::Mouse(mouse) => {
            handle_list_mouse(&model.list, mouse);
            None
        }
        Msg::Resize | Msg::Tick | Msg::DataLoaded(_) => None,
    }
}

fn handle_agent_key(model: &mut AgentModel, key: KeyEvent) -> Option<AgentOutcome> {
    if is_ctrl_c(key) {
        return Some(AgentOutcome::Cancelled);
    }

    match key.code {
        KeyCode::Esc => {
            if !model.list.query.is_empty() {
                model.list.query.clear();
                model.list.refilter(|a| a.filter_text.as_str());
                return None;
            }
            return Some(AgentOutcome::Cancelled);
        }
        KeyCode::Enter => {
            if let Some(row) = model.list.selected() {
                return Some(AgentOutcome::Selected(row.name.clone()));
            }
        }
        KeyCode::Up => model.list.move_selection(-1),
        KeyCode::Down => model.list.move_selection(1),
        KeyCode::PageUp => model.list.move_selection(-PAGE),
        KeyCode::PageDown => model.list.move_selection(PAGE),
        KeyCode::Char('j') => model.list.move_selection(1),
        KeyCode::Char('k') => model.list.move_selection(-1),
        KeyCode::Backspace if model.list.query.pop().is_some() => {
            model.list.refilter(|a| a.filter_text.as_str());
        }
        KeyCode::Char('q') if model.list.query.is_empty() => {
            return Some(AgentOutcome::Cancelled);
        }
        KeyCode::Char(c) => {
            model.list.query.push(c);
            model.list.refilter(|a| a.filter_text.as_str());
        }
        _ => {}
    }
    None
}

// ---------------------------------------------------------------------
// Worktree(+agent) screen
// ---------------------------------------------------------------------

pub fn update_worktree(model: &mut WorktreeModel, msg: Msg) -> Option<WorktreeOutcome> {
    match msg {
        Msg::Key(key) => handle_worktree_key(model, key),
        Msg::Mouse(mouse) => {
            handle_worktree_mouse(model, mouse);
            None
        }
        Msg::Resize | Msg::Tick | Msg::DataLoaded(_) => None,
    }
}

fn handle_worktree_key(model: &mut WorktreeModel, key: KeyEvent) -> Option<WorktreeOutcome> {
    if is_ctrl_c(key) {
        return Some(WorktreeOutcome::Cancelled);
    }

    let overlay_open = matches!(
        model.mode,
        WorktreeMode::Single {
            agent_overlay: true,
            ..
        }
    );
    if overlay_open {
        handle_agent_overlay_key(model, key)
    } else {
        handle_worktree_list_key(model, key)
    }
}

fn handle_worktree_list_key(model: &mut WorktreeModel, key: KeyEvent) -> Option<WorktreeOutcome> {
    let is_multi = matches!(model.mode, WorktreeMode::Multi);

    match key.code {
        KeyCode::Esc => {
            if !model.list.query.is_empty() {
                model.list.query.clear();
                model.list.refilter(|r| r.filter_text.as_str());
                return None;
            }
            return Some(WorktreeOutcome::Cancelled);
        }
        KeyCode::Enter => {
            // In multi-select mode Enter "activates the focused row" by
            // toggling its checkbox, same as Space/click — the batch
            // delete itself commits on `d` (see the Interaction-model
            // bullet on multi-select), not on Enter, so keyboard and mouse
            // stay in lockstep: neither a click nor an Enter ever deletes
            // by itself.
            if is_multi {
                toggle_focused(model);
                return None;
            }
            return enter_worktree_row(model);
        }
        KeyCode::Char(' ') if is_multi => toggle_focused(model),
        // Query-gated like `q` above (and for the same reason): unlike `q`,
        // an accidental match here is destructive, so it gets the stricter
        // treatment even though `q` itself only needs it for the "search
        // string can contain a letter" case.
        KeyCode::Char('d') if is_multi && model.list.query.is_empty() => {
            return commit_multi_delete(model)
        }
        KeyCode::Up => model.list.move_selection(-1),
        KeyCode::Down => model.list.move_selection(1),
        KeyCode::PageUp => model.list.move_selection(-PAGE),
        KeyCode::PageDown => model.list.move_selection(PAGE),
        KeyCode::Char('j') => model.list.move_selection(1),
        KeyCode::Char('k') => model.list.move_selection(-1),
        KeyCode::Backspace if model.list.query.pop().is_some() => {
            model.list.refilter(|r| r.filter_text.as_str());
        }
        KeyCode::Char('q') if model.list.query.is_empty() => {
            return Some(WorktreeOutcome::Cancelled);
        }
        KeyCode::Char(c) => {
            model.list.query.push(c);
            model.list.refilter(|r| r.filter_text.as_str());
        }
        _ => {}
    }
    None
}

/// Enter on a worktree row (single-select mode only — multi-select never
/// reaches this, see `handle_worktree_list_key`): finishes the screen
/// outright when no agent still needs picking, otherwise opens the inline
/// agent sub-panel instead of tearing this screen down and relaunching a
/// second one.
fn enter_worktree_row(model: &mut WorktreeModel) -> Option<WorktreeOutcome> {
    let selection = model.list.selected()?.selection.clone();
    match &mut model.mode {
        WorktreeMode::Single {
            agent_needed,
            agent_overlay,
            pending,
            ..
        } => {
            if *agent_needed {
                *pending = Some(selection);
                *agent_overlay = true;
                None
            } else {
                Some(WorktreeOutcome::Single {
                    selection,
                    agent: None,
                })
            }
        }
        WorktreeMode::Multi => unreachable!("multi-select Enter never reaches this function"),
    }
}

fn handle_agent_overlay_key(model: &mut WorktreeModel, key: KeyEvent) -> Option<WorktreeOutcome> {
    let WorktreeMode::Single {
        agents,
        agent_overlay,
        pending,
        ..
    } = &mut model.mode
    else {
        unreachable!("overlay_open is only ever true in Single mode");
    };

    match key.code {
        KeyCode::Esc => {
            if !agents.query.is_empty() {
                agents.query.clear();
                agents.refilter(|a| a.filter_text.as_str());
            } else {
                // Back out to the worktree list — not a full cancel. Esc's
                // "clear filter, else cancel" rule nests one level deeper
                // for a sub-panel than for the top-level screen.
                *agent_overlay = false;
            }
        }
        KeyCode::Enter => {
            if let Some(row) = agents.selected() {
                let selection = pending
                    .clone()
                    .expect("pending is set whenever the overlay opens");
                return Some(WorktreeOutcome::Single {
                    selection,
                    agent: Some(row.name.clone()),
                });
            }
        }
        KeyCode::Up => agents.move_selection(-1),
        KeyCode::Down => agents.move_selection(1),
        KeyCode::PageUp => agents.move_selection(-PAGE),
        KeyCode::PageDown => agents.move_selection(PAGE),
        KeyCode::Char('j') => agents.move_selection(1),
        KeyCode::Char('k') => agents.move_selection(-1),
        KeyCode::Backspace if agents.query.pop().is_some() => {
            agents.refilter(|a| a.filter_text.as_str());
        }
        KeyCode::Char('q') if agents.query.is_empty() => {
            return Some(WorktreeOutcome::Cancelled);
        }
        KeyCode::Char(c) => {
            agents.query.push(c);
            agents.refilter(|a| a.filter_text.as_str());
        }
        _ => {}
    }
    None
}

fn toggle_focused(model: &mut WorktreeModel) {
    let Some(filtered_pos) = model.list.selected_index() else {
        return;
    };
    let Some(&item_idx) = model.list.filtered.get(filtered_pos) else {
        return;
    };
    if !model.checked.remove(&item_idx) {
        model.checked.insert(item_idx);
    }
}

/// `d`: commits the current checked set as the Outcome. Mirrors
/// `picker.rs`'s old skim-backed behavior exactly: zero rows checked is
/// `Cancelled`, not an outcome with an empty `Vec` — `clean.rs::run_clean`
/// never sees a distinction between "cancelled" and "selected nothing".
/// Dirty rows are never excluded here — `dirty` rides along on each
/// `CleanCandidate` unchanged, and `clean.rs::run_clean`'s own
/// dirty/`--force` gate (unaffected by this rewrite) decides what actually
/// gets removed.
fn commit_multi_delete(model: &WorktreeModel) -> Option<WorktreeOutcome> {
    if model.checked.is_empty() {
        return Some(WorktreeOutcome::Cancelled);
    }
    let mut idxs: Vec<usize> = model.checked.iter().copied().collect();
    idxs.sort_unstable();

    let mut candidates: Vec<CleanCandidate> = Vec::with_capacity(idxs.len());
    for idx in idxs {
        if let Some(row) = model.list.items.get(idx) {
            if let WorktreeSelection::Existing(entry) = &row.selection {
                candidates.push(CleanCandidate {
                    entry: entry.clone(),
                    dirty: row.dirty,
                });
            }
        }
    }
    Some(WorktreeOutcome::Multi(candidates))
}

fn handle_worktree_mouse(model: &mut WorktreeModel, mouse: MouseEvent) {
    let overlay_open = matches!(
        model.mode,
        WorktreeMode::Single {
            agent_overlay: true,
            ..
        }
    );
    if overlay_open {
        if let WorktreeMode::Single { agents, .. } = &model.mode {
            handle_list_mouse(agents, mouse);
        }
        return;
    }

    let is_multi = matches!(model.mode, WorktreeMode::Multi);
    let focused = handle_list_mouse(&model.list, mouse);
    // Mouse click focuses only, never activates — except in multi-select
    // mode, where the plan makes a click "the mouse equivalent of Space":
    // it still only toggles a checkbox, it never deletes by itself.
    if is_multi && focused && matches!(mouse.kind, MouseEventKind::Down(_)) {
        toggle_focused(model);
    }
}

/// Resolves a click to a row and focuses it (never activates); scrolls move
/// the selection like `j`/`k`. Returns whether a click successfully focused
/// a row, so multi-select mode's click-also-toggles rule can key off it.
fn handle_list_mouse<T>(list: &ListState<T>, mouse: MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::Down(_) => {
            let area = list.table_rect.get();
            let offset = list.offset();
            match widgets::row_at(area, offset, mouse.column, mouse.row) {
                Some(idx) if idx < list.filtered.len() => {
                    list.select(Some(idx));
                    true
                }
                _ => false,
            }
        }
        MouseEventKind::ScrollDown => {
            list.move_selection(1);
            false
        }
        MouseEventKind::ScrollUp => {
            list.move_selection(-1);
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;
    use crate::worktree::WorktreeEntry as ScannedEntry;
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

    fn repo_model(names: &[&str]) -> RepoModel {
        let repos = names
            .iter()
            .map(|n| crate::github::Repo {
                owner: "acme".to_string(),
                name: (*n).to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .collect();
        RepoModel::new(repos, PathBuf::from("/nonexistent-root"))
    }

    fn scanned_entry(repo: &str, slug: &str) -> ScannedEntry {
        ScannedEntry {
            repo: repo.to_string(),
            slug: slug.to_string(),
            path: PathBuf::from(format!("/tmp/{repo}/{slug}")),
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

    // --- Filter-then-Esc / Ctrl-C ------------------------------------

    #[test]
    fn repo_filter_then_esc_clears_then_cancels() {
        let mut model = repo_model(&["alpha", "beta"]);
        assert!(update_repo(&mut model, key(KeyCode::Char('a'))).is_none());
        assert_eq!(model.list.query, "a");

        // First Esc: clears the filter, does not cancel.
        assert!(update_repo(&mut model, key(KeyCode::Esc)).is_none());
        assert_eq!(model.list.query, "");

        // Second Esc, with an empty filter: cancels.
        match update_repo(&mut model, key(KeyCode::Esc)) {
            Some(RepoOutcome::Cancelled) => {}
            _ => panic!("expected Cancelled on Esc with an empty filter"),
        }
    }

    #[test]
    fn repo_ctrl_c_cancels_immediately_with_filter_text_present() {
        let mut model = repo_model(&["alpha", "beta"]);
        update_repo(&mut model, key(KeyCode::Char('c')));
        assert_eq!(model.list.query, "c", "plain 'c' must still type");

        match update_repo(&mut model, ctrl_c()) {
            Some(RepoOutcome::Cancelled) => {}
            _ => panic!("Ctrl-C must cancel even with filter text present"),
        }
    }

    // --- Mouse hit-testing (row_at is covered directly in widgets.rs;
    //     this exercises it wired through update_repo's mouse handling) --

    #[test]
    fn repo_mouse_click_focuses_without_activating() {
        let mut model = repo_model(&["alpha", "beta", "gamma"]);
        model.list.table_rect.set(Rect::new(0, 0, 20, 4)); // header + 3 rows

        let click = Msg::Mouse(MouseEvent {
            kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
            column: 2,
            row: 2, // second visible row (row 1 is the header)
            modifiers: KeyModifiers::NONE,
        });
        let outcome = update_repo(&mut model, click);
        assert!(outcome.is_none(), "a click must never activate a row");
        assert_eq!(model.list.selected_index(), Some(1));
    }

    // --- Multi-select: Space toggles, dirty-without-force still returned -

    #[test]
    fn multi_select_space_toggles_checkbox() {
        let mut model = WorktreeModel::new_multi(
            vec![
                scanned_entry("acme/proj", "one"),
                scanned_entry("acme/proj", "two"),
            ],
            14,
        );
        assert!(model.checked.is_empty());
        update_worktree(&mut model, key(KeyCode::Char(' ')));
        assert_eq!(model.checked.len(), 1, "Space toggles the focused row on");
        update_worktree(&mut model, key(KeyCode::Char(' ')));
        assert!(model.checked.is_empty(), "Space toggles it back off");
    }

    #[test]
    fn multi_select_delete_types_into_active_filter_instead_of_deleting() {
        let mut model = WorktreeModel::new_multi(vec![scanned_entry("acme/proj", "one")], 14);
        update_worktree(&mut model, key(KeyCode::Char(' ')));
        assert_eq!(model.checked.len(), 1, "row is checked going in");

        model.list.query.push_str("abc");
        model.list.refilter(|r| r.filter_text.as_str());
        assert!(
            update_worktree(&mut model, key(KeyCode::Char('d'))).is_none(),
            "'d' with an active filter must type into the query, not delete"
        );
        assert_eq!(
            model.list.query, "abcd",
            "'d' must append to the filter, same as any other letter"
        );
        assert_eq!(
            model.checked.len(),
            1,
            "checked rows must survive an in-filter 'd' untouched"
        );
    }

    #[test]
    fn multi_select_delete_with_nothing_checked_cancels() {
        let mut model = WorktreeModel::new_multi(vec![scanned_entry("acme/proj", "one")], 14);
        match update_worktree(&mut model, key(KeyCode::Char('d'))) {
            Some(WorktreeOutcome::Cancelled) => {}
            _ => panic!("'d' with nothing checked must cancel, not return an empty Vec"),
        }
    }

    #[test]
    fn multi_select_delete_returns_dirty_flag_unfiltered() {
        // `dirty` here is set directly (bypassing `gitstatus::is_dirty`,
        // which needs a real repo) to drive the outcome-shape assertion:
        // the TUI itself must never drop/skip a dirty row — that gating
        // lives in `clean.rs::run_clean`, unchanged by this rewrite.
        let mut model = WorktreeModel::new_multi(vec![scanned_entry("acme/proj", "dirty-one")], 14);
        model.list.items[0].dirty = true;

        update_worktree(&mut model, key(KeyCode::Char(' ')));
        match update_worktree(&mut model, key(KeyCode::Char('d'))) {
            Some(WorktreeOutcome::Multi(candidates)) => {
                assert_eq!(candidates.len(), 1);
                assert!(
                    candidates[0].dirty,
                    "dirty flag must ride along, not be filtered out"
                );
            }
            other => panic!("expected Multi outcome, got {}", describe(&other)),
        }
    }

    fn describe(outcome: &Option<WorktreeOutcome>) -> &'static str {
        match outcome {
            Some(WorktreeOutcome::Single { .. }) => "Single",
            Some(WorktreeOutcome::Multi(_)) => "Multi",
            Some(WorktreeOutcome::Cancelled) => "Cancelled",
            None => "None",
        }
    }

    // --- agent_needed = false never shows the agent panel -------------

    #[test]
    fn agent_not_needed_never_opens_or_returns_agent_panel() {
        let mut model = WorktreeModel::new_single(
            vec![scanned_entry("acme/proj", "one")],
            14,
            true,
            &agents_map(&["claude"]),
            false, // agent_needed
        );

        match update_worktree(&mut model, key(KeyCode::Enter)) {
            Some(WorktreeOutcome::Single { agent, .. }) => {
                assert!(
                    agent.is_none(),
                    "agent_needed=false must never resolve an agent via the picker"
                );
            }
            other => panic!(
                "expected an immediate Single outcome, got {}",
                describe(&other)
            ),
        }

        // Confirm the overlay flag itself never flips true, independent of
        // the outcome above (regression guard against a future change that
        // opens the overlay but forgets to also skip returning an agent).
        let mut model2 = WorktreeModel::new_single(
            vec![scanned_entry("acme/proj", "one")],
            14,
            true,
            &agents_map(&["claude"]),
            false,
        );
        update_worktree(&mut model2, key(KeyCode::Enter));
        let overlay_open = matches!(
            model2.mode,
            WorktreeMode::Single {
                agent_overlay: true,
                ..
            }
        );
        assert!(
            !overlay_open,
            "agent_needed=false must never open the overlay"
        );
    }

    #[test]
    fn agent_needed_opens_overlay_then_enter_resolves_agent() {
        let mut model = WorktreeModel::new_single(
            vec![scanned_entry("acme/proj", "one")],
            14,
            true,
            &agents_map(&["claude", "grok"]),
            true, // agent_needed
        );

        assert!(update_worktree(&mut model, key(KeyCode::Enter)).is_none());
        let overlay_open = matches!(
            model.mode,
            WorktreeMode::Single {
                agent_overlay: true,
                ..
            }
        );
        assert!(
            overlay_open,
            "Enter with agent_needed=true must open the overlay"
        );

        match update_worktree(&mut model, key(KeyCode::Enter)) {
            Some(WorktreeOutcome::Single { agent, .. }) => {
                assert_eq!(agent.as_deref(), Some("claude")); // first sorted name
            }
            other => panic!(
                "expected Single outcome from the overlay, got {}",
                describe(&other)
            ),
        }
    }

    #[test]
    fn esc_in_overlay_backs_out_not_full_cancel() {
        let mut model = WorktreeModel::new_single(
            vec![scanned_entry("acme/proj", "one")],
            14,
            true,
            &agents_map(&["claude"]),
            true,
        );
        update_worktree(&mut model, key(KeyCode::Enter)); // opens overlay
        let outcome = update_worktree(&mut model, key(KeyCode::Esc));
        assert!(
            outcome.is_none(),
            "Esc on an empty overlay filter must not cancel the whole screen"
        );
        let overlay_open = matches!(
            model.mode,
            WorktreeMode::Single {
                agent_overlay: true,
                ..
            }
        );
        assert!(
            !overlay_open,
            "Esc must close the overlay, back to the worktree list"
        );
    }
}
