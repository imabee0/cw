//! Pure rendering — `draw_dashboard` takes `&DashboardModel` and paints one
//! frame. Never mutates model state directly (see `Screen::draw`'s doc
//! comment): geometry a later click needs is cached through the model's own
//! interior mutability (`ListState::table_rect`/`pane_rect`, and the
//! `hotspots` list every clickable segment registers into), not by
//! widening this signature.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::model::{
    is_recently_updated, plural, relative_time, Action, DashboardModel, DeleteTarget, Focus,
    ListState, Modal, RepoRow, Stage, WorktreeRow,
};
use crate::gitstatus::WorkState;
use crate::worktree::WorktreeSelection;

const CHANGED: Color = Color::Yellow;
const UNPUSHED: Color = Color::Magenta;
const DIM: Color = Color::DarkGray;
const FOCUSED_BORDER: Color = Color::Cyan;
const RECENT: Color = Color::Green;
const NEW_ROW: Color = Color::Green;
const KEY_HINT: Color = Color::Cyan;
const KEY_HINT_DESTRUCTIVE: Color = Color::Yellow;
const DANGER: Color = Color::Red;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const HIGHLIGHT_SYMBOL: &str = "> ";
/// Width of the worktree table's mark column (`[x]`).
const MARK_WIDTH: u16 = 3;

pub fn draw_dashboard(frame: &mut Frame, model: &DashboardModel) {
    model.hotspots.borrow_mut().clear();
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // filter prompt + version/update notice
            Constraint::Min(3),    // body: repo/worktree panes
            Constraint::Length(1), // agent bar
            Constraint::Length(1), // help line
        ])
        .split(area);

    draw_top_line(frame, model, rows[0]);
    draw_body(frame, model, rows[1]);
    draw_agent_bar(frame, model, rows[2]);
    draw_help_line(frame, model, rows[3]);

    if let Some(modal) = &model.modal {
        draw_modal(frame, model, modal, area);
    }
}

/// One renderable segment of a single-line bar; `action` makes it a
/// clickable hotspot (registered at its rendered position).
struct Seg<'a> {
    span: Span<'a>,
    action: Option<Action>,
}

fn seg<'a>(span: Span<'a>) -> Seg<'a> {
    Seg { span, action: None }
}

fn button<'a>(span: Span<'a>, action: Action) -> Seg<'a> {
    Seg {
        span,
        action: Some(action),
    }
}

fn segs_width(segs: &[Seg]) -> u16 {
    segs.iter().map(|s| s.span.width() as u16).sum()
}

