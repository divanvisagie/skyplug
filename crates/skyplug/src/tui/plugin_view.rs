use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use skyplug_core::plugins::{PluginStatus, State};

const HELP_TEXT: &str = "space: toggle  gg/G: top/bottom  /: filter  o: sort  tab: saves  s: save  q: quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    Name,
    Type,
    Enabled,
    LoadOrder,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::Name => SortMode::Type,
            SortMode::Type => SortMode::Enabled,
            SortMode::Enabled => SortMode::LoadOrder,
            SortMode::LoadOrder => SortMode::Name,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::Type => "type",
            SortMode::Enabled => "enabled",
            SortMode::LoadOrder => "load order",
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

pub(super) struct Row {
    pub(super) status: PluginStatus,
    /// Desired active state if the user has toggled it away from `status.state`.
    pub(super) pending: Option<bool>,
}

impl Row {
    pub(super) fn effective_active(&self) -> bool {
        self.pending.unwrap_or(self.status.state == State::Enabled)
    }

    fn is_toggleable(&self) -> bool {
        !self.status.is_forced && self.status.state != State::Missing
    }
}

/// The Plugins tab: every plugin with its state, toggled in memory until
/// the app saves.
pub(super) struct PluginsView {
    rows: Vec<Row>,
    list_state: ListState,
    message: String,
    filter: String,
    editing_filter: bool,
    filter_before_edit: String,
    sort_mode: SortMode,
    /// Set after a lone `g` press, so a following `g` completes the vim
    /// `gg` (jump to top) chord. Cleared on any other key.
    pending_g: bool,
}

impl PluginsView {
    pub(super) fn new(statuses: Vec<PluginStatus>) -> Self {
        let rows: Vec<Row> = statuses.into_iter().map(|status| Row { status, pending: None }).collect();
        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            rows,
            list_state,
            message: HELP_TEXT.to_string(),
            filter: String::new(),
            editing_filter: false,
            filter_before_edit: String::new(),
            sort_mode: SortMode::LoadOrder,
            pending_g: false,
        }
    }

    pub(super) fn dirty_count(&self) -> usize {
        self.rows.iter().filter(|r| r.pending.is_some()).count()
    }

    /// True while typing a filter, when every key belongs to this view.
    pub(super) fn is_editing(&self) -> bool {
        self.editing_filter
    }

    /// The row for `name` (case-insensitive), if it's in Data or Plugins.txt.
    pub(super) fn row(&self, name: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.status.name.eq_ignore_ascii_case(name))
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
                SortMode::LoadOrder => ra
                    .status
                    .load_order
                    .unwrap_or(usize::MAX)
                    .cmp(&rb.status.load_order.unwrap_or(usize::MAX)),
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
    }

    fn select_first(&mut self) {
        if !self.filtered_indices().is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        let filtered_len = self.filtered_indices().len();
        if filtered_len > 0 {
            self.list_state.select(Some(filtered_len - 1));
        }
    }

    fn toggle_selected(&mut self) {
        let filtered = self.filtered_indices();
        let Some(pos) = self.list_state.selected() else { return };
        let Some(&row_idx) = filtered.get(pos) else { return };
        let row = &mut self.rows[row_idx];

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

    pub(super) fn pending_changes(&self) -> Vec<(String, bool)> {
        self.rows
            .iter()
            .filter_map(|r| r.pending.map(|want| (r.status.name.clone(), want)))
            .collect()
    }

    fn start_filter_edit(&mut self) {
        self.filter_before_edit = self.filter.clone();
        self.editing_filter = true;
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

    /// View-specific part of the header line.
    pub(super) fn header_info(&self) -> String {
        let mut info = format!("sort: {}", self.sort_mode.label());
        if !self.filter.is_empty() {
            let shown = self.filtered_indices().len();
            info.push_str(&format!(" \u{2014} filter \"{}\" ({shown}/{})", self.filter, self.rows.len()));
        }
        info
    }

    pub(super) fn footer(&self) -> String {
        if self.editing_filter { format!("/{}_", self.filter) } else { self.message.clone() }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .filtered_indices()
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
                let order_label = match row.status.load_order {
                    Some(n) => format!("{n:>3}"),
                    None => "  -".to_string(),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{order_label} "), Style::default().fg(Color::DarkGray)),
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
                area,
            );
        } else {
            let list = List::new(items)
                .block(list_block)
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> ");
            frame.render_stateful_widget(list, area, &mut self.list_state);
        }
    }

    /// Handle a key the app didn't claim. Returns false only for an `Esc`
    /// with nothing to clear, so the app can treat it as quit.
    pub(super) fn handle_key(&mut self, code: KeyCode) -> bool {
        if self.editing_filter {
            self.handle_filter_key(code);
            return true;
        }

        if !matches!(code, KeyCode::Char('g')) {
            self.pending_g = false;
        }

        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_selected(),
            KeyCode::Char('/') => self.start_filter_edit(),
            KeyCode::Char('o') => self.cycle_sort(),
            KeyCode::Char('G') => self.select_last(),
            KeyCode::Char('g') => {
                if self.pending_g {
                    self.pending_g = false;
                    self.select_first();
                } else {
                    self.pending_g = true;
                }
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                let len = self.filtered_indices().len();
                self.clamp_selection(len);
                self.message = HELP_TEXT.to_string();
            }
            KeyCode::Esc => return false,
            _ => {}
        }
        true
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
