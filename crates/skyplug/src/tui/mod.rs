//! Interactive TUI with two tabs: Plugins (toggle plugins, saved to
//! Plugins.txt on `s`) and Saves (browse characters, their saves, and the
//! plugins each save was made with).

mod plugin_view;
mod save_view;

use std::path::Path;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use skyplug_core::plugins::{self, PluginStatus};

use plugin_view::PluginsView;
use save_view::SavesView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Plugins,
    Saves,
}

struct App {
    tab: Tab,
    plugins: PluginsView,
    saves: SavesView,
    confirm_discard: bool,
    quit: bool,
    save_on_exit: bool,
}

impl App {
    fn switch_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Plugins => Tab::Saves,
            Tab::Saves => Tab::Plugins,
        };
        if self.tab == Tab::Saves {
            self.saves.ensure_loaded();
        }
    }

    /// Quit, unless there are unsaved changes and this is the first ask.
    fn request_quit(&mut self, confirming: bool) {
        if self.plugins.dirty_count() > 0 && !confirming {
            self.confirm_discard = true;
        } else {
            self.quit = true;
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        // Typing a filter: every key is text, including q/s/tab.
        if self.tab == Tab::Plugins && self.plugins.is_editing() {
            self.plugins.handle_key(code);
            return;
        }

        // Any key other than a second q/esc cancels a pending discard prompt.
        let confirming = std::mem::take(&mut self.confirm_discard);
        match code {
            KeyCode::Tab | KeyCode::BackTab => self.switch_tab(),
            KeyCode::Char('s') => {
                self.save_on_exit = true;
                self.quit = true;
            }
            KeyCode::Char('q') => self.request_quit(confirming),
            _ => {
                let handled = match self.tab {
                    Tab::Plugins => self.plugins.handle_key(code),
                    Tab::Saves => self.saves.handle_key(code),
                };
                if !handled && code == KeyCode::Esc {
                    self.request_quit(confirming);
                }
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header_area, body_area, footer_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

        let tab_style = |tab: Tab| {
            if self.tab == tab {
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        };
        let info = match self.tab {
            Tab::Plugins => self.plugins.header_info(),
            Tab::Saves => self.saves.header_info(),
        };
        let mut header = vec![
            Span::styled("skyplug ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(" Plugins ", tab_style(Tab::Plugins)),
            Span::raw(" "),
            Span::styled(" Saves ", tab_style(Tab::Saves)),
            Span::raw(format!("  {info}")),
        ];
        let dirty = self.plugins.dirty_count();
        if dirty > 0 {
            header.push(Span::styled(
                format!(" \u{2014} {dirty} unsaved change(s)"),
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(header)), header_area);

        match self.tab {
            Tab::Plugins => self.plugins.draw(frame, body_area),
            Tab::Saves => self.saves.draw(frame, body_area, &self.plugins),
        }

        let footer = if self.confirm_discard {
            "unsaved changes \u{2014} press q again to discard, or s to save".to_string()
        } else {
            match self.tab {
                Tab::Plugins => self.plugins.footer(),
                Tab::Saves => self.saves.footer(),
            }
        };
        frame.render_widget(Paragraph::new(footer), footer_area);
    }
}

/// Run the interactive TUI. Returns the number of plugin changes saved to
/// Plugins.txt (0 if the user quit without saving).
pub fn run(plugins_txt: &Path, saves_dir: &Path, statuses: Vec<PluginStatus>) -> Result<usize> {
    let mut app = App {
        tab: Tab::Plugins,
        plugins: PluginsView::new(statuses),
        saves: SavesView::new(saves_dir.to_path_buf()),
        confirm_discard: false,
        quit: false,
        save_on_exit: false,
    };

    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut app);
    ratatui::restore();
    result?;

    if !app.save_on_exit || app.plugins.dirty_count() == 0 {
        return Ok(0);
    }

    plugins::apply_changes(plugins_txt, &app.plugins.pending_changes())
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
