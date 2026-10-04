use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const PLUGIN_EXTENSIONS: [&str; 3] = ["esp", "esm", "esl"];

// TES4 header record flags (see the Creation Kit wiki's "Data File Format" page).
const RECORD_FLAG_MASTER: u32 = 0x0000_0001;

/// The base game and its DLC: the only plugins outside Skyrim.ccc that the
/// engine loads regardless of Plugins.txt, always in this order.
const NATIVE_MASTERS: [&str; 5] = ["Skyrim.esm", "Update.esm", "Dawnguard.esm", "HearthFires.esm", "Dragonborn.esm"];

fn native_master_pos(name: &str) -> Option<usize> {
    NATIVE_MASTERS.iter().position(|n| eq_ci(n, name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Enabled,
    Disabled,
    /// Listed in Plugins.txt (or Skyrim.ccc) but the file is missing from Data.
    Missing,
}

#[derive(Debug, Clone)]
pub struct PluginStatus {
    pub name: String,
    pub state: State,
    /// Creation Club content: auto-loaded via Skyrim.ccc regardless of Plugins.txt.
    pub is_cc: bool,
    /// Loaded by the engine regardless of Plugins.txt: the base game/DLC
    /// masters (`NATIVE_MASTERS`) and Creation Club content in Skyrim.ccc.
    /// Nothing else is — a master-flagged mod (.esm, or an .esp with the
    /// master bit) still needs `*` in Plugins.txt like any other plugin.
    pub is_forced: bool,
    /// The TES4 header's master bit is set. Only affects load order (masters
    /// load before regular plugins), not whether the plugin loads.
    pub is_master: bool,
    /// 1-based position in the approximate engine load order, or `None` if
    /// the plugin doesn't actually load (disabled or missing). See
    /// `build_status` for how this is derived.
    pub load_order: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct DataPlugin {
    pub name: String,
    pub is_master: bool,
}

/// Read the TES4 header's record flags and report whether the master bit
/// is set. Any read/parse failure is treated as "not a master" rather than
/// an error, since a handful of unreadable/odd files shouldn't stop the
/// whole scan.
fn read_master_flag(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 12];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    if &header[0..4] != b"TES4" {
        return false;
    }
    let flags = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    flags & RECORD_FLAG_MASTER != 0
}

/// List every `.esp`/`.esm`/`.esl` file directly inside `Data`, exact case
/// as it exists on disk, along with whether its header marks it as a
/// master.
pub fn scan_data_plugins(data_dir: &Path) -> Result<Vec<DataPlugin>> {
    let mut plugins = Vec::new();
    let entries = std::fs::read_dir(data_dir)
        .with_context(|| format!("reading Data directory at {}", data_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().to_string();
        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
        if PLUGIN_EXTENSIONS.contains(&ext.as_str()) {
            let path: PathBuf = data_dir.join(&name);
            plugins.push(DataPlugin { name, is_master: read_master_flag(&path) });
        }
    }

    plugins.sort_by_key(|p| p.name.to_lowercase());
    Ok(plugins)
}

/// One parsed line of Plugins.txt: `active` reflects a leading `*`.
#[derive(Debug, Clone)]
pub struct PluginsTxtEntry {
    pub name: String,
    pub active: bool,
}

pub fn parse_plugins_txt(path: &Path) -> Result<Vec<PluginsTxtEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut entries = Vec::new();
    for line in contents.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let active = line.starts_with('*');
        let name = line.trim_start_matches('*').to_string();
        entries.push(PluginsTxtEntry { name, active });
    }
    Ok(entries)
}

/// List every plugin named in Skyrim.ccc (Creation Club content, auto-loaded
/// independent of Plugins.txt). Returns an empty list if the file is absent.
pub fn parse_ccc(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(contents
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Build the merged view: every plugin found in Data or referenced by
/// Plugins.txt/Skyrim.ccc, with its resolved state.
pub fn build_status(
    data_plugins: &[DataPlugin],
    plugins_txt: &[PluginsTxtEntry],
    ccc: &[String],
) -> Vec<PluginStatus> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut statuses = Vec::new();

    for plugin in data_plugins {
        let key = plugin.name.to_lowercase();
        if !seen.insert(key.clone()) {
            continue;
        }
        let is_cc = ccc.iter().any(|c| eq_ci(c, &plugin.name));
        let is_forced = is_cc || native_master_pos(&plugin.name).is_some();
        let txt_entry = plugins_txt.iter().find(|e| eq_ci(&e.name, &plugin.name));
        let state = if is_forced {
            State::Enabled
        } else if let Some(entry) = txt_entry {
            if entry.active { State::Enabled } else { State::Disabled }
        } else {
            State::Disabled
        };
        statuses.push(PluginStatus {
            name: plugin.name.clone(),
            state,
            is_cc,
            is_forced,
            is_master: plugin.is_master,
            load_order: None,
        });
    }

    // Plugins.txt entries whose file no longer exists in Data.
    for entry in plugins_txt {
        let key = entry.name.to_lowercase();
        if seen.insert(key) {
            statuses.push(PluginStatus {
                name: entry.name.clone(),
                state: State::Missing,
                is_cc: false,
                is_forced: false,
                is_master: false,
                load_order: None,
            });
        }
    }

    assign_load_order(&mut statuses, plugins_txt, ccc);

    statuses.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    statuses
}

/// Approximate the engine's load order and stamp each loadable plugin's
/// `load_order` with its 1-based position in it.
///
/// The engine actually loads: the base game/DLC masters (fixed order), then
/// Creation Club content (Skyrim.ccc order), then active master-flagged
/// plugins, then regular plugins — with masters additionally reordered
/// among themselves by their master-file dependencies. We don't parse each plugin's master list, so within a
/// group this just uses the order it's listed in Plugins.txt/Skyrim.ccc,
/// which is right often enough to be useful but isn't a guarantee.
/// Disabled and missing plugins don't load at all, so they're left `None`.
fn assign_load_order(statuses: &mut [PluginStatus], plugins_txt: &[PluginsTxtEntry], ccc: &[String]) {
    let txt_pos = |name: &str| plugins_txt.iter().position(|e| eq_ci(&e.name, name));
    let ccc_pos = |name: &str| ccc.iter().position(|c| eq_ci(c, name));

    let mut loadable: Vec<usize> = (0..statuses.len())
        .filter(|&i| statuses[i].is_forced || statuses[i].state == State::Enabled)
        .collect();

    loadable.sort_by_key(|&i| {
        let s = &statuses[i];
        if let Some(pos) = native_master_pos(&s.name) {
            (0u8, pos)
        } else if s.is_cc {
            (1u8, ccc_pos(&s.name).unwrap_or(usize::MAX))
        } else if s.is_master {
            (2u8, txt_pos(&s.name).unwrap_or(usize::MAX))
        } else {
            (3u8, txt_pos(&s.name).unwrap_or(usize::MAX))
        }
    });

    for (order, &i) in loadable.iter().enumerate() {
        statuses[i].load_order = Some(order + 1);
    }
}

/// Resolve a user-supplied plugin argument (exact name, wrong case, or bare
/// stem without extension) to the exact on-disk filename.
pub fn resolve_plugin_name<'a>(query: &str, data_plugins: &'a [DataPlugin]) -> Result<&'a str> {
    if let Some(exact) = data_plugins.iter().find(|p| eq_ci(&p.name, query)) {
        return Ok(&exact.name);
    }

    let stem_matches: Vec<&DataPlugin> = data_plugins
        .iter()
        .filter(|p| {
            let stem = p.name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&p.name);
            eq_ci(stem, query)
        })
        .collect();

    match stem_matches.as_slice() {
        [only] => Ok(&only.name),
        [] => anyhow::bail!("no plugin matching \"{query}\" found in Data"),
        many => {
            let list = many.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
            anyhow::bail!("\"{query}\" is ambiguous, matches: {list}")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeResult {
    AlreadyInState,
    Toggled,
    AddedNew,
    NoOpDisableUntracked,
}

fn read_lines_or_default_header(path: &Path) -> Result<(String, Vec<String>)> {
    let original = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::from(
            "# This file is used by Skyrim to keep track of your downloaded content.\n# Please do not modify this file.\n",
        )
    };
    let lines = original.lines().map(|l| l.to_string()).collect();
    Ok((original, lines))
}

fn find_plugin_line(lines: &[String], plugin_name: &str) -> Option<usize> {
    lines.iter().position(|line| {
        let trimmed = line.trim_start_matches('*');
        !trimmed.trim().is_empty() && !line.trim_start().starts_with('#') && eq_ci(trimmed, plugin_name)
    })
}

fn write_with_backup(path: &Path, original: &str, lines: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if path.exists() {
        std::fs::write(path.with_extension("txt.bak"), original)
            .with_context(|| format!("writing backup for {}", path.display()))?;
    }

    let mut new_contents = lines.join("\n");
    new_contents.push('\n');
    std::fs::write(path, new_contents).with_context(|| format!("writing {}", path.display()))
}

/// Enable or disable `plugin_name` (exact on-disk case) in Plugins.txt,
/// rewriting the file in place. A `.bak` backup of the previous contents is
/// written alongside it first.
pub fn set_plugin_enabled(path: &Path, plugin_name: &str, enable: bool) -> Result<ChangeResult> {
    let (original, mut lines) = read_lines_or_default_header(path)?;
    let match_idx = find_plugin_line(&lines, plugin_name);

    let result = match (match_idx, enable) {
        (Some(idx), true) => {
            if lines[idx].starts_with('*') {
                ChangeResult::AlreadyInState
            } else {
                lines[idx] = format!("*{plugin_name}");
                ChangeResult::Toggled
            }
        }
        (Some(idx), false) => {
            if lines[idx].starts_with('*') {
                lines[idx] = plugin_name.to_string();
                ChangeResult::Toggled
            } else {
                ChangeResult::AlreadyInState
            }
        }
        (None, true) => {
            lines.push(format!("*{plugin_name}"));
            ChangeResult::AddedNew
        }
        (None, false) => return Ok(ChangeResult::NoOpDisableUntracked),
    };

    write_with_backup(path, &original, &lines)?;
    Ok(result)
}

/// Apply a batch of (exact plugin name, desired active state) changes to
/// Plugins.txt in a single read/write, with a single `.bak` backup. Returns
/// the number of lines actually changed (entries already in the desired
/// state, or disables of untracked plugins, don't count).
pub fn apply_changes(path: &Path, changes: &[(String, bool)]) -> Result<usize> {
    let (original, mut lines) = read_lines_or_default_header(path)?;
    let mut applied = 0usize;

    for (name, want_active) in changes {
        match find_plugin_line(&lines, name) {
            Some(idx) => {
                let currently_active = lines[idx].starts_with('*');
                if currently_active != *want_active {
                    lines[idx] = if *want_active { format!("*{name}") } else { name.clone() };
                    applied += 1;
                }
            }
            None if *want_active => {
                lines.push(format!("*{name}"));
                applied += 1;
            }
            None => {}
        }
    }

    if applied > 0 {
        write_with_backup(path, &original, &lines)?;
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(name: &str, is_master: bool) -> DataPlugin {
        DataPlugin { name: name.to_string(), is_master }
    }

    fn txt(name: &str, active: bool) -> PluginsTxtEntry {
        PluginsTxtEntry { name: name.to_string(), active }
    }

    #[test]
    fn merges_data_plugins_txt_and_ccc() {
        let data_plugins = [data("Skyrim.esm", true), data("ccFish.esm", true), data("A.esp", false), data("B.esp", false)];
        let plugins_txt = [txt("b.esp", true), txt("a.esp", false), txt("Gone.esp", true)];
        let ccc = ["ccFish.esm".to_string()];

        let statuses = build_status(&data_plugins, &plugins_txt, &ccc);
        let get = |n: &str| statuses.iter().find(|s| s.name == n).unwrap();

        assert_eq!(get("A.esp").state, State::Disabled);
        assert_eq!(get("B.esp").state, State::Enabled);
        assert_eq!(get("Gone.esp").state, State::Missing);
        assert!(get("ccFish.esm").is_cc);
        // Base game first, then CC, then regular plugins in Plugins.txt order.
        assert_eq!(get("Skyrim.esm").load_order, Some(1));
        assert_eq!(get("ccFish.esm").load_order, Some(2));
        assert_eq!(get("B.esp").load_order, Some(3));
        assert_eq!(get("A.esp").load_order, None);
    }

    #[test]
    fn mod_masters_need_plugins_txt() {
        let data_plugins = [
            data("Dragonborn.esm", true),
            data("Skyrim.esm", true),
            data("ApachiiHair.esm", true),
            data("Unofficial Patch.esp", true),
            data("Mod.esp", false),
        ];
        let plugins_txt = [txt("Mod.esp", true), txt("Unofficial Patch.esp", true)];

        let statuses = build_status(&data_plugins, &plugins_txt, &[]);
        let get = |n: &str| statuses.iter().find(|s| s.name == n).unwrap();

        assert!(get("Skyrim.esm").is_forced);
        assert!(!get("ApachiiHair.esm").is_forced);
        assert_eq!(get("ApachiiHair.esm").state, State::Disabled);
        assert_eq!(get("ApachiiHair.esm").load_order, None);
        assert_eq!(get("Unofficial Patch.esp").state, State::Enabled);
        // Native masters in their fixed order, then active masters ahead of
        // regular plugins even when Plugins.txt lists them later.
        assert_eq!(get("Skyrim.esm").load_order, Some(1));
        assert_eq!(get("Dragonborn.esm").load_order, Some(2));
        assert_eq!(get("Unofficial Patch.esp").load_order, Some(3));
        assert_eq!(get("Mod.esp").load_order, Some(4));
    }

    #[test]
    fn resolves_plugin_by_stem_and_case() {
        let data_plugins = [data("Paarthurnax Dilemma.esp", false), data("Foo.esp", false), data("Foo.esm", true)];
        assert_eq!(resolve_plugin_name("paarthurnax dilemma", &data_plugins).unwrap(), "Paarthurnax Dilemma.esp");
        assert_eq!(resolve_plugin_name("FOO.ESM", &data_plugins).unwrap(), "Foo.esm");
        assert!(resolve_plugin_name("foo", &data_plugins).is_err());
    }
}
