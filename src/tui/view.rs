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
    for s in segs {
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
            dim("type to filter")
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
    right.push(dim(format!("cw {} ", env!("CARGO_PKG_VERSION"))).into());

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
                .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
                .split(area);
            draw_repo_pane(
                frame,
                repos,
                model.focus == Focus::Repos,
                model.loading,
                cols[0],
            );
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
    repos: &ListState<RepoRow>,
    focused: bool,
    loading: bool,
    area: Rect,
) {
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
    let header = Row::new(vec!["NAME", "OWNER", "UPDATED", "LOCAL"]).style(header_style());
    let rows: Vec<Row> = repos
        .filtered
        .iter()
        .filter_map(|&idx| repos.items.get(idx))
        .map(|row| {
            let recent = is_recently_updated(&row.repo.updated_at, now);
            let name = if recent {
                Span::styled(format!("★ {}", row.repo.name), Style::default().fg(RECENT))
            } else {
                Span::raw(row.repo.name.clone())
            };
            Row::new(vec![
                name,
                dim(row.repo.owner.clone()),
                dim(relative_time(&row.repo.updated_at, now)),
                Span::raw(if row.local { "✓" } else { "" }),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Fill(4),
            Constraint::Fill(3),
            Constraint::Length(9),
            Constraint::Length(5),
        ],
    )
    .header(header)
    .row_highlight_style(selected_style())
    .highlight_symbol(HIGHLIGHT_SYMBOL);

    let mut table_state = repos.table.borrow_mut();
    frame.render_stateful_widget(table, inner, &mut table_state);
}

/// The worktree pane's bottom-border message: the in-flight pipeline stage
/// (with a spinner) beats the last informational status.
fn worktree_footer(model: &DashboardModel) -> Option<Line<'static>> {
    if let Some(pending) = &model.pending {
        let spinner = SPINNER[(model.ticks % SPINNER.len() as u64) as usize];
        let text = match pending.stage {
            Stage::Cloning => format!("cloning/pulling {}…", pending.ctx.repo_label),
            Stage::CloneHook | Stage::CreateHook => "running hook…".to_string(),
            Stage::CreatingWorktree => format!("creating worktree {}…", pending.ctx.slug),
            Stage::Launching => format!("launching {}…", pending.ctx.agent),
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

    let header = Row::new(vec!["", "REPO", "WORKTREE", "STATUS", "AGE"]).style(header_style());
    let rows: Vec<Row> = model
        .worktrees
        .filtered
        .iter()
        .filter_map(|&idx| model.worktrees.items.get(idx))
        .map(|row| build_worktree_row(row, model))
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(MARK_WIDTH),
            Constraint::Fill(5),
            Constraint::Fill(4),
            Constraint::Length(22),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .row_highlight_style(selected_style())
    .highlight_symbol(HIGHLIGHT_SYMBOL);

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

fn build_worktree_row<'a>(row: &'a WorktreeRow, model: &DashboardModel) -> Row<'a> {
    match &row.selection {
        WorktreeSelection::New => Row::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                row.repo_label.clone(),
                Style::default().fg(NEW_ROW),
            )),
            Line::from(Span::styled(
                "+ new worktree",
                Style::default().fg(NEW_ROW).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(""),
        ]),
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
            Row::new(vec![
                Line::from(mark),
                Line::from(Span::styled(row.repo_label.clone(), name_style)),
                Line::from(Span::styled(entry.slug.clone(), name_style)),
                Line::from(status_cell(row.state)),
                Line::from(age),
            ])
        }
    }
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
    if let Some(current) = model.agents.get(model.agent_index) {
        if current.installed {
            segs.push(seg(dim(format!(" {} ", current.cmd_preview))));
        } else {
            segs.push(seg(Span::styled(
                format!(" not installed — '{}' is not on PATH ", current.cmd_preview),
                Style::default().fg(DANGER),
            )));
        }
    }
    segs.push(seg(dim(" ←/→ or click to choose")));
    render_segments(frame, model, area, segs);
}

/// Every key hint is also a button — clicking it performs the same
/// `Action` the key does (`tui::update::perform`).
fn draw_help_line(frame: &mut Frame, model: &DashboardModel, area: Rect) {
    let sep = || seg(dim(" · "));
    let mut segs = vec![
        seg(Span::raw(" ")),
        button(key("enter"), Action::OpenFocused),
        seg(Span::raw(" open")),
        sep(),
        button(key("n"), Action::NewWorktree),
        seg(Span::raw(" new")),
        sep(),
    ];
    if let Some(idx) = model.worktrees.selected_index() {
        segs.push(button(key("space"), Action::ToggleMark(idx)));
    } else {
        segs.push(seg(key("space")));
    }
    segs.extend([
        seg(Span::raw(" mark")),
        sep(),
        button(destructive_key("d"), Action::Delete),
        seg(Span::raw(" delete")),
        sep(),
        button(key("r"), Action::Rescan),
        seg(Span::raw(" rescan")),
        sep(),
    ]);
    if model.repos.is_some() {
        segs.extend([
            button(key("tab"), Action::ToggleFocus),
            seg(Span::raw(" repos")),
            sep(),
        ]);
    }
    segs.extend([
        button(key("←/→"), Action::CycleAgent),
        seg(Span::raw(" agent")),
        sep(),
        button(destructive_key("q"), Action::Quit),
        seg(Span::raw(" quit")),
        seg(dim("   click a row to focus, double-click to open")),
    ]);
    render_segments(frame, model, area, segs);
}

// ---------------------------------------------------------------------
// Modals
// ---------------------------------------------------------------------

fn draw_modal(frame: &mut Frame, model: &DashboardModel, modal: &Modal, full: Rect) {
    match modal {
        Modal::HookConsent { resolved, .. } => {
            let area = centered(full, 70, 7);
            let block = modal_block(" run hook? ", Style::default().fg(KEY_HINT));
            let inner = block.inner(area);
            frame.render_widget(Clear, area);
            frame.render_widget(block, area);
            let rows = split_last_line(inner);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("{} {}", resolved.program, resolved.args.join(" "))),
                    Line::from(dim(format!("cwd: {}", resolved.cwd.display()))),
                    Line::from(dim(
                        "this runs code the cloned repo supplies — asked once per repo",
                    )),
                ])
                .wrap(Wrap { trim: false }),
                rows.0,
            );
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

    // Height: one line per target (capped), a blank, two summary lines, the
    // button row, plus the border.
    let max_list = (full.height.saturating_sub(10) as usize).max(3);
    let listed = targets.len().min(max_list);
    let height = (listed + 6 + usize::from(listed < targets.len())) as u16;
    let band = centered(full, 80, 0);
    let strip = centered_fixed(full, 0, height);
    let area = Rect::new(band.x, strip.y, band.width, strip.height);
    let block = modal_block(title, title_style);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for t in targets.iter().take(listed) {
        let will_go = include_risky || !t.risky();
        let marker = if will_go {
            Span::styled("✗ ", Style::default().fg(DANGER))
        } else {
            dim("· ")
        };
        let mut spans = vec![marker, Span::raw(format!("{:<40} ", t.label))];
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

    let rows = split_last_line(inner);
    frame.render_widget(Paragraph::new(lines), rows.0);

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

/// A rect exactly `height` rows tall (clamped to `area`), vertically
/// centered, spanning `area`'s full width — combined with `centered`'s
/// horizontal band via `Rect::union`-style intersection by the caller.
fn centered_fixed(area: Rect, _width: u16, height: u16) -> Rect {
    let height = height.min(area.height);
    let y = area.y + (area.height - height) / 2;
    Rect::new(area.x, y, area.width, height)
}
