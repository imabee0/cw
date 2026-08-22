//! Pure rendering — `draw_dashboard` takes `&DashboardModel` and paints one
//! frame. Never mutates model state directly (see `Screen::draw`'s doc
//! comment): geometry a later click needs is cached through the model's own
//! interior mutability (`ListState::table`/`table_rect`), not by widening
//! this signature.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table};
use ratatui::Frame;

use super::model::{
    is_recently_updated, relative_time, AgentEntry, DashboardModel, Focus, ListState, Modal,
    RepoRow, Scope, Stage, WorktreeRow,
};
use crate::worktree::WorktreeSelection;

const DIRTY: Color = Color::Yellow;
const IDLE: Color = Color::DarkGray;
const FOCUSED_BORDER: Color = Color::Cyan;
const UNFOCUSED_BORDER: Color = Color::DarkGray;
const RECENT: Color = Color::Green;
const KEY_HINT: Color = Color::Cyan;
const KEY_HINT_DESTRUCTIVE: Color = Color::Yellow;

pub fn draw_dashboard(frame: &mut Frame, model: &DashboardModel) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // filter line
            Constraint::Min(3),    // body: repo/worktree panes
            Constraint::Length(1), // agent footer
            Constraint::Length(1), // status/help line
        ])
        .split(area);
    let (title_area, filter_area, body_area, agent_area, status_area) =
        (rows[0], rows[1], rows[2], rows[3], rows[4]);

    frame.render_widget(Paragraph::new(title(model)), title_area);
    frame.render_widget(filter_line(model), filter_area);
    draw_body(frame, model, body_area);
    frame.render_widget(agent_bar(model), agent_area);
    frame.render_widget(Paragraph::new(status_line(model)), status_area);

    if let Some(modal) = &model.modal {
        draw_modal(frame, modal, area);
    }
}

fn title(model: &DashboardModel) -> &'static str {
    match &model.scope {
        Scope::Browse { .. } => "cw — pick a repo, then a worktree",
        Scope::AllWorktrees if model.delete_mode => "cw clean — select worktrees to remove",
        Scope::AllWorktrees => "cw resume — pick a worktree",
        Scope::SingleRepo { .. } => "cw scratch — pick a worktree",
    }
}

fn header_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
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
        Style::default().fg(UNFOCUSED_BORDER)
    }
}

fn filter_line(model: &DashboardModel) -> Paragraph<'_> {
    let (label, query) = match model.focus {
        Focus::Repos => match &model.scope {
            Scope::Browse { repos } => ("repos", repos.query.as_str()),
            _ => ("", ""),
        },
        Focus::Worktrees => ("worktrees", model.worktrees.query.as_str()),
    };
    Paragraph::new(Line::from(vec![
        Span::styled(format!("{label}> "), Style::default().fg(Color::Cyan)),
        Span::raw(query),
    ]))
}

