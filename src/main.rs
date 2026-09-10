mod plugins;
mod steam;
mod tui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use plugins::{ChangeResult, State};

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
    List,
    /// Enable a plugin (adds/sets the `*` prefix in Plugins.txt).
    Enable { plugin: String },
    /// Disable a plugin (removes the `*` prefix in Plugins.txt).
    Disable { plugin: String },
    /// Interactive TUI to enable/disable plugins.
    Edit,
    /// Show the resolved game/Plugins.txt paths and exit.
    Paths,
}

struct Resolved {
    data_dir: PathBuf,
    plugins_txt: PathBuf,
    ccc_path: PathBuf,
}

fn resolve(cli: &Cli) -> Result<Resolved> {
    let (data_dir, plugins_txt, ccc_path) = if let Some(game_dir) = &cli.game_dir {
        // Manual override: derive compatdata from the library two levels up
        // (<library>/steamapps/common/<game>), same layout Steam uses.
        let library_root = game_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .context("--game-dir doesn't look like a Steam steamapps/common install")?
            .to_path_buf();
        let install = steam::GameInstall {
            game_dir: game_dir.clone(),
            library_root,
        };
        (
            install.data_dir(),
            install.plugins_txt_path(&cli.appid),
            install.ccc_path(),
        )
    } else {
        let install = steam::find_game_install()?;
        (
            install.data_dir(),
            install.plugins_txt_path(&cli.appid),
            install.ccc_path(),
        )
    };

    Ok(Resolved { data_dir, plugins_txt, ccc_path })
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

fn cmd_list(resolved: &Resolved) -> Result<()> {
    let data_plugins = plugins::scan_data_plugins(&resolved.data_dir)?;
    let plugins_txt = plugins::parse_plugins_txt(&resolved.plugins_txt)?;
    let ccc = plugins::parse_ccc(&resolved.ccc_path)?;
    let statuses = plugins::build_status(&data_plugins, &plugins_txt, &ccc);

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

fn cmd_enable(resolved: &Resolved, plugin: &str) -> Result<()> {
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

fn cmd_disable(resolved: &Resolved, plugin: &str) -> Result<()> {
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

fn cmd_edit(resolved: &Resolved) -> Result<()> {
    let data_plugins = plugins::scan_data_plugins(&resolved.data_dir)?;
    let plugins_txt = plugins::parse_plugins_txt(&resolved.plugins_txt)?;
    let ccc = plugins::parse_ccc(&resolved.ccc_path)?;
    let statuses = plugins::build_status(&data_plugins, &plugins_txt, &ccc);

    let saved = tui::run(&resolved.plugins_txt, statuses)?;
    if saved > 0 {
        println!("saved {saved} change(s) to {}", resolved.plugins_txt.display());
    } else {
        println!("no changes saved");
    }
    Ok(())
}

fn cmd_paths(cli: &Cli, resolved: &Resolved) -> Result<()> {
    println!("appid:       {}", cli.appid);
    println!("data dir:    {}", resolved.data_dir.display());
    println!("plugins.txt: {}", resolved.plugins_txt.display());
    println!("skyrim.ccc:  {}", resolved.ccc_path.display());
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
    }
}
