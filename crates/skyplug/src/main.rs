mod tui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use skyplug_core::plugins::{self, ChangeResult, State};
use skyplug_core::{GameInstall, GamePaths, saves, steam};

/// Scan and toggle Skyrim Special Edition plugins on Linux/Steam.
#[derive(Parser)]
#[command(name = "skyplug", version, about)]
struct Cli {
    /// Skyrim Special Edition game directory (contains SkyrimSE.exe).
    /// Auto-detected from Steam libraries when omitted.
    #[arg(long, global = true)]
    game_dir: Option<PathBuf>,

    /// Steam AppID, used to locate the Proton prefix holding Plugins.txt.
    #[arg(long, global = true, default_value = steam::DEFAULT_APPID)]
    appid: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List every plugin in Data with its enabled/disabled/missing state.
    ///
    /// Each line is prefixed with a status marker:
    ///   [x]  enabled
    ///   [ ]  disabled
    ///   [!]  missing from Data (listed in Plugins.txt but the file isn't there)
    ///   [M]  master/light-master — the engine always loads it regardless of Plugins.txt
    ///   [CC] Creation Club content — always loaded via Skyrim.ccc regardless of Plugins.txt
    #[command(verbatim_doc_comment)]
    List,
    /// Enable a plugin (adds/sets the `*` prefix in Plugins.txt).
    Enable { plugin: String },
    /// Disable a plugin (removes the `*` prefix in Plugins.txt).
    Disable { plugin: String },
    /// Interactive TUI to enable/disable plugins and browse saves.
    Edit,
    /// Show the resolved game/Plugins.txt paths and exit.
    Paths,
    /// List every character across all saves, with their latest save.
    Characters,
    /// List every save belonging to a character (exact or substring match
    /// on the character/player name).
    Saves { character: String },
    /// List the plugins that were active in a specific save (exact
    /// filename, filename without `.ess`, or a substring).
    SavePlugins { save: String },
}

fn resolve(cli: &Cli) -> Result<GamePaths> {
    let install = match &cli.game_dir {
        Some(game_dir) => GameInstall::from_game_dir(game_dir).context("--game-dir")?,
        None => steam::find_game_install(&cli.appid)?,
    };
    Ok(GamePaths::new(&install, &cli.appid))
}

/// List saves, reporting any that couldn't be parsed on stderr.
fn list_saves(resolved: &GamePaths) -> Result<Vec<saves::SaveEntry>> {
    let list = saves::list_saves(&resolved.saves_dir)?;
    for skipped in &list.skipped {
        eprintln!("warning: skipping {}: {:#}", skipped.file_name, skipped.error);
    }
    Ok(list.entries)
}

fn state_label(status: &plugins::PluginStatus) -> &'static str {
    if status.is_forced {
        return if status.is_cc { "[CC]" } else { "[M]" };
    }
    match status.state {
        State::Enabled => "[x]",
        State::Disabled => "[ ]",
        State::Missing => "[!]",
    }
}

fn cmd_list(resolved: &GamePaths) -> Result<()> {
    let statuses = resolved.plugin_status()?;

    for status in &statuses {
        println!("{} {}", state_label(status), status.name);
    }

    let enabled = statuses.iter().filter(|s| s.state == State::Enabled && !s.is_forced).count();
    let disabled = statuses.iter().filter(|s| s.state == State::Disabled).count();
    let missing = statuses.iter().filter(|s| s.state == State::Missing).count();
    let forced = statuses.iter().filter(|s| s.is_forced).count();
    println!(
        "\n{} enabled, {} disabled, {} always-loaded (master/CC), {} missing ({} total)",
        enabled,
        disabled,
        forced,
        missing,
        statuses.len()
    );

    Ok(())
}

fn warn_if_forced(data_plugins: &[plugins::DataPlugin], exact: &str) {
    if let Some(p) = data_plugins.iter().find(|p| p.name == exact) {
        if p.is_forced {
            eprintln!(
                "note: {exact} is a master/light-master plugin — the engine always loads it regardless of Plugins.txt"
            );
        }
    }
}

fn cmd_enable(resolved: &GamePaths, plugin: &str) -> Result<()> {
    let data_plugins = plugins::scan_data_plugins(&resolved.data_dir)?;
    let exact = plugins::resolve_plugin_name(plugin, &data_plugins)?.to_string();
    warn_if_forced(&data_plugins, &exact);

    match plugins::set_plugin_enabled(&resolved.plugins_txt, &exact, true)? {
        ChangeResult::AlreadyInState => println!("{exact} is already enabled"),
        ChangeResult::Toggled => println!("enabled {exact}"),
        ChangeResult::AddedNew => println!("added {exact} to Plugins.txt and enabled it"),
        ChangeResult::NoOpDisableUntracked => unreachable!(),
    }
    Ok(())
}