/// Renders `segs` left-to-right in `area`, registering a hotspot for every
/// segment that carries an `Action` — clamped to the area, so a segment
/// pushed off the right edge by a narrow terminal is simply not clickable
/// rather than mis-registered.
fn render_segments(frame: &mut Frame, model: &DashboardModel, area: Rect, segs: Vec<Seg>) {
    let mut x = area.x;
    let mut spans = Vec::with_capacity(segs.len());
    let mut hotspots = model.hotspots.borrow_mut();
    for mut s in segs {
        if x >= area.right() {
            break;
        }
        let room = area.right() - x;
        if s.span.width() as u16 > room {
            // Cut where it crosses the edge, visibly: "…" rather than half a word.
            s.span.content = fit(&s.span.content, room).into();
        }
        let w = s.span.width() as u16;
        if let Some(action) = s.action {
            let end = x.saturating_add(w).min(area.right());
            if end > x {
                hotspots.push((Rect::new(x, area.y, end - x, 1), action));
            }
        }
        x = x.saturating_add(w);
        spans.push(s.span);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn dim(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(DIM))
}

fn key(text: &'static str) -> Span<'static> {
    Span::styled(text, Style::default().fg(KEY_HINT))
}

fn destructive_key(text: &'static str) -> Span<'static> {
    Span::styled(text, Style::default().fg(KEY_HINT_DESTRUCTIVE))
}

/// `text` cut to `width` columns, ending in `…` when it had to be cut — so
/// a truncated name reads as truncated rather than as a different name.
fn fit(text: &str, width: u16) -> String {
    let width = width as usize;
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// The widths a `Table` drawn into `area_width` gives its columns (after
/// the highlight-symbol gutter and ratatui's default 1-column spacing), so
/// cells can be fitted with `fit` before rendering.
fn column_widths(area_width: u16, constraints: &[Constraint]) -> Vec<u16> {
    let avail = area_width.saturating_sub(HIGHLIGHT_SYMBOL.len() as u16);
    Layout::horizontal(constraints.to_vec())
        .spacing(1)
        .split(Rect::new(0, 0, avail, 1))
        .iter()
        .map(|r| r.width)
        .collect()
}

/// Which of a table's columns to show at `width`. Each column is its
/// constraint plus a drop rank (0 = always shown); while the `Fill` columns
/// would get less than `fill_min` each, the highest-ranked column goes —
/// so on a narrow terminal the names stay readable and the extras give way.
fn responsive_columns(width: u16, cols: &[(Constraint, u8)], fill_min: u16) -> Vec<usize> {
    let mut kept: Vec<usize> = (0..cols.len()).collect();
    loop {
        let avail = width.saturating_sub(HIGHLIGHT_SYMBOL.len() as u16);
        let fixed: u16 = kept
            .iter()
            .map(|&i| match cols[i].0 {
                Constraint::Length(n) => n,
                _ => 0,
            })
            .sum();
        let fills = kept
            .iter()
            .filter(|&&i| matches!(cols[i].0, Constraint::Fill(_)))
            .count() as u16;
        let spacing = kept.len().saturating_sub(1) as u16;
        if avail >= fixed + spacing + fills * fill_min {
            break;
        }
        let Some(pos) = kept
            .iter()
            .enumerate()
            .filter(|(_, &i)| cols[i].1 > 0)
            .max_by_key(|(_, &i)| cols[i].1)
            .map(|(pos, _)| pos)
        else {
            break;
        };
        kept.remove(pos);
    }
    kept
}

/// Only the focused pane shows a cursor and a reversed row; the other pane
/// keeps its row in bold, so it's always clear where keys go.
fn highlight(table: Table<'_>, focused: bool) -> Table<'_> {
    if focused {
        table
            .row_highlight_style(selected_style())
            .highlight_symbol(HIGHLIGHT_SYMBOL)
    } else {
        table
            .row_highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .highlight_symbol("  ")
    }
}

fn header_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD).fg(DIM)
}

fn selected_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(FOCUSED_BORDER)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM)
    }
}

// ---------------------------------------------------------------------
// Top line: filter prompt, version, update notice
// ---------------------------------------------------------------------

fn draw_top_line(frame: &mut Frame, model: &DashboardModel, area: Rect) {
    let (label, query) = match model.focus {
        Focus::Repos => (
            "repos",
            model.repos.as_ref().map(|r| r.query.as_str()).unwrap_or(""),
        ),
        Focus::Worktrees => ("worktrees", model.worktrees.query.as_str()),
    };
    let left = vec![
        seg(Span::styled(
            format!(" {label} › "),
            Style::default().fg(KEY_HINT),
        )),
        seg(Span::raw(query.to_string())),
        seg(if query.is_empty() {
            dim(if model.editing_filter {
                "search: type any letters; Esc clears"
            } else {
                "type to filter; / search all letters"
            })
        } else {
            dim("▏esc clears")
        }),
    ];

    let mut right: Vec<Seg> = Vec::new();
    if let Some(version) = &model.update_available {
        right.push(button(
            Span::styled(
                format!("update available: {version} — press u "),
                Style::default().fg(KEY_HINT_DESTRUCTIVE),
            ),
            Action::ApplyUpdate,
        ));
    }
    right.push(dim(format!("cw {} ", env!("CW_VERSION"))).into());

    let right_width = segs_width(&right).min(area.width);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_width)])
        .split(area);
    render_segments(frame, model, cols[0], left);
    render_segments(frame, model, cols[1], right);
}

impl<'a> From<Span<'a>> for Seg<'a> {
    fn from(span: Span<'a>) -> Self {
        seg(span)
    }
}

// ---------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------

