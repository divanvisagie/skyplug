mod tui;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use skyplug_core::armour::{self, ArmourCatalog, BodySex};
use skyplug_core::assets::AssetResolver;
use skyplug_core::plugins::{self, ChangeResult, State};
use skyplug_core::{nif, render};
use skyplug_core::{GameInstall, GamePaths, saves, steam};

/// Scan and toggle Skyrim Special Edition plugins on Linux, Steam or GOG.
#[derive(Parser)]
#[command(name = "skyplug", version, about)]
struct Cli {
    /// Skyrim Special Edition game directory (contains SkyrimSE.exe).
    /// Auto-detected from Steam libraries when omitted.
    #[arg(long, global = true, env = "SKYPLUG_GAME_DIR")]
    game_dir: Option<PathBuf>,

    /// Wine prefix the game runs in (the folder holding drive_c), for a
    /// copy run outside Steam, like GOG's under umu or Wine. Steam's
    /// Proton prefix when omitted.
    #[arg(long, global = true, env = "SKYPLUG_PREFIX")]
    prefix: Option<PathBuf>,

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
    ///   [M]  base game/DLC master — the engine always loads it regardless of Plugins.txt
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
    /// List what a character wears in a save: the armours the load order has for the worn items
    /// (exact filename, filename without `.ess`, or a substring, as for `save-plugins`).
    Outfit { save: String },
    /// List the armours in the load order: the plugins that have some, or one plugin's armours
    /// with the models they are worn as.
    Armours {
        /// A plugin file name. Omit to list the plugins that have armour records.
        plugin: Option<String>,
        /// Only armours whose name or editor ID contains this (any case).
        #[arg(long)]
        filter: Option<String>,
        /// Also read each model and say which the game won't draw (a skin partition on a slot the armour isn't worn on).
        #[arg(long)]
        check: bool,
        /// List the models worn on this body: female (the default) or male.
        #[arg(long, value_enum)]
        sex: Option<SexArg>,
    },
    /// Render a model with its textures to a PNG, and say where each texture comes from.
    Preview(PreviewArgs),
    /// Print the skyplug(1) manual page.
    Man {
        /// Write it under ../share/man/man1 next to the skyplug binary
        /// (e.g. ~/.cargo/share/man/man1 after `cargo install`) instead.
        #[arg(long)]
        install: bool,
    },
}

/// Which body to draw armours on.
#[derive(Clone, Copy, ValueEnum)]
enum SexArg {
    Female,
    Male,
}

impl From<SexArg> for BodySex {
    fn from(s: SexArg) -> Self {
        match s {
            SexArg::Female => BodySex::Female,
            SexArg::Male => BodySex::Male,
        }
    }
}

#[derive(Args)]
struct PreviewArgs {
    /// The model: a game path such as `meshes/armor/iron/f/cuirass_1.nif`, or a `.nif` file on disk.
    model: String,
    /// Where to write the PNG (all views side by side).
    #[arg(short, long, default_value = "preview.png")]
    out: PathBuf,
    /// Views to draw, comma separated: front, three-quarter, right, back, left, or an angle in degrees
    /// (0 is the front, 90 the character's right).
    #[arg(long, value_delimiter = ',', default_value = "front,three-quarter,right,back")]
    views: Vec<String>,
    /// Width and height of each view, in pixels.
    #[arg(long, default_value_t = 480)]
    size: usize,
    /// Also draw the installed female body, for scale.
    #[arg(long)]
    body: bool,
    /// Draw another model too (a game path or a file); can be repeated.
    #[arg(long = "with")]
    with: Vec<String>,
    /// Draw an armour by name or editor ID, as the load order wears it; can be repeated. The model
    /// argument may then be left as `-`.
    #[arg(long)]
    armour: Vec<String>,
    /// Draw what the character wears in a save (as for `skyplug outfit`). The model argument may
    /// then be left as `-`.
    #[arg(long)]
    outfit: Option<String>,
    /// The body to draw armours on, and its skeleton: female or male. Female unless `--outfit` names
    /// a save of a man.
    #[arg(long, value_enum)]
    sex: Option<SexArg>,
    /// Degrees above the horizon.
    #[arg(long, default_value_t = 6.0)]
    elevation: f32,
    /// 1 fits the whole model; larger zooms in.
    #[arg(long, default_value_t = 1.0)]
    zoom: f32,
    /// A transparent background instead of dark grey.
    #[arg(long)]
    transparent: bool,
    /// Draw skinned models as stored, instead of posed on the game's skeleton.
    #[arg(long)]
    no_pose: bool,
}

