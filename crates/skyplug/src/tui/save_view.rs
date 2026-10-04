use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use skyplug_core::plugins::State;
use skyplug_core::saves::{self, SaveEntry};

use super::plugin_view::PluginsView;

const HELP_TEXT: &str = "enter: open  esc: back  gg/G: top/bottom  tab: plugins  s: save  q: quit";

/// Where in the characters → saves → plugins drill-down the view is.
enum Level {
    Characters,
    Saves { character: String },
    /// `save` indexes `SavesView::entries`.
    Plugins { character: String, save: usize },
}

/// The Saves tab: browse characters, their saves, and the plugins each
/// save was made with, marked against the current (unsaved) plugin state.
pub(super) struct SavesView {
    saves_dir: PathBuf,
    /// Saves are only read the first time the tab is opened, so a missing
    /// or unreadable saves folder never gets in the way of plugin editing.
    loaded: bool,
    load_error: Option<String>,
    /// Newest first.
    entries: Vec<SaveEntry>,
    /// Full save parses (the body is LZ4-decompressed), keyed by entry
    /// index, so re-opening a save is instant.
    plugin_lists: HashMap<usize, Result<Vec<String>, String>>,
    level: Level,
    characters_state: ListState,
    saves_state: ListState,
    plugins_state: ListState,
    message: String,
    pending_g: bool,
}

/// How a plugin a save was made with relates to the current setup.
fn plugin_tag(plugins: &PluginsView, name: &str) -> (&'static str, &'static str, Color) {
    let Some(row) = plugins.row(name) else {
        return ("[!]", "missing from Data", Color::Red);
    };
    let status = &row.status;
    if status.is_forced {
        if status.is_cc { ("[CC]", "creation club", Color::Cyan) } else { ("[M]", "base game master, always loaded", Color::Cyan) }
    } else if status.state == State::Missing {
        ("[!]", "in Plugins.txt but missing from Data", Color::Red)
    } else if row.effective_active() {
        ("[x]", "installed and active", Color::Green)
    } else {
        ("[ ]", "installed but disabled", Color::DarkGray)
    }
}

fn when(entry: &SaveEntry) -> String {
    saves::save_timestamp(&entry.file_name).unwrap_or_else(|| "unknown time".to_string())
}

fn list_block(title: String) -> Block<'static> {
    Block::default().borders(Borders::ALL).title(title)
}

fn render_list(frame: &mut Frame, area: Rect, items: Vec<ListItem>, block: Block, state: &mut ListState) {
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, state);
}

impl SavesView {
    pub(super) fn new(saves_dir: PathBuf) -> Self {
        Self {
            saves_dir,
            loaded: false,
            load_error: None,
            entries: Vec::new(),
            plugin_lists: HashMap::new(),
            level: Level::Characters,
            characters_state: ListState::default(),
            saves_state: ListState::default(),
            plugins_state: ListState::default(),
            message: HELP_TEXT.to_string(),
            pending_g: false,
        }
    }