fn draw_body(frame: &mut Frame, model: &DashboardModel, area: Rect) {
    match &model.scope {
        Scope::Browse { repos } => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
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
        Scope::AllWorktrees | Scope::SingleRepo { .. } => {
            draw_worktree_pane(frame, model, area);
        }
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
        .title(" repos ")
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    repos.table_rect.set(inner);

    if repos.items.is_empty() {
        let msg = if loading {
            "loading repos…"
        } else {
            "no repos found — check `gh auth status` or your --org filter"
        };
        frame.render_widget(Paragraph::new(msg), inner);
        return;
    }

    let now = chrono::Utc::now();
    let header = Row::new(vec!["NAME", "OWNER", "UPDATED", "LOCAL"]).style(header_style());
    let rows: Vec<Row> = repos
        .filtered
        .iter()
        .filter_map(|&idx| repos.items.get(idx))
        .map(|row| {
            let name = if is_recently_updated(&row.repo.updated_at, now) {
                format!("★ {}", row.repo.name)
            } else {
                row.repo.name.clone()
            };
            let name_style = if is_recently_updated(&row.repo.updated_at, now) {
                Style::default().fg(RECENT)
            } else {
                Style::default()
            };
            Row::new(vec![
                Span::styled(name, name_style),
                Span::raw(row.repo.owner.clone()),
                Span::raw(relative_time(&row.repo.updated_at, now)),
                Span::raw(if row.local { "✓" } else { "" }),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(40),
            Constraint::Percentage(25),
            Constraint::Percentage(20),
            Constraint::Percentage(15),
        ],
    )
    .header(header)
    .row_highlight_style(selected_style())
    .highlight_symbol("> ");

    let mut table_state = repos.table.borrow_mut();
    frame.render_stateful_widget(table, inner, &mut table_state);
}

fn draw_worktree_pane(frame: &mut Frame, model: &DashboardModel, area: Rect) {
    let focused = model.focus == Focus::Worktrees;
    let block = Block::default()
        .title(" worktrees ")
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    model.worktrees.table_rect.set(inner);

    if model.worktrees.items.is_empty() {
        frame.render_widget(Paragraph::new("no worktrees yet"), inner);
        return;
    }

    let is_multi = model.delete_mode;
    let mut header_cells = vec!["REPO", "SLUG", "IDLE", "STATUS"];
    if is_multi {
        header_cells.insert(0, "");
    }
    let header = Row::new(header_cells).style(header_style());

    let rows: Vec<Row> = model
        .worktrees
        .filtered
        .iter()
        .filter_map(|&idx| model.worktrees.items.get(idx).map(|row| (idx, row)))
        .map(|(idx, row)| build_worktree_row(row, idx, is_multi, &model.checked))
        .collect();

    let widths: Vec<Constraint> = if is_multi {
        vec![
            Constraint::Length(3),
            Constraint::Percentage(32),
            Constraint::Percentage(32),
            Constraint::Percentage(16),
            Constraint::Percentage(16),
        ]
    } else {
        vec![
            Constraint::Percentage(35),
            Constraint::Percentage(35),
            Constraint::Percentage(15),
            Constraint::Percentage(15),
        ]
    };

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(selected_style())
        .highlight_symbol("> ");

    let mut table_state = model.worktrees.table.borrow_mut();
    frame.render_stateful_widget(table, inner, &mut table_state);
}

fn build_worktree_row<'a>(
    row: &'a WorktreeRow,
    idx: usize,
    is_multi: bool,
    checked: &std::collections::HashSet<usize>,
) -> Row<'a> {
    let (repo_cell, slug_cell) = match &row.selection {
        WorktreeSelection::Existing(entry) => (row.repo_label.clone(), entry.slug.clone()),
        WorktreeSelection::New => (String::new(), "+ new worktree".to_string()),
    };
    let idle_cell = row.idle_label.clone().unwrap_or_default();
    let status_cell = if row.dirty { "dirty" } else { "" };

    let idle_style = if row.idle_label.is_some() {
        Style::default().fg(IDLE)
    } else {
        Style::default()
    };
    let row_style = if row.dirty {
        Style::default().fg(DIRTY)
    } else {
        Style::default()
    };

    let mut cells = vec![
        Span::styled(repo_cell, row_style),
        Span::styled(slug_cell, row_style),
        Span::styled(idle_cell, idle_style),
        Span::styled(status_cell.to_string(), row_style),
    ];
    if is_multi {
        let mark = if checked.contains(&idx) { "[x]" } else { "[ ]" };
        cells.insert(0, Span::styled(mark, row_style));
    }
    Row::new(cells)
}

fn agent_bar(model: &DashboardModel) -> Paragraph<'_> {
    if model.agents.is_empty() {
        return Paragraph::new(Span::styled(
            "no agents configured",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let mut spans = vec![Span::styled(
        "agent: ",
        Style::default().fg(Color::DarkGray),
    )];
    for (i, agent) in model.agents.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(agent_segment(agent, i == model.agent_index));
    }
    if let Some(current) = model.agents.get(model.agent_index) {
        spans.push(Span::styled(
            format!("  ({})", current.cmd_preview),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        "  ctrl-a to cycle",
        Style::default().fg(Color::DarkGray),
    ));
    Paragraph::new(Line::from(spans))
}

fn agent_segment(agent: &AgentEntry, current: bool) -> Span<'_> {
    let text = format!(" {} ", agent.name);
    if current {
        Span::styled(
            text,
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(text, Style::default().fg(Color::DarkGray))
    }
}

fn status_line(model: &DashboardModel) -> Line<'static> {
    if let Some(pending) = &model.pending {
        let text = match pending.stage {
            Stage::Cloning => format!("cloning/pulling {}…", pending.ctx.repo_label),
            Stage::CloneHook | Stage::CreateHook => "running hook…".to_string(),
            Stage::CreatingWorktree => "creating worktree…".to_string(),
            Stage::Launching => format!("launching {}…", pending.ctx.agent),
        };
        return Line::from(text);
    }
    if let Some(status) = &model.status {
        return Line::from(status.clone());
    }
    help_line(model)
}

/// Key tokens colored per the plan's style spec — cyan for neutral/navigation,
/// yellow for destructive — description text plain, matching the pane
/// borders/dirty/idle/agent-segment coloring above rather than leaving this
/// one line as a plain unstyled string.
fn help_line(model: &DashboardModel) -> Line<'static> {
    let key = |k: &'static str| Span::styled(k, Style::default().fg(KEY_HINT));
    let destructive = |k: &'static str| Span::styled(k, Style::default().fg(KEY_HINT_DESTRUCTIVE));
    let plain = |s: &'static str| Span::raw(s);

    if model.delete_mode {
        Line::from(vec![
            key("↑/↓"),
            plain(" move · "),
            key("space"),
            plain("/click check · "),
            destructive("d"),
            plain(" confirm/back out · "),
            key("ctrl-a"),
            plain(" agent · "),
            destructive("esc"),
            plain(" cancel"),
        ])
    } else {
        Line::from(vec![
            key("↑/↓"),
            plain(" move · "),
            key("tab"),
            plain(" switch pane · type to filter · "),
            key("enter"),
            plain(" select · "),
            destructive("d"),
            plain(" delete-mode · "),
            key("ctrl-a"),
            plain(" agent · "),
            destructive("q"),
            plain("/"),
            destructive("esc"),
            plain(" quit"),
        ])
    }
}

fn draw_modal(frame: &mut Frame, modal: &Modal, full: Rect) {
    match modal {
        Modal::HookConsent { resolved, .. } => {
            let area = centered_rect(60, 30, full);
            frame.render_widget(Clear, area);
            let block = Block::default().title(" run hook? ").borders(Borders::ALL);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let lines = vec![
                Line::from(format!("{} {}", resolved.program, resolved.args.join(" "))),
                Line::from(format!("cwd: {}", resolved.cwd.display())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("y", Style::default().fg(KEY_HINT)),
                    Span::raw(" run   "),
                    Span::styled("n", Style::default().fg(KEY_HINT_DESTRUCTIVE)),
                    Span::raw("/esc skip"),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Modal::ConfirmDelete {
            total_count,
            dirty_count,
            force,
        } => {
            let area = centered_rect(50, 30, full);
            frame.render_widget(Clear, area);
            let block = Block::default()
                .title(" remove worktrees? ")
                .borders(Borders::ALL);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let dirty_note = if *dirty_count == 0 {
                String::new()
            } else if *force {
                format!(" ({dirty_count} dirty, --force will remove anyway)")
            } else {
                format!(" ({dirty_count} dirty — will be skipped, not --force)")
            };
            let lines = vec![
                Line::from(format!("remove {total_count} worktree(s)?{dirty_note}")),
                Line::from(""),
                Line::from(vec![
                    Span::styled("y", Style::default().fg(KEY_HINT_DESTRUCTIVE)),
                    Span::raw("/enter confirm   "),
                    Span::styled("n", Style::default().fg(KEY_HINT)),
                    Span::raw("/esc cancel"),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}