const MAN_PAGE: &str = include_str!("../man/skyplug.1");

fn resolve(cli: &Cli) -> Result<GamePaths> {
    let install = match &cli.game_dir {
        Some(game_dir) => GameInstall::new(game_dir, cli.prefix.clone()).context("--game-dir (a game outside Steam needs --prefix too)")?,
        None => GameInstall { prefix: cli.prefix.clone(), ..steam::find_game_install(&cli.appid)? },
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
        "\n{} enabled, {} disabled, {} always-loaded (base game/CC), {} missing ({} total)",
        enabled,
        disabled,
        forced,
        missing,
        statuses.len()
    );

    Ok(())
}

fn warn_if_forced(resolved: &GamePaths, exact: &str) -> Result<()> {
    let statuses = resolved.plugin_status()?;
    if let Some(s) = statuses.iter().find(|s| s.name == exact && s.is_forced) {
        let what = if s.is_cc { "Creation Club content (Skyrim.ccc)" } else { "a base game/DLC master" };
        eprintln!("note: {exact} is {what} — the engine always loads it regardless of Plugins.txt");
    }
    Ok(())
}

fn cmd_enable(resolved: &GamePaths, plugin: &str) -> Result<()> {
    let data_plugins = plugins::scan_data_plugins(&resolved.data_dir)?;
    let exact = plugins::resolve_plugin_name(plugin, &data_plugins)?.to_string();
    warn_if_forced(resolved, &exact)?;

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
    warn_if_forced(resolved, &exact)?;

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
    println!("edition:     {}", resolved.edition.name());
    if let Some(prefix) = &resolved.prefix {
        println!("prefix:      {}", prefix.display());
    }
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

fn cmd_outfit(resolved: &GamePaths, save: &str) -> Result<()> {
    let entries = list_saves(resolved)?;
    let entry = saves::resolve_save(save, &entries)?;
    let outfit = skyplug_core::outfit::read_outfit_from_file(&entry.path)?;
    let catalog = ArmourCatalog::for_game(resolved)?;
    let resolver = resolved.asset_resolver()?;
    let keys: Vec<_> = outfit.worn.iter().map(|w| w.key.clone()).collect();
    let armours = catalog.armours_for(&keys, Some(&resolver));
    println!("{} wears {} items in {}", entry.header.player_name, keys.len(), entry.file_name);
    for key in &keys {
        match armours.iter().find(|a| a.key == *key) {
            Some(a) => println!("{}  [{}]  {}  {}", a.label(), a.editor_id, a.slot_names().join(", "), a.plugin),
            None => println!("(not armour: a weapon, torch or the like)  {}:{:06X}", key.plugin, key.id),
        }
    }
    if outfit.skipped > 0 {
        println!("{} worn items were made in game (enchanted or renamed) and can't be shown", outfit.skipped);
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

fn view_angle(name: &str) -> Result<f32> {
    Ok(match name {
        "front" => 0.0,
        "three-quarter" => 35.0,
        "right" => 90.0,
        "back" => 180.0,
        "left" => 270.0,
        other => other.parse().with_context(|| format!("unknown view {other:?} (use front, three-quarter, right, back, left or an angle)"))?,
    })
}

/// A model from the game's assets with an armour's texture swaps applied.
fn load_with_swaps(resolver: &AssetResolver, name: &str, alts: &[armour::AltTexture]) -> Result<Option<render::Model>> {
    let Some(bytes) = resolver.read_asset(name).with_context(|| format!("reading {name}"))? else { return Ok(None) };
    let mut parsed = nif::parse(&bytes).with_context(|| format!("reading {name}"))?;
    armour::apply_alternate_textures(&mut parsed, alts);
    Ok(Some(render::Model::load(resolver, &parsed, 1024)))
}

/// Read a model from a file on disk if there is one, else from the game's assets.
fn load_model(resolver: &AssetResolver, name: &str) -> Result<Option<render::Model>> {
    let max_texture = 1024;
    let path = std::path::Path::new(name);
    if path.is_file() {
        let nif = nif::parse(&std::fs::read(path)?).with_context(|| format!("reading {name}"))?;
        return Ok(Some(render::Model::load(resolver, &nif, max_texture)));
    }
    render::Model::from_asset(resolver, name, max_texture).with_context(|| format!("reading {name}"))
}

/// The armour `text` names: an exact name or editor ID if there is one, else the first
/// match; an error listing candidates if nothing matches.
fn find_armour(catalog: &ArmourCatalog, resolver: &AssetResolver, text: &str) -> Result<armour::Armour> {
    let found = catalog.search(text, Some(resolver));
    let exact = found.iter().position(|a| a.editor_id.eq_ignore_ascii_case(text) || a.label().eq_ignore_ascii_case(text));
    match exact.or(if found.is_empty() { None } else { Some(0) }) {
        Some(i) => {
            if exact.is_none() && found.len() > 1 {
                let others: Vec<_> = found.iter().skip(1).take(4).map(|a| a.label()).collect();
                eprintln!("note: {} armours match {text:?}; using {:?} (others: {})", found.len(), found[0].label(), others.join(", "));
            }
            Ok(found[i].clone())
        }
        None => anyhow::bail!("no armour matches {text:?} (see `skyplug armours`)"),
    }
}

fn cmd_armours(resolved: &GamePaths, plugin: Option<&str>, filter: Option<&str>, check: bool, sex: BodySex) -> Result<()> {
    let catalog = ArmourCatalog::for_game(resolved)?;
    for p in catalog.problems() {
        eprintln!("warning: {p}");
    }
    let Some(plugin) = plugin else {
        for (name, n) in catalog.plugins_with_armours() {
            println!("{n:5}  {name}");
        }
        return Ok(());
    };
    let resolver = resolved.asset_resolver()?;
    let needle = filter.map(str::to_lowercase);
    let armours = catalog.armours_in(plugin, Some(&resolver))?;
    for a in armours.iter().filter(|a| needle.as_ref().is_none_or(|n| a.label().to_lowercase().contains(n) || a.editor_id.to_lowercase().contains(n))) {
        let change = if a.is_override() { format!("  (changes {})", a.defined_in) } else { String::new() };
        println!("{}  [{}]  {}{change}", a.label(), a.editor_id, a.slot_names().join(", "));
        let set = a.models_for(sex);
        for m in &set.models {
            println!("    {m}");
        }
        if set.models.is_empty() {
            println!("    (no models)");
        }
        if check {
            for problem in a.visibility_problems(sex, &resolver) {
                println!("    ! {problem}");
            }
        }
    }
    Ok(())
}

fn cmd_preview(resolved: &GamePaths, a: &PreviewArgs) -> Result<()> {
    let resolver = resolved.asset_resolver()?;
    for w in resolver.warnings() {
        eprintln!("warning: {w}");
    }
    let mut model = if a.model == "-" {
        render::Model::default()
    } else {
        load_model(&resolver, &a.model)?.with_context(|| format!("{}: the game has no such file", a.model))?
    };
    let outfit_save = match &a.outfit {
        Some(save) => {
            let entries = list_saves(resolved)?;
            Some(saves::resolve_save(save, &entries)?.clone())
        }
        None => None,
    };
    // Asked for, else the character's, else female.
    let sex = a.sex.map(BodySex::from).or_else(|| outfit_save.as_ref().map(|e| e.header.player_sex.into())).unwrap_or_default();
    if !a.armour.is_empty() || outfit_save.is_some() {
        let catalog = ArmourCatalog::for_game(resolved)?;
        let mut chosen = Vec::new();
        for text in &a.armour {
            chosen.push((find_armour(&catalog, &resolver, text)?, true));
        }
        if let Some(entry) = &outfit_save {
            let worn = skyplug_core::outfit::read_outfit_from_file(&entry.path)?;
            let keys: Vec<_> = worn.worn.iter().map(|w| w.key.clone()).collect();
            // A save's outfit is what it is: a model that can't be read is skipped, not fatal.
            chosen.extend(catalog.armours_for(&keys, Some(&resolver)).into_iter().map(|armour| (armour, false)));
        }
        for (armour, asked) in chosen {
            let set = armour.models_for(sex);
            println!("armour {} ({}, from {}): {}", armour.label(), armour.editor_id, armour.plugin, set.models.join(", "));
            for problem in armour.visibility_problems(sex, &resolver) {
                println!("  WARNING: {problem}");
            }
            for (m, alts) in set.models.iter().zip(&set.alt_textures) {
                let loaded = load_with_swaps(&resolver, m, alts).and_then(|l| l.with_context(|| format!("{m}: the game has no such file")));
                match loaded {
                    Ok(part) => model.merge(part),
                    Err(e) if asked => return Err(e),
                    Err(e) => println!("  WARNING: skipped {m}: {e:#}"),
                }
            }
        }
    }
    if a.body {
        for asset in sex.body() {
            match load_model(&resolver, asset)? {
                Some(body) => model.merge_underlay(body),
                None => eprintln!("note: no {asset} in this install"),
            }
        }
    }
    for extra in &a.with {
        model.merge(load_model(&resolver, extra)?.with_context(|| format!("{extra}: the game has no such file"))?);
    }
    anyhow::ensure!(model.mesh_count() > 0, "{}: nothing to draw (no meshes it can read)", a.model);
    if !a.no_pose {
        match render::load_skeleton_for(&resolver, sex) {
            Some(skeleton) => {
                let r = model.pose(&skeleton);
                println!("posed {} skinned meshes on the game's skeleton: {} move, by up to {:.1} units", r.posed, r.moved, r.largest);
            }
            None => eprintln!("note: this install has no {} skeleton; drawing models as stored", if sex == BodySex::Male { "male" } else { "female" }),
        }
    }

    println!("{} meshes", model.mesh_count());
    for t in &model.textures {
        match &t.result {
            render::TextureResult::Found { source, hides } if hides.is_empty() => println!("  {} slot {}: {} <- {source}", t.mesh, t.slot, t.path),
            render::TextureResult::Found { source, hides } => println!("  {} slot {}: {} <- {source} (hides {})", t.mesh, t.slot, t.path, hides.join(", ")),
            render::TextureResult::Missing => println!("  {} slot {}: {} MISSING", t.mesh, t.slot, t.path),
            render::TextureResult::Unreadable(why) => println!("  {} slot {}: {} UNREADABLE ({why})", t.mesh, t.slot, t.path),
        }
    }
    for w in &model.warnings {
        println!("warning: {w}");
    }

    let size = a.size.max(16);
    let opts = render::Options { width: size, height: size, background: if a.transparent { [0, 0, 0, 0] } else { [38, 41, 50, 255] }, ..Default::default() };
    let angles: Vec<f32> = a.views.iter().map(|v| view_angle(v)).collect::<Result<_>>()?;
    anyhow::ensure!(!angles.is_empty(), "no views asked for");
    let mut strip = vec![0u8; size * angles.len() * size * 4];
    for (i, azimuth) in angles.iter().enumerate() {
        let camera = render::Camera { azimuth: *azimuth, elevation: a.elevation, zoom: a.zoom };
        let img = render::render(&model, &camera, &opts);
        for y in 0..size {
            let from = y * size * 4;
            let to = (y * angles.len() * size + i * size) * 4;
            strip[to..to + size * 4].copy_from_slice(&img.pixels[from..from + size * 4]);
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(&a.out).with_context(|| format!("creating {}", a.out.display()))?);
    let mut enc = png::Encoder::new(file, (size * angles.len()) as u32, size as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&strip)?;
    println!("wrote {}", a.out.display());
    Ok(())
}

fn cmd_man(install: bool) -> Result<()> {
    if !install {
        std::io::stdout().write_all(MAN_PAGE.as_bytes())?;
        return Ok(());
    }

    let exe = std::env::current_exe().context("locating the skyplug binary")?;
    let prefix = exe
        .parent()
        .and_then(|bin| bin.parent())
        .context("the skyplug binary has no parent directory to install under")?;
    let dir = prefix.join("share/man/man1");
    let path = dir.join("skyplug.1");
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&path, MAN_PAGE))
        .with_context(|| format!("writing {} (run `skyplug man > <path>` to put it somewhere else)", path.display()))?;
    println!("installed {}", path.display());
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Man { install } = cli.command {
        return cmd_man(install);
    }
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
        Command::Outfit { save } => cmd_outfit(&resolved, save),
        Command::Armours { plugin, filter, check, sex } => cmd_armours(&resolved, plugin.as_deref(), filter.as_deref(), *check, sex.map(BodySex::from).unwrap_or_default()),
        Command::Preview(args) => cmd_preview(&resolved, args),
        Command::Man { .. } => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The man page is written by hand, so make sure it keeps up with the
    /// CLI: every subcommand and long flag needs at least a mention.
    #[test]
    fn man_page_covers_cli() {
        fn check(cmd: &clap::Command, missing: &mut Vec<String>) {
            for arg in cmd.get_arguments() {
                // mdoc writes `--flag` as `Fl -flag`.
                if let Some(long) = arg.get_long().filter(|long| !MAN_PAGE.contains(&format!("Fl -{long}"))) {
                    missing.push(format!("--{long}"));
                }
            }
            for sub in cmd.get_subcommands() {
                if sub.get_name() != "help" && !MAN_PAGE.contains(&format!("Cm {}", sub.get_name())) {
                    missing.push(sub.get_name().to_string());
                }
                check(sub, missing);
            }
        }

        let mut missing = Vec::new();
        check(&Cli::command(), &mut missing);
        assert!(missing.is_empty(), "not documented in man/skyplug.1: {}", missing.join(", "));
    }
}