    /// Read every save's header, once.
    pub(super) fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        match saves::list_saves(&self.saves_dir) {
            Ok(list) => {
                if !list.skipped.is_empty() {
                    self.message = format!("skipped {} unreadable save(s) \u{2014} {HELP_TEXT}", list.skipped.len());
                }
                self.entries = list.entries;
                if !self.entries.is_empty() {
                    self.characters_state.select(Some(0));
                }
            }
            Err(e) => self.load_error = Some(format!("{e:#}")),
        }
    }

    /// Indices into `entries` of `character`'s saves, newest first.
    fn saves_of(&self, character: &str) -> Vec<usize> {
        (0..self.entries.len()).filter(|&i| self.entries[i].header.player_name == character).collect()
    }

    fn current_len(&self) -> usize {
        match &self.level {
            Level::Characters => saves::characters(&self.entries).len(),
            Level::Saves { character } => self.saves_of(character).len(),
            Level::Plugins { save, .. } => match self.plugin_lists.get(save) {
                Some(Ok(plugins)) => plugins.len(),
                _ => 0,
            },
        }
    }

    fn current_state(&mut self) -> &mut ListState {
        match self.level {
            Level::Characters => &mut self.characters_state,
            Level::Saves { .. } => &mut self.saves_state,
            Level::Plugins { .. } => &mut self.plugins_state,
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.current_len() as i32;
        if len == 0 {
            return;
        }
        let state = self.current_state();
        let current = state.selected().unwrap_or(0) as i32;
        state.select(Some((current + delta).rem_euclid(len) as usize));
    }

    fn select_edge(&mut self, last: bool) {
        let len = self.current_len();
        if len > 0 {
            self.current_state().select(Some(if last { len - 1 } else { 0 }));
        }
    }

    /// Drill into the selected character or save.
    fn open(&mut self) {
        match &self.level {
            Level::Characters => {
                let Some(pos) = self.characters_state.selected() else { return };
                let Some(character) = saves::characters(&self.entries).get(pos).map(|c| c.name.to_string()) else {
                    return;
                };
                self.saves_state.select(Some(0));
                self.level = Level::Saves { character };
            }
            Level::Saves { character } => {
                let Some(pos) = self.saves_state.selected() else { return };
                let Some(&save) = self.saves_of(character).get(pos) else { return };
                let path = &self.entries[save].path;
                self.plugin_lists
                    .entry(save)
                    .or_insert_with(|| saves::read_plugins_from_file(path).map_err(|e| format!("{e:#}")));
                self.plugins_state.select(Some(0));
                self.level = Level::Plugins { character: character.clone(), save };
            }
            Level::Plugins { .. } => {}
        }
    }

    /// Go up a level. Returns false if already at the top.
    fn back(&mut self) -> bool {
        self.level = match std::mem::replace(&mut self.level, Level::Characters) {
            Level::Characters => return false,
            Level::Saves { .. } => Level::Characters,
            Level::Plugins { character, .. } => Level::Saves { character },
        };
        true
    }

    /// Breadcrumb for the header line.
    pub(super) fn header_info(&self) -> String {
        match &self.level {
            Level::Characters => {
                let count = saves::characters(&self.entries).len();
                format!("{count} character(s), {} save(s)", self.entries.len())
            }
            Level::Saves { character } => format!("characters \u{203a} {character}"),
            Level::Plugins { character, save } => {
                format!("characters \u{203a} {character} \u{203a} save #{}", self.entries[*save].header.save_number)
            }
        }
    }

    pub(super) fn footer(&self) -> String {
        self.message.clone()
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, plugins: &PluginsView) {
        if let Some(error) = &self.load_error {
            frame.render_widget(Paragraph::new(error.as_str()).wrap(Wrap { trim: false }).block(list_block("Saves".into())), area);
            return;
        }
        if self.entries.is_empty() {
            let text = format!("no saves found in {}", self.saves_dir.display());
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(list_block("Saves".into())), area);
            return;
        }

        match &self.level {
            Level::Characters => self.draw_characters(frame, area),
            Level::Saves { character } => {
                let character = character.clone();
                self.draw_saves(frame, area, &character);
            }
            Level::Plugins { save, .. } => {
                let save = *save;
                self.draw_plugins(frame, area, save, plugins);
            }
        }
    }

    fn draw_characters(&mut self, frame: &mut Frame, area: Rect) {
        let characters = saves::characters(&self.entries);
        let name_width = characters.iter().map(|c| c.name.len()).max().unwrap_or(0);
        let race_width = characters.iter().map(|c| c.latest.header.player_race_editor_id.len()).max().unwrap_or(0);
        let items: Vec<ListItem> = characters
            .iter()
            .map(|c| {
                let h = &c.latest.header;
                let plural = if c.save_count == 1 { "save" } else { "saves" };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<name_width$}  ", c.name), Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(format!(
                        "{:<race_width$}  level {:<3} {:>4} {plural:<5}  ",
                        h.player_race_editor_id, h.player_level, c.save_count
                    )),
                    Span::styled(format!("latest {}", when(c.latest)), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        render_list(frame, area, items, list_block("Characters".into()), &mut self.characters_state);
    }

    fn draw_saves(&mut self, frame: &mut Frame, area: Rect, character: &str) {
        let indices = self.saves_of(character);
        let location_width = indices.iter().map(|&i| self.entries[i].header.player_location.len()).max().unwrap_or(0);
        let items: Vec<ListItem> = indices
            .iter()
            .map(|&i| {
                let entry = &self.entries[i];
                let h = &entry.header;
                ListItem::new(Line::from(vec![
                    Span::raw(format!(
                        "#{:<4} level {:<3} {:<location_width$}  day {:<10} ",
                        h.save_number, h.player_level, h.player_location, h.game_date
                    )),
                    Span::styled(when(entry), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let title = format!("Saves \u{2014} {character}");
        render_list(frame, area, items, list_block(title), &mut self.saves_state);
    }

    fn draw_plugins(&mut self, frame: &mut Frame, area: Rect, save: usize, plugins: &PluginsView) {
        let entry = &self.entries[save];
        let h = &entry.header;
        let [info_area, list_area] = Layout::vertical([Constraint::Length(4), Constraint::Min(3)]).areas(area);

        let mut info = vec![
            Line::from(format!(
                "{}, level {} {} \u{2014} {}, in-game day {}",
                h.player_name, h.player_level, h.player_race_editor_id, h.player_location, h.game_date
            )),
            Line::styled(format!("{} \u{2014} {}", entry.file_name, when(entry)), Style::default().fg(Color::DarkGray)),
        ];

        let plugin_list = match &self.plugin_lists[&save] {
            Ok(list) => list,
            Err(error) => {
                info.push(Line::styled(format!("couldn't read plugins: {error}"), Style::default().fg(Color::Red)));
                frame.render_widget(Paragraph::new(info).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL)), area);
                return;
            }
        };

        let tags: Vec<_> = plugin_list.iter().map(|p| plugin_tag(plugins, p)).collect();
        let count = |symbol: &str| tags.iter().filter(|(s, _, _)| *s == symbol).count();
        let (missing, disabled) = (count("[!]"), count("[ ]"));
        let summary = if missing + disabled == 0 {
            Line::styled("every plugin this save uses is loaded", Style::default().fg(Color::Green))
        } else {
            Line::styled(
                format!("{missing} missing, {disabled} disabled \u{2014} load this save and those plugins won't be there"),
                Style::default().fg(Color::Yellow),
            )
        };
        info.push(summary);
        frame.render_widget(Paragraph::new(info).block(Block::default().borders(Borders::TOP)), info_area);

        let name_width = plugin_list.iter().map(|p| p.len()).max().unwrap_or(0);
        let items: Vec<ListItem> = plugin_list
            .iter()
            .zip(&tags)
            .map(|(name, &(symbol, note, color))| {
                let pending = plugins.row(name).is_some_and(|r| r.pending.is_some());
                let (color, note) = if pending { (Color::Yellow, format!("{note} (unsaved)")) } else { (color, note.to_string()) };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{name:<name_width$}  ")),
                    Span::styled(format!("{symbol:<4} "), Style::default().fg(color)),
                    Span::styled(note, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let title = format!("Plugins in save ({})", plugin_list.len());
        render_list(frame, list_area, items, list_block(title), &mut self.plugins_state);
    }

    /// Handle a key the app didn't claim. Returns false only for an `Esc`
    /// at the top level, so the app can treat it as quit.
    pub(super) fn handle_key(&mut self, code: KeyCode) -> bool {
        if !matches!(code, KeyCode::Char('g')) {
            self.pending_g = false;
        }

        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open(),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                self.back();
            }
            KeyCode::Esc => return self.back(),
            KeyCode::Char('G') => self.select_edge(true),
            KeyCode::Char('g') => {
                if self.pending_g {
                    self.pending_g = false;
                    self.select_edge(false);
                } else {
                    self.pending_g = true;
                }
            }
            _ => {}
        }
        true
    }
}