fn draw_body(frame: &mut Frame, model: &DashboardModel, area: Rect) {
    match &model.repos {
        Some(repos) => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                // The focused pane gets the larger share: a repo list you're
                // picking from, or the sessions you're working in.
                .constraints(match model.focus {
                    Focus::Repos => [Constraint::Percentage(58), Constraint::Percentage(42)],
                    Focus::Worktrees => [Constraint::Percentage(40), Constraint::Percentage(60)],
                })
                .split(area);
            draw_repo_pane(frame, model, repos, cols[0]);
            draw_worktree_pane(frame, model, cols[1]);
        }
        None => draw_worktree_pane(frame, model, area),
    }
}

fn pane_title(name: &str, shown: usize, total: usize) -> String {
    if shown == total {
        format!(" {name} ({total}) ")
    } else {
        format!(" {name} ({shown}/{total}) ")
    }
}

fn draw_repo_pane(
    frame: &mut Frame,
    model: &DashboardModel,
    repos: &ListState<RepoRow>,
    area: Rect,
) {
    let focused = model.focus == Focus::Repos;
    let loading = model.loading;
    let block = Block::default()
        .title(pane_title("repos", repos.filtered.len(), repos.items.len()))
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    repos.pane_rect.set(area);
    repos.table_rect.set(inner);

    if repos.items.is_empty() {
        let msg = if loading {
            "loading repos…"
        } else {
            "no repos found — check `gh auth status` or your --org filter"
        };
        frame.render_widget(Paragraph::new(dim(msg)).wrap(Wrap { trim: true }), inner);
        return;
    }

    let now = chrono::Utc::now();
    let counts = model.session_counts();
    // OWNER is as wide as the longest owner (capped), NAME takes the rest.
    let owner_width = repos
        .items
        .iter()
        .map(|r| r.repo.owner.chars().count())
        .max()
        .unwrap_or(5)
        .clamp(5, 20) as u16;
    // Dropped right to left as the pane narrows: SESSIONS, UPDATED, OWNER.
    let columns = [
        ("NAME", Constraint::Fill(1), 0),
        ("OWNER", Constraint::Length(owner_width), 1),
        ("UPDATED", Constraint::Length(8), 2),
        ("SESSIONS", Constraint::Length(8), 3),
    ];
    let kept = responsive_columns(inner.width, &columns.map(|(_, c, rank)| (c, rank)), 14);
    let constraints: Vec<Constraint> = kept.iter().map(|&i| columns[i].1).collect();
    let mut widths = [0u16; 4];
    for (&i, w) in kept.iter().zip(column_widths(inner.width, &constraints)) {
        widths[i] = w;
    }
    let header =
        Row::new(kept.iter().map(|&i| columns[i].0).collect::<Vec<_>>()).style(header_style());
    let rows: Vec<Row> = repos
        .filtered
        .iter()
        .filter_map(|&idx| repos.items.get(idx))
        .map(|row| {
            let recent = is_recently_updated(&row.repo.updated_at, now);
            let name = if recent {
                Span::styled(
                    fit(&format!("★ {}", row.repo.name), widths[0]),
                    Style::default().fg(RECENT),
                )
            } else {
                Span::raw(fit(&row.repo.name, widths[0]))
            };
            let sessions = counts
                .get(row.repo.full_name().as_str())
                .map(|n| n.to_string())
                .unwrap_or_default();
            let mut cells = [
                Some(name),
                Some(dim(fit(&row.repo.owner, widths[1]))),
                Some(dim(relative_time(&row.repo.updated_at, now))),
                Some(Span::raw(sessions)),
            ];
            Row::new(
                kept.iter()
                    .map(|&i| cells[i].take().unwrap_or_default())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let table = highlight(Table::new(rows, constraints).header(header), focused);

    let mut table_state = repos.table.borrow_mut();
    frame.render_stateful_widget(table, inner, &mut table_state);
}

/// The worktree pane's bottom-border message: the in-flight pipeline stage
/// (with a spinner) beats the last informational status.
fn worktree_footer(model: &DashboardModel) -> Option<Line<'static>> {
    if let Some(pending) = &model.pending {
        let spinner = SPINNER[(model.ticks % SPINNER.len() as u64) as usize];
        let text = match pending.stage {
            Stage::Cloning => format!("fetching {}…", pending.ctx.repo_label),
            Stage::CloneHook | Stage::CreateHook => "running hook…".to_string(),
            Stage::CreatingWorktree => format!("creating worktree {}…", pending.ctx.slug),
            Stage::Launching => {
                let skipping = model
                    .agents
                    .iter()
                    .find(|a| a.name == pending.ctx.agent)
                    .is_some_and(|a| a.skips_permissions(model.skip_permissions()));
                if skipping {
                    format!("launching {} — skipping permissions…", pending.ctx.agent)
                } else {
                    format!("launching {}…", pending.ctx.agent)
                }
            }
        };
        return Some(Line::from(Span::styled(
            format!(" {spinner} {text} "),
            Style::default().fg(KEY_HINT),
        )));
    }
    model.status.as_ref().map(|status| {
        Line::from(vec![
            Span::styled(
                format!(" {status} "),
                Style::default().fg(KEY_HINT_DESTRUCTIVE),
            ),
            dim("esc dismiss "),
        ])
    })
}

fn draw_worktree_pane(frame: &mut Frame, model: &DashboardModel, area: Rect) {
    let focused = model.focus == Focus::Worktrees;
    let existing = model
        .worktrees
        .items
        .iter()
        .filter(|r| matches!(r.selection, WorktreeSelection::Existing(_)))
        .count();
    let shown = model
        .worktrees
        .filtered
        .iter()
        .filter(|&&i| {
            matches!(
                model.worktrees.items[i].selection,
                WorktreeSelection::Existing(_)
            )
        })
        .count();
    let mut block = Block::default()
        .title(pane_title("worktrees", shown, existing))
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused));
    if let Some(footer) = worktree_footer(model) {
        block = block.title_bottom(footer);
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    model.worktrees.pane_rect.set(area);
    model.worktrees.table_rect.set(inner);

    if model.worktrees.items.is_empty() {
        frame.render_widget(
            Paragraph::new(dim(
                "no worktrees yet — pick a repo (tab) and press n, or enter on it",
            ))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    // STATUS is as wide as its longest entry ("clean" … "3 changed · +2 unpushed").
    let status_width = model
        .worktrees
        .items
        .iter()
        .map(|r| match r.selection {
            WorktreeSelection::Existing(_) => Line::from(status_cell(r.state)).width(),
            WorktreeSelection::New => 0,
        })
        .max()
        .unwrap_or(0)
        .max("STATUS".len()) as u16;
    // Dropped as the pane narrows: AGE, then STATUS. The mark column stays
    // first so its click hotspots (below) line up whatever is shown.
    let columns = [
        ("", Constraint::Length(MARK_WIDTH), 0),
        // The session name is what you're scanning for: it gets the larger share.
        ("REPO", Constraint::Fill(2), 0),
        ("WORKTREE", Constraint::Fill(3), 0),
        ("STATUS", Constraint::Length(status_width), 1),
        ("AGE", Constraint::Length(8), 2),
    ];
    let kept = responsive_columns(inner.width, &columns.map(|(_, c, rank)| (c, rank)), 10);
    let constraints: Vec<Constraint> = kept.iter().map(|&i| columns[i].1).collect();
    let mut widths = [0u16; 5];
    for (&i, w) in kept.iter().zip(column_widths(inner.width, &constraints)) {
        widths[i] = w;
    }
    let header =
        Row::new(kept.iter().map(|&i| columns[i].0).collect::<Vec<_>>()).style(header_style());
    let short_names = short_repo_names(model);
    let rows: Vec<Row> = model
        .worktrees
        .filtered
        .iter()
        .filter_map(|&idx| model.worktrees.items.get(idx))
        .map(|row| {
            let repo = short_names
                .get(row.repo_label.as_str())
                .copied()
                .unwrap_or(row.repo_label.as_str());
            let mut cells = build_worktree_cells(row, model, repo, widths[1], widths[2]);
            Row::new(
                kept.iter()
                    .map(|&i| std::mem::take(&mut cells[i]))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();

    let table = highlight(Table::new(rows, constraints).header(header), focused);

    let mut table_state = model.worktrees.table.borrow_mut();
    frame.render_stateful_widget(table, inner, &mut table_state);

    // The mark column is clickable per visible row: the same toggle Space
    // performs. Registered after the render, once `TableState::offset` is
    // final for this frame.
    let offset = table_state.offset();
    let visible = (inner.height.saturating_sub(1) as usize)
        .min(model.worktrees.filtered.len().saturating_sub(offset));
    let mark_width = HIGHLIGHT_SYMBOL.len() as u16 + MARK_WIDTH;
    let mut hotspots = model.hotspots.borrow_mut();
    for i in 0..visible {
        let y = inner.y + 1 + i as u16;
        hotspots.push((
            Rect::new(inner.x, y, mark_width.min(inner.width), 1),
            Action::ToggleMark(offset + i),
        ));
    }
}

fn status_cell(state: Option<WorkState>) -> Vec<Span<'static>> {
    match state {
        None => vec![dim("unknown")],
        Some(s) if !s.has_unsaved_work() => vec![dim("clean")],
        Some(s) => {
            let mut parts = Vec::new();
            if s.changed_files > 0 {
                parts.push(Span::styled(
                    format!("{} changed", s.changed_files),
                    Style::default().fg(CHANGED),
                ));
            }
            if s.unpushed_commits > 0 {
                if !parts.is_empty() {
                    parts.push(dim(" · "));
                }
                parts.push(Span::styled(
                    format!("+{} unpushed", s.unpushed_commits),
                    Style::default().fg(UNPUSHED),
                ));
            }
            parts
        }
    }
}

/// The five cells of a worktree row (mark, repo, worktree, status, age);
/// the caller picks the ones that fit.
fn build_worktree_cells(
    row: &WorktreeRow,
    model: &DashboardModel,
    repo: &str,
    repo_width: u16,
    slug_width: u16,
) -> [Line<'static>; 5] {
    match &row.selection {
        WorktreeSelection::New => [
            Line::from(""),
            Line::from(Span::styled(
                fit(repo, repo_width),
                Style::default().fg(NEW_ROW),
            )),
            Line::from(Span::styled(
                "+ new",
                Style::default().fg(NEW_ROW).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(""),
        ],
        WorktreeSelection::Existing(entry) => {
            let checked = model.checked.contains(&entry.path);
            let mark = if checked {
                Span::styled(
                    "[x]",
                    Style::default()
                        .fg(KEY_HINT_DESTRUCTIVE)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                dim("[ ]")
            };
            let name_style = if checked {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let age = if row.idle {
                dim(row.age_label.clone())
            } else {
                Span::raw(row.age_label.clone())
            };
            [
                Line::from(mark),
                Line::from(Span::styled(fit(repo, repo_width), name_style)),
                Line::from(Span::styled(fit(&entry.slug, slug_width), name_style)),
                Line::from(status_cell(row.state)),
                Line::from(age),
            ]
        }
    }
}

/// Repo labels as the worktree pane shows them: just the repo name, or
/// `owner/name` when two listed repos share a name.
fn short_repo_names(model: &DashboardModel) -> std::collections::HashMap<&str, &str> {
    let labels: std::collections::BTreeSet<&str> = model
        .worktrees
        .items
        .iter()
        .map(|r| r.repo_label.as_str())
        .collect();
    let name_of =
        |label: &'_ str| -> String { label.rsplit_once('/').map_or(label, |(_, n)| n).to_string() };
    labels
        .iter()
        .map(|&label| {
            let name = label.rsplit_once('/').map_or(label, |(_, n)| n);
            let shared = labels
                .iter()
                .filter(|&&other| name_of(other) == name)
                .count()
                > 1;
            (label, if shared { label } else { name })
        })
        .collect()
}

// ---------------------------------------------------------------------
// Agent bar + help line
// ---------------------------------------------------------------------

fn draw_agent_bar(frame: &mut Frame, model: &DashboardModel, area: Rect) {
    if model.agents.is_empty() {
        render_segments(frame, model, area, vec![seg(dim(" no agents configured"))]);
        return;
    }
    let mut segs = vec![seg(dim(" agent "))];
    for (i, agent) in model.agents.iter().enumerate() {
        let current = i == model.agent_index;
        let style = match (current, agent.installed) {
            (true, _) => Style::default()
                .bg(KEY_HINT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            (false, true) => Style::default(),
            (false, false) => Style::default().fg(DIM).add_modifier(Modifier::CROSSED_OUT),
        };
        segs.push(button(
            Span::styled(format!(" {} ", agent.name), style),
            Action::SelectAgent(i),
        ));
        segs.push(seg(Span::raw(" ")));
    }
    let Some(current) = model.current_agent() else {
        render_segments(frame, model, area, segs);
        return;
    };
    let toggle = model.skip_permissions();
    segs.push(seg(dim("│ ")));
    // The permissions control: a red badge whenever the selected agent will
    // launch without prompts, a quiet hint otherwise. Always clickable (it's
    // one session-wide setting); `p` does the same.
    if current.skips_permissions(toggle) {
        let label = if current.bypass_in_args {
            " ⚠ skip permissions (config args) "
        } else {
            " ⚠ skip permissions "
        };
        segs.push(button(
            Span::styled(
                label,
                Style::default()
                    .bg(DANGER)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Action::ToggleSkipPermissions,
        ));
    } else if current.supports_skip_permissions() {
        segs.push(button(
            Span::styled(" asks permission ", Style::default().fg(DIM)),
            Action::ToggleSkipPermissions,
        ));
    } else {
        let text = if toggle {
            format!(" no skip-permissions mode in {} ", current.name)
        } else {
            " asks permission ".to_string()
        };
        segs.push(button(
            Span::styled(text, Style::default().fg(DIM)),
            Action::ToggleSkipPermissions,
        ));
    }
    segs.push(button(key(" p"), Action::ToggleSkipPermissions));
    segs.push(seg(dim(" toggle ")));

    // The exact command a launch runs, only when it says more than the name.
    let command = current.command_line(toggle);
    if !current.installed {
        segs.push(seg(Span::styled(
            format!(" │ not installed — '{command}' is not on PATH "),
            Style::default().fg(DANGER),
        )));
    } else if command != current.name {
        segs.push(seg(dim(format!(" │ {command} "))));
    }
    render_segments(frame, model, area, segs);
}

/// Every key hint is also a button — clicking it performs the same
/// `Action` the key does (`tui::update::perform`).
fn draw_help_line(frame: &mut Frame, model: &DashboardModel, area: Rect) {
    // Each hint is (priority, segments); when the line is too wide for the
    // terminal, the lowest-priority hints are dropped first instead of the
    // line being cut off mid-word.
    let hint = |k: Span<'static>, action: Option<Action>, label: &'static str| {
        let mut v = vec![match action {
            Some(a) => button(k, a),
            None => seg(k),
        }];
        v.push(seg(Span::raw(format!(" {label}"))));
        v
    };
    let mut hints: Vec<(u8, Vec<Seg>)> = vec![
        (9, hint(key("enter"), Some(Action::OpenFocused), "open")),
        (8, hint(key("n"), Some(Action::NewWorktree), "new")),
    ];
    if model.repos.is_some() {
        let other = match model.focus {
            Focus::Repos => "worktrees",
            Focus::Worktrees => "repos",
        };
        hints.push((9, hint(key("tab"), Some(Action::ToggleFocus), other)));
    }
    hints.push((7, hint(key("/"), None, "search")));
    if model.focus == Focus::Worktrees {
        let mark = model.worktrees.selected_index().map(Action::ToggleMark);
        hints.push((5, hint(key("space"), mark, "mark")));
        hints.push((
            6,
            hint(destructive_key("d"), Some(Action::Delete), "delete"),
        ));
    }
    hints.push((6, hint(key("←/→"), Some(Action::CycleAgent), "agent")));
    hints.push((
        6,
        hint(key("p"), Some(Action::ToggleSkipPermissions), "permissions"),
    ));
    hints.push((3, hint(key("r"), Some(Action::Rescan), "rescan")));
    hints.push((9, hint(destructive_key("q"), Some(Action::Quit), "quit")));
    hints.push((1, vec![seg(dim("click to focus · double-click to open"))]));

    let sep_width = 3; // " · "
    let width_of = |hints: &[(u8, Vec<Seg>)]| -> u16 {
        1 + hints
            .iter()
            .map(|(_, h)| segs_width(h) + sep_width)
            .sum::<u16>()
            .saturating_sub(sep_width)
    };
    while width_of(&hints) > area.width {
        let Some(lowest) = hints
            .iter()
            .enumerate()
            .filter(|(_, (p, _))| *p < 9)
            .min_by_key(|(i, (p, _))| (*p, std::cmp::Reverse(*i)))
            .map(|(i, _)| i)
        else {
            break;
        };
        hints.remove(lowest);
    }

    let mut segs = vec![seg(Span::raw(" "))];
    for (i, (_, h)) in hints.into_iter().enumerate() {
        if i > 0 {
            segs.push(seg(dim(" · ")));
        }
        segs.extend(h);
    }
    render_segments(frame, model, area, segs);
}

// ---------------------------------------------------------------------
// Modals
// ---------------------------------------------------------------------

fn draw_modal(frame: &mut Frame, model: &DashboardModel, modal: &Modal, full: Rect) {
    match modal {
        Modal::HookConsent { resolved, .. } => {
            let body = vec![
                Line::from(format!("{} {}", resolved.program, resolved.args.join(" "))),
                Line::from(dim(format!("cwd: {}", resolved.cwd.display()))),
                Line::from(dim(
                    "this runs code the cloned repo supplies — asked once per repo",
                )),
            ];
            let area = fitted_modal_area(full, 90, &body);
            let block = modal_block(" run hook? ", Style::default().fg(KEY_HINT));
            let inner = block.inner(area);
            frame.render_widget(Clear, area);
            frame.render_widget(block, area);
            let rows = split_last_line(inner);
            frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), rows.0);
            render_segments(
                frame,
                model,
                rows.1,
                vec![
                    button(key("[y] run"), Action::ModalConfirm),
                    seg(Span::raw("   ")),
                    button(destructive_key("[n] skip"), Action::ModalCancel),
                ],
            );
        }
        Modal::ConfirmDelete {
            targets,
            include_risky,
        } => draw_confirm_delete(frame, model, targets, *include_risky, full),
        Modal::Error { title, detail } => {
            let area = centered(full, 80, 60);
            let block = modal_block(&format!(" {title} "), Style::default().fg(DANGER));
            let inner = block.inner(area);
            frame.render_widget(Clear, area);
            frame.render_widget(block, area);
            let rows = split_last_line(inner);
            frame.render_widget(
                Paragraph::new(detail.as_str()).wrap(Wrap { trim: false }),
                rows.0,
            );
            render_segments(
                frame,
                model,
                rows.1,
                vec![
                    button(key("[enter] dismiss"), Action::ModalCancel),
                    seg(dim("   also written to ~/.cache/cw/cw.log.<date>")),
                ],
            );
        }
    }
}

fn draw_confirm_delete(
    frame: &mut Frame,
    model: &DashboardModel,
    targets: &[DeleteTarget],
    include_risky: bool,
    full: Rect,
) {
    let risky = targets.iter().filter(|t| t.risky()).count();
    let safe = targets.len() - risky;
    let removing = if include_risky { targets.len() } else { safe };

    let (title, title_style) = if include_risky && risky > 0 {
        (
            " remove worktrees — UNSAVED WORK WILL BE LOST ",
            Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            " remove worktrees? ",
            Style::default().fg(KEY_HINT_DESTRUCTIVE),
        )
    };

    // At most this many targets are listed (the rest summarized), so the
    // summary and buttons always stay on screen.
    let max_list = (full.height.saturating_sub(10) as usize).max(3);
    let listed = targets.len().min(max_list);
    // Labels get whatever the width leaves after the marker, status and
    // "(kept)" note, fitted with "…" rather than wrapped.
    let width = modal_width(full, 90);
    let label_width = width.saturating_sub(2 + 2 + 22 + 8).max(10);

    let mut lines: Vec<Line> = Vec::new();
    for t in targets.iter().take(listed) {
        let will_go = include_risky || !t.risky();
        let marker = if will_go {
            Span::styled("✗ ", Style::default().fg(DANGER))
        } else {
            dim("· ")
        };
        let label = fit(&t.label, label_width);
        let pad = (label_width as usize).saturating_sub(label.chars().count());
        let mut spans = vec![marker, Span::raw(format!("{label}{} ", " ".repeat(pad)))];
        spans.extend(status_cell(t.state));
        if !will_go {
            spans.push(dim("  (kept)"));
        }
        lines.push(Line::from(spans));
    }
    if listed < targets.len() {
        lines.push(Line::from(dim(format!(
            "… and {} more",
            targets.len() - listed
        ))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(if removing == 0 {
        Span::styled(
            "nothing will be removed — every target has unsaved work",
            Style::default().fg(KEY_HINT_DESTRUCTIVE),
        )
    } else {
        Span::raw(format!(
            "{removing} worktree{} and {} branch{} will be removed",
            plural(removing),
            removing,
            if removing == 1 { "" } else { "es" }
        ))
    }));
    lines.push(Line::from(match (risky, include_risky) {
        (0, _) => dim("nothing here has unsaved work"),
        (n, false) => Span::styled(
            format!(
                "{n} with unsaved work (changed files or unpushed commits) will be kept — f includes {}",
                if n == 1 { "it" } else { "them" }
            ),
            Style::default().fg(KEY_HINT_DESTRUCTIVE),
        ),
        (n, true) => Span::styled(
            format!("{n} with unsaved work will be removed too — that work cannot be recovered"),
            Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
        ),
    }));

    let area = fitted_modal_area(full, 90, &lines);
    let block = modal_block(title, title_style);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let rows = split_last_line(inner);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows.0);

    let mut buttons = vec![
        button(destructive_key("[y] remove"), Action::ModalConfirm),
        seg(Span::raw("   ")),
    ];
    if risky > 0 {
        let label = if include_risky {
            "[f] keep unsaved work instead"
        } else {
            "[f] include unsaved work"
        };
        buttons.push(button(
            Span::styled(label, Style::default().fg(DANGER)),
            Action::ModalIncludeRisky,
        ));
        buttons.push(seg(Span::raw("   ")));
    }
    buttons.push(button(key("[n] cancel"), Action::ModalCancel));
    render_segments(frame, model, rows.1, buttons);
}

fn modal_block(title: &str, style: Style) -> Block<'static> {
    Block::default()
        .title(Span::styled(title.to_string(), style))
        .borders(Borders::ALL)
        .border_style(style)
}

/// A modal's inner area split into its body and a final button row.
fn split_last_line(inner: Rect) -> (Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    (rows[0], rows[1])
}

/// A rect `percent_x`% wide and `height` rows tall (or `percent_y`% when
/// `height` is 0), centered in `area`.
fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = if percent_y == 0 {
        area
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ])
            .split(area)[1]
    };
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical)[1]
}

/// A dialog's width: up to `max` columns, never wider than the screen
/// minus a small margin.
fn modal_width(full: Rect, max: u16) -> u16 {
    full.width
        .saturating_sub(4)
        .min(max)
        .max(full.width.min(20))
}

/// A centered dialog sized to its content: `body` (wrapped to the dialog's
/// width) plus a button row and the border, clamped to the screen.
fn fitted_modal_area(full: Rect, max_width: u16, body: &[Line]) -> Rect {
    let width = modal_width(full, max_width);
    let inner_width = width.saturating_sub(2).max(1);
    let body_height: u16 = body
        .iter()
        .map(|l| (l.width() as u16).div_ceil(inner_width).max(1))
        .sum();
    let height = (body_height + 1 + 2).min(full.height);
    Rect::new(
        full.x + (full.width - width) / 2,
        full.y + (full.height - height) / 2,
        width,
        height,
    )
}
