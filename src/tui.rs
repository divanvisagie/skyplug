use std::path::Path;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::plugins::{self, PluginStatus, State};

struct Row {
    status: PluginStatus,
    /// Desired active state if the user has toggled it away from `status.state`.
    pending: Option<bool>,
}

impl Row {
    fn effective_active(&self) -> bool {
        self.pending.unwrap_or(self.status.state == State::Enabled)
    }

    fn is_toggleable(&self) -> bool {
        !self.status.is_forced && self.status.state != State::Missing
    }
}

struct App {
    rows: Vec<Row>,
    list_state: ListState,
    message: String,
    confirm_discard: bool,
    quit: bool,
    save_on_exit: bool,
}

impl App {
    fn new(rows: Vec<Row>) -> Self {
        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            rows,
            list_state,
            message: "space: toggle  s: save  q: quit".to_string(),
            confirm_discard: false,
            quit: false,
            save_on_exit: false,
        }
    }

    fn dirty_count(&self) -> usize {
        self.rows.iter().filter(|r| r.pending.is_some()).count()
    }

    fn move_selection(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as i32;
        let current = self.list_state.selected().unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(len);
        self.list_state.select(Some(next as usize));
        self.confirm_discard = false;
    }

    fn toggle_selected(&mut self) {
        let Some(idx) = self.list_state.selected() else { return };
        let row = &mut self.rows[idx];
        self.confirm_discard = false;

        if !row.is_toggleable() {
            self.message = if row.status.state == State::Missing {
                format!("{} is missing from Data, nothing to toggle", row.status.name)
            } else {
                format!(
                    "{} is a master/light-master plugin \u{2014} always loaded, toggling has no effect",
                    row.status.name
                )
            };
            return;
        }

        let currently_active = row.status.state == State::Enabled;
        let new_effective = !row.effective_active();
        row.pending = if new_effective == currently_active { None } else { Some(new_effective) };
        self.message = "space: toggle  s: save  q: quit".to_string();
    }

    fn pending_changes(&self) -> Vec<(String, bool)> {
        self.rows
            .iter()
            .filter_map(|r| r.pending.map(|want| (r.status.name.clone(), want)))
            .collect()
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header_area, list_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let dirty = self.dirty_count();
        let header_text = if dirty > 0 {
            format!("skyplug \u{2014} {dirty} unsaved change(s)")
        } else {
            "skyplug".to_string()
        };
        frame.render_widget(
            Paragraph::new(header_text).style(Style::default().add_modifier(Modifier::BOLD)),
            header_area,
        );

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| {
                let active = row.effective_active();
                let (symbol, mut color) = if row.status.is_forced {
                    (if row.status.is_cc { "[CC]" } else { "[M]" }, Color::Cyan)
                } else if row.status.state == State::Missing {
                    ("[!]", Color::Red)
                } else if active {
                    ("[x]", Color::Green)
                } else {
                    ("[ ]", Color::DarkGray)
                };
                if row.pending.is_some() {
                    color = Color::Yellow;
                }
                let marker = if row.pending.is_some() { "*" } else { " " };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{symbol} "), Style::default().fg(color)),
                    Span::raw(row.status.name.clone()),
                    Span::styled(marker, Style::default().fg(Color::Yellow)),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Plugins"))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, list_area, &mut self.list_state);

        frame.render_widget(Paragraph::new(self.message.as_str()), footer_area);
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_selected(),
            KeyCode::Char('s') => {
                self.save_on_exit = true;
                self.quit = true;
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.dirty_count() > 0 && !self.confirm_discard {
                    self.confirm_discard = true;
                    self.message =
                        "unsaved changes \u{2014} press q again to discard, or s to save".to_string();
                } else {
                    self.quit = true;
                }
            }
            _ => {}
        }
    }
}

/// Run the interactive plugin editor. Returns the number of changes saved
/// (0 if the user quit without saving).
pub fn run(plugins_txt: &Path, statuses: Vec<PluginStatus>) -> Result<usize> {
    let rows = statuses.into_iter().map(|status| Row { status, pending: None }).collect();
    let mut app = App::new(rows);

    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut app);
    ratatui::restore();
    result?;

    if !app.save_on_exit || app.dirty_count() == 0 {
        return Ok(0);
    }

    let changes = app.pending_changes();
    plugins::apply_changes(plugins_txt, &changes)
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|frame| app.draw(frame))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            app.handle_key(key.code);
        }

        if app.quit {
            return Ok(());
        }
    }
}