fn cmd_disable(resolved: &GamePaths, plugin: &str) -> Result<()> {
    let data_plugins = plugins::scan_data_plugins(&resolved.data_dir)?;
    let exact = plugins::resolve_plugin_name(plugin, &data_plugins)?.to_string();
    warn_if_forced(&data_plugins, &exact);

    match plugins::set_plugin_enabled(&resolved.plugins_txt, &exact, false)? {
        ChangeResult::AlreadyInState => println!("{exact} is already disabled"),
        ChangeResult::Toggled => println!("disabled {exact}"),
        ChangeResult::NoOpDisableUntracked => println!("{exact} isn't listed in Plugins.txt, nothing to disable"),
        ChangeResult::AddedNew => unreachable!(),
    }
    Ok(())
}

fn cmd_edit(resolved: &GamePaths) -> Result<()> {
    let statuses = resolved.plugin_status()?;

    let saved = tui::run(&resolved.plugins_txt, &resolved.saves_dir, statuses)?;
    if saved > 0 {
        println!("saved {saved} change(s) to {}", resolved.plugins_txt.display());
    } else {
        println!("no changes saved");
    }
    Ok(())
}

fn cmd_paths(cli: &Cli, resolved: &GamePaths) -> Result<()> {
    println!("appid:       {}", cli.appid);
    println!("data dir:    {}", resolved.data_dir.display());
    println!("plugins.txt: {}", resolved.plugins_txt.display());
    println!("skyrim.ccc:  {}", resolved.ccc_path.display());
    println!("saves dir:   {}", resolved.saves_dir.display());
    Ok(())
}

fn cmd_characters(resolved: &GamePaths) -> Result<()> {
    let entries = list_saves(resolved)?;
    if entries.is_empty() {
        println!("no saves found in {}", resolved.saves_dir.display());
        return Ok(());
    }

    for character in saves::characters(&entries) {
        let last = &character.latest.header;
        let when = saves::save_timestamp(&character.latest.file_name).unwrap_or_else(|| "unknown time".to_string());
        let count = character.save_count;
        let plural = if count == 1 { "save" } else { "saves" };
        println!(
            "{} ({}) — {count} {plural}, latest: level {} at {} on {}, {when}",
            character.name, last.player_race_editor_id, last.player_level, last.player_location, last.game_date
        );
    }
    Ok(())
}

fn cmd_saves(resolved: &GamePaths, character: &str) -> Result<()> {
    let entries = list_saves(resolved)?;
    let name = saves::resolve_character(character, &entries)?;

    for entry in entries.iter().filter(|e| e.header.player_name == name) {
        let when = saves::save_timestamp(&entry.file_name).unwrap_or_else(|| "unknown time".to_string());
        println!(
            "#{:<4} level {:<3} {:<30} in-game day {:<10} {}  {}",
            entry.header.save_number,
            entry.header.player_level,
            entry.header.player_location,
            entry.header.game_date,
            when,
            entry.file_name
        );
    }
    Ok(())
}

fn cmd_save_plugins(resolved: &GamePaths, save: &str) -> Result<()> {
    let entries = list_saves(resolved)?;
    let entry = saves::resolve_save(save, &entries)?;
    let plugin_list = saves::read_plugins_from_file(&entry.path)?;

    // Same status resolution `list` uses (Data/ + Plugins.txt + Skyrim.ccc),
    // so a save's plugin shows whether it's actually active right now, not
    // just present on disk — a disabled mod is not the same as a missing one.
    let statuses = resolved.plugin_status()?;

    let width = plugin_list.iter().map(|p| p.len()).max().unwrap_or(0);
    println!("{} — {} plugin(s):", entry.file_name, plugin_list.len());
    for plugin in &plugin_list {
        let status = statuses.iter().find(|s| s.name.eq_ignore_ascii_case(plugin));
        let (tag, note) = match status {
            None => ("[!]", "missing from Data"),
            Some(s) => match state_label(s) {
                "[x]" => ("[x]", "installed and active"),
                "[ ]" => ("[ ]", "installed but disabled in Plugins.txt"),
                "[!]" => ("[!]", "listed in Plugins.txt but missing from Data"),
                "[M]" => ("[M]", "native (base game/DLC)"),
                _ => ("[CC]", "creation club"),
            },
        };
        println!("  {plugin:<width$}  {tag} {note}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let resolved = resolve(&cli)?;

    match &cli.command {
        Command::List => cmd_list(&resolved),
        Command::Enable { plugin } => cmd_enable(&resolved, plugin),
        Command::Disable { plugin } => cmd_disable(&resolved, plugin),
        Command::Edit => cmd_edit(&resolved),
        Command::Paths => cmd_paths(&cli, &resolved),
        Command::Characters => cmd_characters(&resolved),
        Command::Saves { character } => cmd_saves(&resolved, character),
        Command::SavePlugins { save } => cmd_save_plugins(&resolved, save),
    }
}
