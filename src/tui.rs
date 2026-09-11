use std::path::Path;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::plugins::{self, PluginStatus, State};

const HELP_TEXT: &str = "space: toggle  /: filter  o: sort  s: save  q: quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    Name,
    Type,
    Enabled,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::Name => SortMode::Type,
            SortMode::Type => SortMode::Enabled,
            SortMode::Enabled => SortMode::Name,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::Type => "type",
            SortMode::Enabled => "enabled",
        }
    }
}

/// Load-order-ish grouping: masters/light-masters before regular plugins,
/// derived from the file extension rather than the parsed TES4 flags so
/// entries missing from Data (no header to read) still sort sensibly.
fn type_rank(name: &str) -> u8 {
    match name.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "esm" => 0,
        "esl" => 1,
        "esp" => 2,
        _ => 3,
    }
}

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
    filter: String,
    editing_filter: bool,
    filter_before_edit: String,
    sort_mode: SortMode,
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
            message: HELP_TEXT.to_string(),
            confirm_discard: false,
            quit: false,
            save_on_exit: false,
            filter: String::new(),
            editing_filter: false,
            filter_before_edit: String::new(),
            sort_mode: SortMode::Name,
        }
    }

    fn dirty_count(&self) -> usize {
        self.rows.iter().filter(|r| r.pending.is_some()).count()
    }

    /// Indices into `rows` of the plugins currently matching `filter`, in
    /// the current sort order.
    fn filtered_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        let mut indices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| self.filter.is_empty() || r.status.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();

        indices.sort_by(|&a, &b| {
            let ra = &self.rows[a];
            let rb = &self.rows[b];
            let ordering = match self.sort_mode {
                SortMode::Name => std::cmp::Ordering::Equal,
                SortMode::Type => type_rank(&ra.status.name).cmp(&type_rank(&rb.status.name)),
                SortMode::Enabled => rb.effective_active().cmp(&ra.effective_active()),
            };
            ordering.then_with(|| ra.status.name.to_lowercase().cmp(&rb.status.name.to_lowercase()))
        });
        indices
    }

    /// Cycle to the next sort mode, keeping the currently selected plugin
    /// selected even though its position in the list changes.
    fn cycle_sort(&mut self) {
        let selected_row_idx = self
            .list_state
            .selected()
            .and_then(|pos| self.filtered_indices().get(pos).copied());

        self.sort_mode = self.sort_mode.next();

        let filtered = self.filtered_indices();
        let new_pos = selected_row_idx.and_then(|row_idx| filtered.iter().position(|&i| i == row_idx));
        self.list_state.select(new_pos.or(if filtered.is_empty() { None } else { Some(0) }));
        self.confirm_discard = false;
        self.message = format!("sorted by {}", self.sort_mode.label());
    }

    fn clamp_selection(&mut self, filtered_len: usize) {
        if filtered_len == 0 {
            self.list_state.select(None);
            return;
        }
        let current = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some(current.min(filtered_len - 1)));
    }

    fn move_selection(&mut self, delta: i32) {
        let filtered_len = self.filtered_indices().len();
        if filtered_len == 0 {
            return;
        }
        let len = filtered_len as i32;
        let current = self.list_state.selected().unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(len);
        self.list_state.select(Some(next as usize));
        self.confirm_discard = false;
    }

    fn toggle_selected(&mut self) {
        let filtered = self.filtered_indices();
        let Some(pos) = self.list_state.selected() else { return };
        let Some(&row_idx) = filtered.get(pos) else { return };
        let row = &mut self.rows[row_idx];
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
        self.message = HELP_TEXT.to_string();
    }

    fn pending_changes(&self) -> Vec<(String, bool)> {
        self.rows
            .iter()
            .filter_map(|r| r.pending.map(|want| (r.status.name.clone(), want)))
            .collect()
    }

    fn start_filter_edit(&mut self) {
        self.filter_before_edit = self.filter.clone();
        self.editing_filter = true;
        self.confirm_discard = false;
    }

    fn commit_filter_edit(&mut self) {
        self.editing_filter = false;
        let len = self.filtered_indices().len();
        self.clamp_selection(len);
    }

    fn cancel_filter_edit(&mut self) {
        self.filter = std::mem::take(&mut self.filter_before_edit);
        self.editing_filter = false;
        let len = self.filtered_indices().len();
        self.clamp_selection(len);
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header_area, list_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let filtered = self.filtered_indices();

        let dirty = self.dirty_count();
        let mut header_text = String::from("skyplug");
        header_text.push_str(&format!(" \u{2014} sort: {}", self.sort_mode.label()));
        if dirty > 0 {
            header_text.push_str(&format!(" \u{2014} {dirty} unsaved change(s)"));
        }
        if !self.filter.is_empty() {
            header_text.push_str(&format!(" \u{2014} filter \"{}\" ({}/{})", self.filter, filtered.len(), self.rows.len()));
        }
        frame.render_widget(
            Paragraph::new(header_text).style(Style::default().add_modifier(Modifier::BOLD)),
            header_area,
        );

        let items: Vec<ListItem> = filtered
            .iter()
            .map(|&idx| {
                let row = &self.rows[idx];
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

        let list_block = Block::default().borders(Borders::ALL).title("Plugins");
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(format!("no plugins match \"{}\"", self.filter)).block(list_block),
                list_area,
            );
        } else {
            let list = List::new(items)
                .block(list_block)
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> ");
            frame.render_stateful_widget(list, list_area, &mut self.list_state);
        }

        if self.editing_filter {
            frame.render_widget(Paragraph::new(format!("/{}_", self.filter)), footer_area);
        } else {
            frame.render_widget(Paragraph::new(self.message.as_str()), footer_area);
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        if self.editing_filter {
            self.handle_filter_key(code);
            return;
        }

        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_selected(),
            KeyCode::Char('/') => self.start_filter_edit(),
            KeyCode::Char('o') => self.cycle_sort(),
            KeyCode::Char('s') => {
                self.save_on_exit = true;
                self.quit = true;
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                let len = self.filtered_indices().len();
                self.clamp_selection(len);
                self.confirm_discard = false;
                self.message = HELP_TEXT.to_string();
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

    fn handle_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => self.commit_filter_edit(),
            KeyCode::Esc => self.cancel_filter_edit(),
            KeyCode::Backspace => {
                self.filter.pop();
                let len = self.filtered_indices().len();
                self.clamp_selection(len);
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                let len = self.filtered_indices().len();
                self.clamp_selection(len);
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
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
