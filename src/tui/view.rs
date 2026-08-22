//! Pure rendering — `draw_repo`/`draw_worktree` take `&Model` and paint one
//! frame. Never mutate model state directly (see `Screen::draw`'s doc
//! comment): geometry a later click needs is cached through the `Model`'s
//! own interior mutability (`ListState::table`/`table_rect`), not by
//! widening these signatures.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table};
use ratatui::Frame;

use super::model::{
    relative_time, AgentModel, AgentRow, ListState, RepoModel, WorktreeMode, WorktreeModel,
};
use crate::picker::WorktreeSelection;

const HELP_PICK: &str = "↑/↓ move · type to filter · Enter select · Esc cancel · Ctrl-C quit";
const HELP_MULTI: &str = "↑/↓ move · Space/click toggle · d delete checked · Esc cancel";
const HELP_AGENT: &str = "↑/↓ move · type to filter · Enter choose agent · Esc back · Ctrl-C quit";

/// Splits the full frame into title / filter-box / table / status rows.
fn chrome(area: Rect) -> [Rect; 4] {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    [chunks[0], chunks[1], chunks[2], chunks[3]]
}

fn filter_line(query: &str) -> Paragraph<'_> {
    Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(query),
    ]))
}

fn header_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn selected_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

pub fn draw_repo(frame: &mut Frame, model: &RepoModel) {
    let [title_area, filter_area, table_area, status_area] = chrome(frame.area());

    let title = if model.loading {
        if model.list.items.is_empty() {
            "cw — loading repos…"
        } else {
            "cw — pick a repo (refreshing…)"
        }
    } else {
        "cw — pick a repo"
    };
    frame.render_widget(Paragraph::new(title), title_area);
    frame.render_widget(filter_line(&model.list.query), filter_area);

    model.list.table_rect.set(table_area);

    if model.list.items.is_empty() {
        let msg = if model.loading {
            ""
        } else {
            "no repos found — check `gh auth status` or your --org filter"
        };
        frame.render_widget(Paragraph::new(msg), table_area);
    } else {
        let now = chrono::Utc::now();
        let header = Row::new(vec!["NAME", "OWNER", "UPDATED", "LOCAL"]).style(header_style());
        let rows: Vec<Row> = model
            .list
            .filtered
            .iter()
            .filter_map(|&idx| model.list.items.get(idx))
            .map(|row| {
                Row::new(vec![
                    row.repo.name.clone(),
                    row.repo.owner.clone(),
                    relative_time(&row.repo.updated_at, now),
                    if row.local {
                        "✓".to_string()
                    } else {
                        String::new()
                    },
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

        let mut table_state = model.list.table.borrow_mut();
        frame.render_stateful_widget(table, table_area, &mut table_state);
    }

    let status = model.status.as_deref().unwrap_or(HELP_PICK);
    frame.render_widget(Paragraph::new(status), status_area);
}

pub fn draw_worktree(frame: &mut Frame, model: &WorktreeModel) {
    let [title_area, filter_area, table_area, status_area] = chrome(frame.area());
    let is_multi = matches!(model.mode, WorktreeMode::Multi);

    let title = if is_multi {
        "cw clean — select worktrees to remove"
    } else {
        "cw — pick a worktree"
    };
    frame.render_widget(Paragraph::new(title), title_area);
    frame.render_widget(filter_line(&model.list.query), filter_area);

    model.list.table_rect.set(table_area);
    draw_worktree_table(frame, model, table_area, is_multi);

    let help = if is_multi { HELP_MULTI } else { HELP_PICK };
    let status = model.status.as_deref().unwrap_or(help);
    frame.render_widget(Paragraph::new(status), status_area);

    if let WorktreeMode::Single {
        agent_overlay: true,
        agents,
        ..
    } = &model.mode
    {
        draw_agent_overlay(frame, agents, frame.area());
    }
}

fn draw_worktree_table(frame: &mut Frame, model: &WorktreeModel, area: Rect, is_multi: bool) {
    if model.list.items.is_empty() {
        frame.render_widget(Paragraph::new("no worktrees yet"), area);
        return;
    }

    let mut header_cells = vec!["REPO", "SLUG", "IDLE", "STATUS"];
    if is_multi {
        header_cells.insert(0, "");
    }
    let header = Row::new(header_cells).style(header_style());

    let rows: Vec<Row> = model
        .list
        .filtered
        .iter()
        .filter_map(|&idx| model.list.items.get(idx).map(|row| (idx, row)))
        .map(|(idx, row)| {
            let (repo_cell, slug_cell) = match &row.selection {
                WorktreeSelection::Existing(entry) => (row.repo_label.clone(), entry.slug.clone()),
                WorktreeSelection::New => (String::new(), "+ new worktree".to_string()),
            };
            let idle_cell = row.idle_label.clone().unwrap_or_default();
            let status_cell = if row.dirty {
                "dirty".to_string()
            } else {
                String::new()
            };

            let mut cells = vec![repo_cell, slug_cell, idle_cell, status_cell];
            if is_multi {
                let mark = if model.checked.contains(&idx) {
                    "[x]"
                } else {
                    "[ ]"
                };
                cells.insert(0, mark.to_string());
            }

            let built = Row::new(cells);
            if row.dirty {
                // Marked up front — the multi-select "why didn't this
                // delete" case is visible before the user ever presses `d`,
                // not only reported after the fact by `clean.rs`.
                built.style(Style::default().fg(Color::Red))
            } else {
                built
            }
        })
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

    let mut table_state = model.list.table.borrow_mut();
    frame.render_stateful_widget(table, area, &mut table_state);
}

/// Inline agent sub-panel: a centered popup over the (still-visible-behind-
/// it) worktree table, rather than tearing this screen down and relaunching
/// a second full-screen picker.
fn draw_agent_overlay(frame: &mut Frame, agents: &ListState<AgentRow>, full: Rect) {
    let area = centered_rect(60, 50, full);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" choose an agent ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let overlay_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let (filter_area, table_area, status_area) =
        (overlay_chunks[0], overlay_chunks[1], overlay_chunks[2]);

    frame.render_widget(filter_line(&agents.query), filter_area);
    agents.table_rect.set(table_area);
    draw_agent_list(frame, agents, table_area);

    frame.render_widget(Paragraph::new(HELP_AGENT), status_area);
}

/// `picker::pick_agent`'s standalone top-level screen — the fallback used
/// when agent resolution is needed independently of a worktree choice (see
/// that function's doc comment). Same row rendering as the worktree
/// screen's inline sub-panel (`draw_agent_list`), just full-frame instead
/// of a centered popup, and Esc cancels outright rather than backing out to
/// a parent screen (there is no parent here).
pub fn draw_agent(frame: &mut Frame, model: &AgentModel) {
    let [title_area, filter_area, table_area, status_area] = chrome(frame.area());
    frame.render_widget(Paragraph::new("cw — pick an agent"), title_area);
    frame.render_widget(filter_line(&model.list.query), filter_area);
    model.list.table_rect.set(table_area);
    draw_agent_list(frame, &model.list, table_area);
    frame.render_widget(Paragraph::new(HELP_PICK), status_area);
}

fn draw_agent_list(frame: &mut Frame, agents: &ListState<AgentRow>, area: Rect) {
    if agents.items.is_empty() {
        frame.render_widget(
            Paragraph::new("no agents configured — check [agents] in config.toml"),
            area,
        );
        return;
    }
    let header = Row::new(vec!["NAME", "COMMAND"]).style(header_style());
    let rows: Vec<Row> = agents
        .filtered
        .iter()
        .filter_map(|&idx| agents.items.get(idx))
        .map(|row| Row::new(vec![row.name.clone(), row.cmd_preview.clone()]))
        .collect();
    let table = Table::new(
        rows,
        [Constraint::Percentage(40), Constraint::Percentage(60)],
    )
    .header(header)
    .row_highlight_style(selected_style())
    .highlight_symbol("> ");
    let mut table_state = agents.table.borrow_mut();
    frame.render_stateful_widget(table, area, &mut table_state);
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
