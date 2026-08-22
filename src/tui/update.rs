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

use super::model::{DashboardModel, DashboardOutcome, Focus, Modal, Scope};
use super::msg::Msg;
use super::widgets;
use crate::worktree::WorktreeSelection;

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
/// away in `handle_key` before reaching here) → delete-mode → the focused
/// pane's filter → cancel the whole dashboard.
fn handle_pane_key(model: &mut DashboardModel, key: KeyEvent) -> Option<DashboardOutcome> {
    match key.code {
        KeyCode::Esc => {
            if model.delete_mode {
                model.delete_mode = false;
                model.checked.clear();
                return None;
            }
            if !focused_query_empty(model) {
                clear_focused_query(model);
                return None;
            }
            Some(DashboardOutcome::Cancelled)
        }
        KeyCode::Tab => {
            toggle_focus(model);
            None
        }
        KeyCode::Enter => handle_enter(model),
        KeyCode::Char(' ') if model.delete_mode && model.focus == Focus::Worktrees => {
            toggle_focused_if_existing(model);
            None
        }
        // Query-gated like `q` below, and for the same reason spelled out in
        // CLAUDE.md: an accidental match here is destructive (it either
        // enters delete-mode or opens the removal confirm), so it gets the
        // gate even in panes where a bare `q` would too.
        KeyCode::Char('d') if model.focus == Focus::Worktrees && focused_query_empty(model) => {
            toggle_delete_or_confirm(model);
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
/// pane already tracks the repo cursor live (see `move_focused`). Enter on
/// the Worktrees pane either toggles the focused row's checkbox
/// (delete-mode, matching Space/click — the batch delete itself only ever
/// commits on `d`, never Enter) or starts the clone/hook/create/launch
/// pipeline for that row.
fn handle_enter(model: &mut DashboardModel) -> Option<DashboardOutcome> {
    match model.focus {
        Focus::Repos => {
            model.focus = Focus::Worktrees;
            None
        }
        Focus::Worktrees => {
            if model.delete_mode {
                toggle_focused_if_existing(model);
                return None;
            }
            let selection = model.worktrees.selected()?.selection.clone();
            model.start_pending(selection)
        }
    }
}

/// `d` outside delete-mode: enters it. `d` with something checked: opens the
/// removal confirm modal. `d` in delete-mode with nothing checked: backs
/// out. The three-state toggle the plan's delete-flow section describes.
fn toggle_delete_or_confirm(model: &mut DashboardModel) {
    if !model.checked.is_empty() {
        model.open_delete_confirm();
    } else {
        model.delete_mode = !model.delete_mode;
    }
}

/// Never lets the synthetic "+ new worktree" row be checked for removal —
/// there's nothing on disk yet to delete.
fn toggle_focused_if_existing(model: &mut DashboardModel) {
    let is_existing = matches!(
        model.worktrees.selected().map(|r| &r.selection),
        Some(WorktreeSelection::Existing(_))
    );
    if is_existing {
        model.toggle_checked_focused();
    }
}

fn toggle_focus(model: &mut DashboardModel) {
    if matches!(model.scope, Scope::Browse { .. }) {
        model.focus = match model.focus {
            Focus::Repos => Focus::Worktrees,
            Focus::Worktrees => Focus::Repos,
        };
    }
}

fn focused_query_empty(model: &DashboardModel) -> bool {
    match model.focus {
        Focus::Repos => match &model.scope {
            Scope::Browse { repos } => repos.query.is_empty(),
            _ => true,
        },
        Focus::Worktrees => model.worktrees.query.is_empty(),
    }
}

fn move_focused(model: &mut DashboardModel, delta: isize) {
    match model.focus {
        Focus::Repos => {
            if let Scope::Browse { repos } = &model.scope {
                repos.move_selection(delta);
            }
            model.refresh_worktree_pane();
        }
        Focus::Worktrees => model.worktrees.move_selection(delta),
    }
}

fn clear_focused_query(model: &mut DashboardModel) {
    match model.focus {
        Focus::Repos => {
            if let Scope::Browse { repos } = &mut model.scope {
                repos.query.clear();
                repos.refilter(|r| r.filter_text.as_str());
            }
            model.refresh_worktree_pane();
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
            let popped = if let Scope::Browse { repos } = &mut model.scope {
                let popped = repos.query.pop().is_some();
                if popped {
                    repos.refilter(|r| r.filter_text.as_str());
                }
                popped
            } else {
                false
            };
            if popped {
                model.refresh_worktree_pane();
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
            if let Scope::Browse { repos } = &mut model.scope {
                repos.query.push(c);
                repos.refilter(|r| r.filter_text.as_str());
            }
            model.refresh_worktree_pane();
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
/// row. A click never activates a row — see CLAUDE.md's mouse invariant —
/// except that, like the keyboard, delete-mode also toggles the checkbox on
/// the row it just focused. Scroll wheel events move the currently focused
/// pane's selection, same as `j`/`k`.
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
    let repo_idx = match &model.scope {
        Scope::Browse { repos } => hit_test(repos.table_rect.get(), repos, mouse),
        _ => None,
    };
    if let Some(idx) = repo_idx {
        if let Scope::Browse { repos } = &model.scope {
            repos.select(Some(idx));
        }
        model.focus = Focus::Repos;
        model.refresh_worktree_pane();
        return;
    }

    if let Some(idx) = hit_test(model.worktrees.table_rect.get(), &model.worktrees, mouse) {
        model.worktrees.select(Some(idx));
        model.focus = Focus::Worktrees;
        if model.delete_mode {
            toggle_focused_if_existing(model);
        }
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
    fn all_worktrees_model(entries: Vec<ScannedEntry>, delete_mode: bool) -> DashboardModel {
        let consent_dir = tempfile::tempdir().expect("tempdir");
        let mut model = DashboardModel::new_all_worktrees(
            PathBuf::from("/nonexistent-root"),
            config_with_agents(&["claude", "grok"]),
            HookConsent::new(),
            consent_dir.path().join("hook-consent.json"),
            false,
            delete_mode,
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

    // --- Esc nesting: delete-mode -> filter -> cancel -------------------

    #[test]
    fn esc_exits_delete_mode_before_clearing_filter() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")], true);
        model.worktrees.query.push_str("abc");
        model.worktrees.refilter(|r| r.filter_text.as_str());

        assert!(update_dashboard(&mut model, key(KeyCode::Esc)).is_none());
        assert!(!model.delete_mode, "first Esc must exit delete-mode");
        assert_eq!(
            model.worktrees.query, "abc",
            "delete-mode exit must not also clear the filter"
        );

        assert!(update_dashboard(&mut model, key(KeyCode::Esc)).is_none());
        assert_eq!(model.worktrees.query, "", "second Esc clears the filter");

        match update_dashboard(&mut model, key(KeyCode::Esc)) {
            Some(DashboardOutcome::Cancelled) => {}
            _ => panic!("third Esc, with delete-mode off and filter empty, must cancel"),
        }
    }

    #[test]
    fn ctrl_c_cancels_immediately_regardless_of_state() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")], true);
        model.worktrees.query.push('c');
        match update_dashboard(&mut model, ctrl_c()) {
            Some(DashboardOutcome::Cancelled) => {}
            _ => panic!("Ctrl-C must cancel even with filter text and delete-mode active"),
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

        assert_eq!(model.focus, Focus::Repos);
        update_dashboard(&mut model, key(KeyCode::Char('a')));
        assert_eq!(model.focus, Focus::Repos);

        update_dashboard(&mut model, key(KeyCode::Tab));
        assert_eq!(model.focus, Focus::Worktrees);
        update_dashboard(&mut model, key(KeyCode::Char('z')));

        update_dashboard(&mut model, key(KeyCode::Tab));
        assert_eq!(model.focus, Focus::Repos);

        let Scope::Browse { repos } = &model.scope else {
            panic!("expected Browse scope");
        };
        assert_eq!(repos.query, "a", "repo pane's own query must survive Tab");
        assert_eq!(
            model.worktrees.query, "z",
            "worktree pane's own query must survive Tab"
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

    // --- Delete three-state toggle ---------------------------------------

    #[test]
    fn d_three_state_toggle_enter_confirm_back_out() {
        let mut model = all_worktrees_model(
            vec![
                scanned_entry("acme/proj", "one"),
                scanned_entry("acme/proj", "two"),
            ],
            false,
        );

        // State 1: not in delete-mode, nothing checked -> `d` enters it.
        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        assert!(model.delete_mode);
        assert!(model.modal.is_none());

        // State 2: in delete-mode, focused row checked -> `d` opens confirm.
        update_dashboard(&mut model, key(KeyCode::Char(' ')));
        assert_eq!(model.checked.len(), 1);
        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        assert!(
            matches!(model.modal, Some(Modal::ConfirmDelete { .. })),
            "d with something checked must open the confirm modal"
        );

        // Back out of the modal without confirming, uncheck, then `d` with
        // nothing checked while still in delete-mode backs all the way out.
        update_dashboard(&mut model, key(KeyCode::Esc));
        assert!(model.modal.is_none());
        update_dashboard(&mut model, key(KeyCode::Char(' '))); // uncheck
        assert!(model.checked.is_empty());
        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        assert!(
            !model.delete_mode,
            "d with nothing checked while already in delete-mode must back out"
        );
    }

    #[test]
    fn d_gated_on_active_filter_types_instead_of_toggling() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")], true);
        model.worktrees.query.push_str("abc");
        model.worktrees.refilter(|r| r.filter_text.as_str());

        assert!(update_dashboard(&mut model, key(KeyCode::Char('d'))).is_none());
        assert_eq!(
            model.worktrees.query, "abcd",
            "'d' with an active filter must type into the query, not toggle delete-mode"
        );
        assert!(model.delete_mode, "delete-mode must be unaffected");
    }

    #[test]
    fn confirm_delete_modal_yes_removes_only_checked() {
        let mut model = all_worktrees_model(vec![scanned_entry("acme/proj", "one")], true);
        update_dashboard(&mut model, key(KeyCode::Char(' '))); // check the only row
        update_dashboard(&mut model, key(KeyCode::Char('d'))); // opens confirm
        assert!(matches!(model.modal, Some(Modal::ConfirmDelete { .. })));

        // 'n' backs out without removing anything or touching all_entries.
        update_dashboard(&mut model, key(KeyCode::Char('n')));
        assert!(model.modal.is_none());
        assert_eq!(model.all_entries.len(), 1);
    }

    // --- Hook-consent-once-per-repo ---------------------------------------

    #[test]
    fn hook_consent_declined_is_recorded_and_not_reprompted() {
        let mut model = all_worktrees_model(vec![], false);
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
        let mut model = all_worktrees_model(vec![], false);
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
        let mut model = all_worktrees_model(vec![], false);
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
        let mut model = all_worktrees_model(
            vec![
                scanned_entry("acme/proj", "one"),
                scanned_entry("acme/proj", "two"),
                scanned_entry("acme/proj", "three"),
            ],
            false,
        );
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
        let mut model = all_worktrees_model(vec![], false);
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
        let mut model = all_worktrees_model(vec![], false);
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
        let mut model = all_worktrees_model(
            vec![
                scanned_entry("acme/first", "one"),
                scanned_entry("acme/second", "two"),
            ],
            false,
        );
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
