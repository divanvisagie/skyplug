//! Finding the armours in a load order and the models they are worn as.
//!
//! In Skyrim an armour (`ARMO`) doesn't name its models. It lists
//! "armatures" (`ARMA`), each holding the model for one race and sex, and
//! the armature that counts is the last one in the load order: a mod such as
//! UNP Vanilla Outfits works by overriding vanilla armatures with refitted
//! models. So [`ArmourCatalog`] reads every enabled plugin's `ARMA` records
//! once, keeps the winner for each, and then answers "which armours does this
//! plugin add or change, and what do they look like" with the models the game
//! would load.
//!
//! The models are the female ones (the male one if an armature has no female
//! model) for the human races: armatures made for Argonians and Khajiit are
//! left out, unless an armour has nothing else.
//!
//! Names of vanilla armours are in string tables (`strings\skyrim_english.strings`
//! in `Skyrim - Interface.bsa`) because the base plugins are localized; they are
//! looked up through an [`AssetResolver`] when one is given.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::assets::AssetResolver;
use crate::records::{PluginFile, Record, decode_text};

/// Which record a FormID means once the plugin it was written in is taken into account.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FormKey {
    /// The defining plugin's file name, lower case.
    pub plugin: String,
    /// The FormID without its plugin index.
    pub id: u32,
}

/// Skyrim.esm's Argonian and Khajiit races: their armatures aren't what a human wears.
const BEAST_RACES: [u32; 2] = [0x0001_3740, 0x0001_3745];
const SKYRIM_ESM: &str = "skyrim.esm";

/// A swap of textures on one shape of a model: the armature's `MO3S` / `MO2S` lists them, and
/// an armour whose armature has some looks different from the armour that shares its model
/// (the Tartan and Dark variants of a mod's outfit are the same mesh with another set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltTexture {
    /// The shape's name in the model; every shape with this name takes the swap.
    pub shape: String,
    /// The replacement texture paths by [`crate::nif::slot`], like `textures/armor/x_alt.dds`;
    /// an empty string leaves that slot as the model has it.
    pub textures: Vec<String>,
}

/// A raw alternate-texture entry before its texture-set record is looked up.
#[derive(Debug, Clone)]
struct AltRef {
    shape: String,
    set: FormKey,
}

#[derive(Debug, Clone)]
struct Arma {
    race: Option<FormKey>,
    female: Option<String>,
    male: Option<String>,
    female_alts: Vec<AltRef>,
    male_alts: Vec<AltRef>,
    /// The biped slots the armature occupies (bit 0 is slot 30); 0 if it says none.
    slots: u32,
}

/// An armour record and what it's worn as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Armour {
    pub key: FormKey,
    /// The plugin this entry was read from (it may only override the record).
    pub plugin: String,
    /// The plugin that first defines the armour.
    pub defined_in: String,
    pub editor_id: String,
    /// The in-game name, if it could be read.
    pub name: Option<String>,
    /// The biped slots it covers, as a bit set: bit 0 is slot 30.
    pub slots: u32,
    /// Game paths of the worn models, like `meshes/armor/iron/f/cuirassf_1.nif`.
    pub models: Vec<String>,
    /// For each of `models`, the slots of the armature that wears it (bit 0 is slot 30; 0 if
    /// the armature gives none).
    pub model_slots: Vec<u32>,
    /// For each of `models`, the texture swaps its armature applies (usually none).
    pub alt_textures: Vec<Vec<AltTexture>>,
    /// Armatures it lists that no loaded plugin defines.
    pub missing_armatures: usize,
}

const SLOT_NAMES: [&str; 14] =
    ["Head", "Hair", "Body", "Hands", "Forearms", "Amulet", "Ring", "Feet", "Calves", "Shield", "Tail", "Long hair", "Circlet", "Ears"];

impl Armour {
    /// The name, or the editor ID if the name couldn't be read.
    pub fn label(&self) -> &str {
        self.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.editor_id)
    }

    /// Whether this entry changes an armour another plugin added.
    pub fn is_override(&self) -> bool {
        !self.plugin.eq_ignore_ascii_case(&self.defined_in)
    }

    /// Names of the slots covered ("Body", "Hands", ...); slots with no common name show as "slot 44".
    pub fn slot_names(&self) -> Vec<String> {
        (0..32)
            .filter(|b| self.slots & (1 << b) != 0)
            .map(|b| SLOT_NAMES.get(b).map_or_else(|| format!("slot {}", 30 + b), |n| (*n).to_string()))
            .collect()
    }
}

/// The armours of a load order. Build it once per game; ask it per plugin.
pub struct ArmourCatalog {
    /// Plugins in load order.
    plugins: Vec<PluginFile>,
    armas: HashMap<FormKey, Arma>,
    /// Texture sets (`TXST`), each as the `TX00`..`TX07` paths by NIF texture slot (see [`TXST_TO_NIF_SLOT`]).
    texture_sets: HashMap<FormKey, Vec<String>>,
    /// Plugins that couldn't be read, and records skipped, with why.
    problems: Vec<String>,
    language: String,
}

impl ArmourCatalog {
    /// Read the `ARMA` records of `plugins`, given in load order.
    pub fn new(plugins: Vec<PluginFile>) -> Self {
        let mut armas = HashMap::new();
        let mut texture_sets = HashMap::new();
        let mut problems = Vec::new();
        for plugin in &plugins {
            if plugin.has_group(b"TXST") {
                match plugin.records(b"TXST") {
                    Ok(list) => {
                        for r in &list.records {
                            let (plugin_name, id) = plugin.resolve(r.form_id);
                            texture_sets.insert(FormKey { plugin: plugin_name, id }, read_texture_set(r));
                        }
                    }
                    Err(e) => problems.push(format!("{}: {e:#}", plugin.name)),
                }
            }
            if !plugin.has_group(b"ARMA") {
                continue;
            }
            match plugin.records(b"ARMA") {
                Ok(list) => {
                    problems.extend(list.problems.iter().map(|p| format!("{}: {p}", plugin.name)));
                    for r in &list.records {
                        let (plugin_name, id) = plugin.resolve(r.form_id);
                        armas.insert(FormKey { plugin: plugin_name, id }, read_arma(plugin, r));
                    }
                }
                Err(e) => problems.push(format!("{}: {e:#}", plugin.name)),
            }
        }
        ArmourCatalog { plugins, armas, texture_sets, problems, language: "english".into() }
    }

    /// Open the plugins named in `load_order` (lowest first) from `data_dir`. Names with no
    /// file, and files that can't be read, are skipped and reported in [`Self::problems`].
    pub fn open(data_dir: &Path, load_order: &[String]) -> Self {
        let on_disk: HashMap<String, String> = std::fs::read_dir(data_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .map(|n| (n.to_ascii_lowercase(), n))
            .collect();
        let mut plugins = Vec::new();
        let mut problems = Vec::new();
        for name in load_order {
            let Some(real) = on_disk.get(&name.to_ascii_lowercase()) else { continue };
            match PluginFile::open(data_dir.join(real)) {
                Ok(p) => plugins.push(p),
                Err(e) => problems.push(format!("{real}: {e:#}")),
            }
        }
        let mut catalog = Self::new(plugins);
        problems.append(&mut catalog.problems);
        catalog.problems = problems;
        catalog
    }

    /// The catalog of an install: its enabled plugins, in load order.
    pub fn for_game(paths: &crate::GamePaths) -> Result<Self> {
        let mut enabled: Vec<_> =
            paths.plugin_status()?.into_iter().filter(|p| p.state == crate::plugins::State::Enabled).collect();
        enabled.sort_by_key(|p| p.load_order);
        let names: Vec<String> = enabled.into_iter().map(|p| p.name).collect();
        Ok(Self::open(&paths.data_dir, &names))
    }

    /// The language of the string tables names are read from (`english` unless changed).
    pub fn set_language(&mut self, language: &str) {
        self.language = language.to_ascii_lowercase();
    }

    /// Plugins and records that couldn't be read.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// The plugins that have armour records (adding or changing), with how many, in load order.
    pub fn plugins_with_armours(&self) -> Vec<(String, usize)> {
        self.plugins
            .iter()
            .filter(|p| p.has_group(b"ARMO"))
            .filter_map(|p| p.count(b"ARMO").ok().filter(|&n| n > 0).map(|n| (p.name.clone(), n)))
            .collect()
    }

    /// The armours `plugin` adds or changes. `resolver` finds the string tables for names;
    /// without one only plugins that store names inline have them.
    pub fn armours_in(&self, plugin: &str, resolver: Option<&AssetResolver>) -> Result<Vec<Armour>> {
        let file = self
            .plugins
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(plugin))
            .with_context(|| format!("{plugin} isn't in the load order"))?;
        let list = file.records(b"ARMO")?;
        let strings = if file.is_localized() { resolver.and_then(|r| self.string_table(r, file)) } else { None };
        let mut out: Vec<Armour> = list.records.iter().map(|r| self.read_armour(file, r, strings.as_ref())).collect();
        out.sort_by_cached_key(|a| a.label().to_lowercase());
        Ok(out)
    }

    /// Armours of every plugin whose name or editor ID contains `text` (any case), most
    /// specific plugin last in load order: the entry that wins comes first for each armour.
    pub fn search(&self, text: &str, resolver: Option<&AssetResolver>) -> Vec<Armour> {
        let needle = text.to_lowercase();
        let mut found: Vec<Armour> = Vec::new();
        for (name, _) in self.plugins_with_armours().into_iter().rev() {
            for a in self.armours_in(&name, resolver).unwrap_or_default() {
                if (a.label().to_lowercase().contains(&needle) || a.editor_id.to_lowercase().contains(&needle))
                    && !found.iter().any(|f| f.key == a.key)
                {
                    found.push(a);
                }
            }
        }
        found
    }

    /// The armour each of `keys` names, as the load order has it (the last plugin to change a
    /// record wins), in the order of `keys`; a key no plugin has an armour record for is left out.
    pub fn armours_for(&self, keys: &[FormKey], resolver: Option<&AssetResolver>) -> Vec<Armour> {
        let mut found: Vec<Option<Armour>> = vec![None; keys.len()];
        for (name, _) in self.plugins_with_armours().into_iter().rev() {
            if found.iter().all(Option::is_some) {
                break;
            }
            for a in self.armours_in(&name, resolver).unwrap_or_default() {
                for (slot, key) in found.iter_mut().zip(keys) {
                    if slot.is_none() && a.key == *key {
                        *slot = Some(a.clone());
                    }
                }
            }
        }
        found.into_iter().flatten().collect()
    }

    fn defined_in(&self, key: &FormKey) -> String {
        self.plugins.iter().find(|p| p.name.eq_ignore_ascii_case(&key.plugin)).map_or_else(|| key.plugin.clone(), |p| p.name.clone())
    }

    fn string_table(&self, resolver: &AssetResolver, file: &PluginFile) -> Option<HashMap<u32, String>> {
        let stem = file.name.rsplit_once('.').map_or(file.name.as_str(), |(s, _)| s);
        let bytes = resolver.read_asset(&format!("strings/{stem}_{}.strings", self.language)).ok().flatten()?;
        parse_strings(&bytes)
    }

    fn read_armour(&self, file: &PluginFile, r: &Record, strings: Option<&HashMap<u32, String>>) -> Armour {
        let (plugin, id) = file.resolve(r.form_id);
        let key = FormKey { plugin, id };
        let (mut name, mut slots) = (None, 0);
        let mut addons = Vec::new();
        for s in r.subrecords() {
            match &s.kind {
                b"FULL" if file.is_localized() => name = s.u32().and_then(|i| strings?.get(&i).cloned()),
                b"FULL" => name = Some(s.string()),
                b"BOD2" | b"BODT" => slots = s.u32().unwrap_or(0),
                b"MODL" => addons.extend(s.u32().map(|f| {
                    let (plugin, id) = file.resolve(f);
                    FormKey { plugin, id }
                })),
                _ => {}
            }
        }
        // A string ID left where text should be isn't a name.
        let name = name.filter(|n| !n.chars().any(char::is_control));
        let found: Vec<&Arma> = addons.iter().filter_map(|k| self.armas.get(k)).collect();
        let missing_armatures = addons.len() - found.len();
        let human: Vec<&Arma> = found.iter().copied().filter(|a| !a.race.as_ref().is_some_and(is_beast_race)).collect();
        let chosen = if human.is_empty() { found } else { human };
        let mut models: Vec<String> = Vec::new();
        let mut model_slots: Vec<u32> = Vec::new();
        let mut alt_textures: Vec<Vec<AltTexture>> = Vec::new();
        for a in chosen {
            let (model, alts) = if a.female.is_some() { (a.female.as_ref(), &a.female_alts) } else { (a.male.as_ref(), &a.male_alts) };
            if let Some(m) = model {
                let path = game_path(m);
                if !models.contains(&path) {
                    models.push(path);
                    model_slots.push(a.slots);
                    alt_textures.push(
                        alts.iter()
                            .filter_map(|r| self.texture_sets.get(&r.set).map(|t| AltTexture { shape: r.shape.clone(), textures: t.clone() }))
                            .collect(),
                    );
                }
            }
        }
        Armour {
            defined_in: self.defined_in(&key),
            plugin: file.name.clone(),
            key,
            editor_id: r.editor_id().unwrap_or_default(),
            name,
            slots,
            models,
            model_slots,
            alt_textures,
            missing_armatures,
        }
    }
}

fn is_beast_race(k: &FormKey) -> bool {
    k.plugin == SKYRIM_ESM && BEAST_RACES.contains(&k.id)
}

fn read_arma(plugin: &PluginFile, r: &Record) -> Arma {
    let (mut race, mut female, mut male, mut slots) = (None, None, None, 0);
    let (mut female_alts, mut male_alts) = (Vec::new(), Vec::new());
    for s in r.subrecords() {
        match &s.kind {
            b"BOD2" | b"BODT" => slots = s.u32().unwrap_or(0),
            b"MO3S" => female_alts = read_alt_refs(plugin, s.data),
            b"MO2S" => male_alts = read_alt_refs(plugin, s.data),
            b"RNAM" => {
                race = s.u32().map(|f| {
                    let (plugin, id) = plugin.resolve(f);
                    FormKey { plugin, id }
                })
            }
            b"MOD3" => female = Some(s.string()),
            b"MOD2" => male = Some(s.string()),
            _ => {}
        }
    }
    Arma { race, female, male, female_alts, male_alts, slots }
}

/// An alternate-textures subrecord: a count, then for each a length-prefixed shape name, the
/// texture set's FormID and the shape's 3D index (not used: shapes are matched by name).
fn read_alt_refs(plugin: &PluginFile, data: &[u8]) -> Vec<AltRef> {
    let u32_at = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let mut out = Vec::new();
    let Some(count) = u32_at(0) else { return out };
    let mut q = 4;
    for _ in 0..count {
        let Some(len) = u32_at(q) else { break };
        let (start, end) = (q + 4, q + 4 + len as usize);
        let (Some(name), Some(form)) = (data.get(start..end), u32_at(end)) else { break };
        let (plugin_name, id) = plugin.resolve(form);
        out.push(AltRef { shape: decode_text(name), set: FormKey { plugin: plugin_name, id } });
        q = end + 8; // the form and the 3D index
    }
    out
}

/// Which NIF texture slot each of a `TXST` record's `TX00` to `TX07` fills: diffuse, normal,
/// environment mask, glow, height, environment, multilayer, and backlight/specular.
pub const TXST_TO_NIF_SLOT: [usize; 8] = [0, 1, 5, 2, 3, 4, 6, 7];

/// A texture set's paths by NIF slot, as game paths (`textures/...`); "" where it has none.
fn read_texture_set(r: &Record) -> Vec<String> {
    let mut out = vec![String::new(); 8];
    for s in r.subrecords() {
        if let [b'T', b'X', b'0', d @ b'0'..=b'7'] = s.kind {
            let path = s.string().replace('\\', "/").to_ascii_lowercase();
            if !path.is_empty() {
                out[TXST_TO_NIF_SLOT[(d - b'0') as usize]] = if path.starts_with("textures/") { path } else { format!("textures/{path}") };
            }
        }
    }
    out
}

/// Put an armour's texture swaps on a parsed model: each shape named in `alts` takes the
/// replacement paths for the slots they give.
pub fn apply_alternate_textures(nif: &mut crate::nif::Nif, alts: &[AltTexture]) {
    for alt in alts {
        for m in nif.meshes.iter_mut().filter(|m| m.name.eq_ignore_ascii_case(&alt.shape)) {
            if m.textures.len() < 8 {
                m.textures.resize(8, String::new());
            }
            for (slot, path) in alt.textures.iter().enumerate().filter(|(_, p)| !p.is_empty()) {
                m.textures[slot] = path.clone();
            }
        }
    }
}

/// `Armor\Iron\F\CuirassLight_1.nif` as stored in a record becomes `meshes/armor/iron/f/cuirasslight_1.nif`.
fn game_path(model: &str) -> String {
    let p = model.replace('\\', "/").to_ascii_lowercase();
    if p.starts_with("meshes/") { p } else { format!("meshes/{p}") }
}

/// The biped slot (30 to 61) a skin partition's body-part number means, if it is one: 30 to
/// 61 as they are, and 130 to 161 as the game numbers the head parts (131 is hair, 142 circlet).
pub fn slot_of_part(part: u16) -> Option<u32> {
    match part {
        30..=61 => Some(part as u32),
        130..=161 => Some(part as u32 - 100),
        _ => None,
    }
}

/// Reasons the game won't draw parts of `model` for an armature that occupies the slots in
/// `mask` (bit 0 is slot 30): the game only draws a skin partition whose slot is among the
/// armature's, so a model whose partitions are all on other slots is invisible. A shape counts
/// as drawn if any one of its partitions fits (boots have two), and nothing is said when the
/// armature gives no slots or the shape has no partitions to judge by.
pub fn partition_problems(model: &str, mask: u32, nif: &crate::nif::Nif) -> Vec<String> {
    if mask == 0 {
        return Vec::new();
    }
    let worn: Vec<String> = (0..32).filter(|b| mask & (1 << b) != 0).map(|b| (30 + b).to_string()).collect();
    nif.meshes
        .iter()
        .filter_map(|m| {
            let slots: Vec<u32> = m.body_parts.iter().filter_map(|&p| slot_of_part(p)).collect();
            let fits = slots.iter().any(|s| mask & (1 << (s - 30)) != 0);
            (!slots.is_empty() && !fits).then(|| {
                let on: Vec<String> = slots.iter().map(u32::to_string).collect();
                format!(
                    "{model}: '{}' is on slot {}, but the armature is worn on slot {}, so the game won't draw it",
                    m.name,
                    on.join(" and "),
                    worn.join(", ")
                )
            })
        })
        .collect()
}

impl Armour {
    /// Read each of this armour's models and say what the game won't draw (see [`partition_problems`]).
    pub fn visibility_problems(&self, resolver: &AssetResolver) -> Vec<String> {
        let mut out = Vec::new();
        for (model, &mask) in self.models.iter().zip(&self.model_slots) {
            if let Ok(Some(bytes)) = resolver.read_asset(model)
                && let Ok(nif) = crate::nif::parse(&bytes)
            {
                out.extend(partition_problems(model, mask, &nif));
            }
        }
        out
    }
}

/// A `.strings` table: a count and data size, then (id, offset) pairs, then NUL-terminated text.
fn parse_strings(b: &[u8]) -> Option<HashMap<u32, String>> {
    let u32_at = |o: usize| b.get(o..o + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()));
    let count = u32_at(0)? as usize;
    let data_start = count.checked_mul(8)?.checked_add(8)?;
    (data_start <= b.len()).then_some(())?;
    let mut out = HashMap::with_capacity(count);
    for i in 0..count {
        let id = u32_at(8 + i * 8)?;
        let offset = u32_at(12 + i * 8)? as usize;
        let text = b.get(data_start.checked_add(offset)?..)?;
        let end = text.iter().position(|&c| c == 0)?;
        out.insert(id, decode_text(&text[..end]));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bsa::tests::temp_dir;
    use crate::records::tests::{group, plugin, record, sub, write, zstr};
    use crate::records::{FLAG_LOCALIZED, FLAG_MASTER};

    fn id(n: u32) -> Vec<u8> {
        n.to_le_bytes().to_vec()
    }

    fn arma_rec(form: u32, edid: &str, race: u32, female: Option<&str>, male: Option<&str>) -> Vec<u8> {
        let mut subs = vec![sub(b"EDID", &zstr(edid)), sub(b"RNAM", &id(race))];
        if let Some(m) = male {
            subs.push(sub(b"MOD2", &zstr(m)));
        }
        if let Some(f) = female {
            subs.push(sub(b"MOD3", &zstr(f)));
        }
        record(b"ARMA", form, &subs, false)
    }

    fn armo_rec(form: u32, edid: &str, full: Option<&[u8]>, slots: u32, addons: &[u32]) -> Vec<u8> {
        let mut subs = vec![sub(b"EDID", &zstr(edid))];
        if let Some(f) = full {
            subs.push(sub(b"FULL", f));
        }
        subs.push(sub(b"BOD2", &[id(slots), id(1)].concat()));
        for a in addons {
            subs.push(sub(b"MODL", &id(*a)));
        }
        record(b"ARMO", form, &subs, false)
    }

    /// A tiny "Skyrim.esm" (localized) and a mod that overrides one armature and adds an armour.
    fn install(tag: &str) -> (std::path::PathBuf, ArmourCatalog) {
        let data = temp_dir(tag).join("Data");
        std::fs::create_dir_all(data.join("strings")).unwrap();
        let skyrim = plugin(
            FLAG_MASTER | FLAG_LOCALIZED,
            &[],
            &[
                group(
                    b"ARMA",
                    &[
                        arma_rec(0x100, "IronAA", 0x19, Some("Armor\\Iron\\F\\Cuirass_1.nif"), Some("Armor\\Iron\\M\\Cuirass_1.nif")),
                        arma_rec(0x101, "IronAA_Argonian", 0x13740, Some("Armor\\Iron\\Arg\\Cuirass_1.nif"), None),
                        arma_rec(0x102, "MaleOnlyAA", 0x19, None, Some("Armor\\Misc\\Male_1.nif")),
                    ],
                ),
                group(
                    b"ARMO",
                    &[
                        armo_rec(0x200, "ArmorIron", Some(&id(7)), 0b100, &[0x100, 0x101]),
                        armo_rec(0x201, "ArmorOdd", Some(&id(8)), 0b1000, &[0x102, 0x999]),
                        armo_rec(0x202, "ArmorBeastOnly", Some(&id(9)), 0b1, &[0x101]),
                    ],
                ),
            ],
        );
        std::fs::write(data.join("Skyrim.esm"), skyrim).unwrap();
        // Strings table: ids 7, 8, 9.
        let texts = ["Iron Armor", "Odd Armor", "Beast Armor"];
        let mut table = (texts.len() as u32).to_le_bytes().to_vec();
        let blob: Vec<u8> = texts.iter().flat_map(|t| zstr(t)).collect();
        table.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        let mut off = 0u32;
        for (i, t) in texts.iter().enumerate() {
            table.extend_from_slice(&(7 + i as u32).to_le_bytes());
            table.extend_from_slice(&off.to_le_bytes());
            off += t.len() as u32 + 1;
        }
        table.extend_from_slice(&blob);
        std::fs::write(data.join("strings/Skyrim_English.STRINGS"), table).unwrap();

        // A mod, loading after: refits the iron armature (override), and adds its own armour,
        // whose FormIDs are in plugin 1 (after the master).
        let modp = plugin(
            0,
            &["Skyrim.esm"],
            &[
                group(b"ARMA", &[arma_rec(0x0000_0100, "IronAA", 0x19, Some("Armor\\Iron\\F\\UNP_Cuirass_1.nif"), Some("Armor\\Iron\\M\\Cuirass_1.nif")), arma_rec(0x0100_0800, "NewAA", 0x0000_0019, Some("Mod\\New_1.nif"), None)]),
                group(
                    b"ARMO",
                    &[
                        armo_rec(0x0100_0801, "ModArmour", Some(&zstr("Mod Armour")), 0b100, &[0x0100_0800]),
                        // An override of the vanilla iron armour, with the same addons.
                        // (A plugin that isn't localized stores the name inline, as the editor writes it.)
                        armo_rec(0x0000_0200, "ArmorIron", Some(&zstr("Iron Armor")), 0b100, &[0x0000_0100, 0x0000_0101]),
                        // A damaged one with a raw string ID where text should be: no name, not garbage.
                        armo_rec(0x0000_0201, "ArmorOdd", Some(&id(8)), 0b1000, &[0x0000_0102]),
                    ],
                ),
            ],
        );
        write(&data, "Mod.esp", &modp);
        let order = ["Skyrim.esm", "Mod.esp"].map(String::from);
        let catalog = ArmourCatalog::open(&data, &order);
        (data, catalog)
    }

    #[test]
    fn lists_the_armours_of_a_plugin_with_names_models_and_slots() {
        let (data, catalog) = install("arm_basic");
        assert!(catalog.problems().is_empty(), "{:?}", catalog.problems());
        assert_eq!(catalog.plugins_with_armours(), [("Skyrim.esm".to_string(), 3), ("Mod.esp".to_string(), 3)]);
        let resolver = AssetResolver::new(&data, []);
        let vanilla = catalog.armours_in("skyrim.esm", Some(&resolver)).unwrap();
        let by = |n: &str| vanilla.iter().find(|a| a.editor_id == n).unwrap_or_else(|| panic!("{n}"));
        // Names come from the string table; the Argonian armature is left out; the override
        // by Mod.esp wins the armature, so the vanilla armour is now worn as the refitted model.
        let iron = by("ArmorIron");
        assert_eq!(iron.name.as_deref(), Some("Iron Armor"));
        assert_eq!(iron.models, ["meshes/armor/iron/f/unp_cuirass_1.nif"]);
        assert_eq!(iron.slot_names(), ["Body"]);
        assert!(!iron.is_override());
        assert_eq!(iron.defined_in, "Skyrim.esm");
        // No female model: the male one. A listed armature no plugin has is counted.
        let odd = by("ArmorOdd");
        assert_eq!(odd.models, ["meshes/armor/misc/male_1.nif"]);
        assert_eq!(odd.missing_armatures, 1);
        assert_eq!(odd.slot_names(), ["Hands"]);
        // Only a beast armature: used rather than leaving the armour empty.
        assert_eq!(by("ArmorBeastOnly").models, ["meshes/armor/iron/arg/cuirass_1.nif"]);
        // Sorted by name.
        let labels: Vec<_> = vanilla.iter().map(|a| a.label().to_string()).collect();
        assert_eq!(labels, ["Beast Armor", "Iron Armor", "Odd Armor"]);
    }

    #[test]
    fn a_mods_own_and_overridden_armours_are_told_apart() {
        let (data, catalog) = install("arm_mod");
        let resolver = AssetResolver::new(&data, []);
        let modp = catalog.armours_in("Mod.esp", Some(&resolver)).unwrap();
        assert_eq!(modp.len(), 3);
        let own = modp.iter().find(|a| a.editor_id == "ModArmour").unwrap();
        assert_eq!(own.name.as_deref(), Some("Mod Armour"), "an inline name in a plugin that isn't localized");
        assert_eq!(own.models, ["meshes/mod/new_1.nif"]);
        assert!(!own.is_override());
        let over = modp.iter().find(|a| a.editor_id == "ArmorIron").unwrap();
        assert!(over.is_override());
        assert_eq!(over.defined_in, "Skyrim.esm");
        assert_eq!(over.name.as_deref(), Some("Iron Armor"), "an override in a plugin that isn't localized has the name inline");
        assert_eq!(over.plugin, "Mod.esp");
        let odd = modp.iter().find(|a| a.editor_id == "ArmorOdd").unwrap();
        assert_eq!(odd.name, None, "control characters are not a name");
        assert_eq!(odd.label(), "ArmorOdd");
    }

    #[test]
    fn without_a_resolver_localized_names_fall_back_to_the_editor_id() {
        let (_, catalog) = install("arm_noname");
        let vanilla = catalog.armours_in("Skyrim.esm", None).unwrap();
        let iron = vanilla.iter().find(|a| a.editor_id == "ArmorIron").unwrap();
        assert_eq!(iron.name, None);
        assert_eq!(iron.label(), "ArmorIron");
    }

    #[test]
    fn search_finds_by_name_or_editor_id_and_reports_each_armour_once() {
        let (data, catalog) = install("arm_search");
        let resolver = AssetResolver::new(&data, []);
        let found = catalog.search("iron", Some(&resolver));
        // Mod.esp comes first (it loads last and wins), and the vanilla armour isn't listed twice.
        let iron: Vec<_> = found.iter().filter(|a| a.editor_id == "ArmorIron").collect();
        assert_eq!(iron.len(), 1);
        assert_eq!(iron[0].plugin, "Mod.esp");
        assert!(catalog.search("MOD ARMOUR", Some(&resolver)).iter().any(|a| a.editor_id == "ModArmour"));
        assert!(catalog.search("nothing like this", Some(&resolver)).is_empty());
    }

    #[test]
    fn unreadable_plugins_and_unknown_names_are_skipped_and_reported() {
        let (data, _) = install("arm_bad");
        std::fs::write(data.join("Broken.esp"), b"not a plugin at all, just text").unwrap();
        let order = ["Skyrim.esm", "Broken.esp", "Gone.esp", "Mod.esp"].map(String::from);
        let catalog = ArmourCatalog::open(&data, &order);
        assert_eq!(catalog.problems().len(), 1, "{:?}", catalog.problems());
        assert!(catalog.problems()[0].contains("Broken.esp"));
        assert!(catalog.armours_in("Gone.esp", None).is_err());
        assert_eq!(catalog.plugins_with_armours().len(), 2);
    }

    #[test]
    fn string_tables_and_paths_are_read_defensively() {
        assert!(parse_strings(b"").is_none());
        assert!(parse_strings(&[255, 255, 255, 255, 0, 0, 0, 0]).is_none());
        let mut t = 1u32.to_le_bytes().to_vec();
        t.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]); // id 1 at offset 0, then no data
        assert!(parse_strings(&t).is_none(), "an unterminated string is a bad table, not a panic");
        assert_eq!(game_path("Armor\\Iron\\F\\C_1.nif"), "meshes/armor/iron/f/c_1.nif");
        assert_eq!(game_path("meshes\\x.nif"), "meshes/x.nif");
    }

    fn nif_with(parts: &[u16]) -> crate::nif::Nif {
        crate::nif::Nif {
            meshes: vec![crate::nif::Mesh { name: "Strap".into(), skinned: true, body_parts: parts.to_vec(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn a_model_on_another_slot_than_its_armature_is_reported() {
        let slot = |n: u32| 1u32 << (n - 30);
        // The case that made a moved armour invisible: the plugin says 55, the model still says 54.
        let p = partition_problems("meshes/x/strap_1.nif", slot(55), &nif_with(&[54]));
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("'Strap' is on slot 54") && p[0].contains("worn on slot 55") && p[0].contains("won't draw"), "{p:?}");
        // Matching slots, a pair of boots with one partition that fits, head parts numbered 130+,
        // an armature that gives no slots, a shape with no partitions, an armour on several slots.
        assert!(partition_problems("m", slot(54), &nif_with(&[54])).is_empty());
        assert!(partition_problems("m", slot(37), &nif_with(&[38, 37])).is_empty());
        assert!(partition_problems("m", slot(31), &nif_with(&[131])).is_empty());
        assert!(partition_problems("m", 0, &nif_with(&[54])).is_empty());
        assert!(partition_problems("m", slot(55), &nif_with(&[])).is_empty());
        assert!(partition_problems("m", slot(32) | slot(55), &nif_with(&[55])).is_empty());
        // Numbers that aren't biped slots (dismemberment-only parts) say nothing either way.
        assert!(partition_problems("m", slot(55), &nif_with(&[7])).is_empty());
        assert_eq!(slot_of_part(54), Some(54));
        assert_eq!(slot_of_part(142), Some(42));
        assert_eq!(slot_of_part(99), None);
    }

    #[test]
    fn the_catalog_keeps_each_armatures_slots_beside_its_model() {
        let data = temp_dir("arm_model_slots").join("Data");
        std::fs::create_dir_all(&data).unwrap();
        let skirt = record(b"ARMA", 0x100, &[sub(b"EDID", &zstr("SkirtAA")), sub(b"RNAM", &id(0x19)), sub(b"MOD3", &zstr("A\\skirt_1.nif")), sub(b"BOD2", &[id(1 << 18), id(1)].concat())], false);
        let plain = record(b"ARMA", 0x101, &[sub(b"EDID", &zstr("PlainAA")), sub(b"RNAM", &id(0x19)), sub(b"MOD3", &zstr("A\\plain_1.nif"))], false);
        let armo = armo_rec(0x200, "Skirt", None, 1 << 18, &[0x100, 0x101]);
        write(&data, "T.esp", &plugin(0, &[], &[group(b"ARMA", &[skirt, plain]), group(b"ARMO", &[armo])]));
        let catalog = ArmourCatalog::open(&data, &["T.esp".to_string()]);
        let a = &catalog.armours_in("T.esp", None).unwrap()[0];
        assert_eq!(a.models, ["meshes/a/skirt_1.nif", "meshes/a/plain_1.nif"]);
        assert_eq!(a.model_slots, [1 << 18, 0]);
    }

    fn alt_subrecord(entries: &[(&str, u32, i32)]) -> Vec<u8> {
        let mut d = (entries.len() as u32).to_le_bytes().to_vec();
        for (name, set, index) in entries {
            d.extend_from_slice(&(name.len() as u32).to_le_bytes());
            d.extend_from_slice(name.as_bytes());
            d.extend_from_slice(&set.to_le_bytes());
            d.extend_from_slice(&index.to_le_bytes());
        }
        d
    }

    #[test]
    fn an_armatures_texture_swaps_come_with_its_model() {
        let data = temp_dir("arm_alt").join("Data");
        std::fs::create_dir_all(&data).unwrap();
        let set = record(
            b"TXST",
            0x300,
            &[sub(b"EDID", &zstr("TSAlt")), sub(b"TX00", &zstr("armor\\x\\alt.dds")), sub(b"TX01", &zstr("armor\\x\\alt_n.dds")), sub(b"TX05", &zstr("cubemaps\\c.dds")), sub(b"TX02", &zstr("armor\\x\\alt_m.dds"))],
            false,
        );
        let alt = record(b"ARMA", 0x100, &[sub(b"EDID", &zstr("AltAA")), sub(b"RNAM", &id(0x19)), sub(b"MOD3", &zstr("x\\outfit_1.nif")), sub(b"MO3S", &alt_subrecord(&[("Outfit", 0x300, 1)]))], false);
        let base = record(b"ARMA", 0x101, &[sub(b"EDID", &zstr("BaseAA")), sub(b"RNAM", &id(0x19)), sub(b"MOD3", &zstr("x\\outfit_1.nif"))], false);
        let armos = [armo_rec(0x200, "Alt", None, 4, &[0x100]), armo_rec(0x201, "Base", None, 4, &[0x101])];
        write(&data, "T.esp", &plugin(0, &[], &[group(b"TXST", &[set]), group(b"ARMA", &[alt, base]), group(b"ARMO", &armos)]));
        let catalog = ArmourCatalog::open(&data, &["T.esp".to_string()]);
        let list = catalog.armours_in("T.esp", None).unwrap();
        let (alt, base) = (list.iter().find(|a| a.editor_id == "Alt").unwrap(), list.iter().find(|a| a.editor_id == "Base").unwrap());
        assert_eq!(alt.models, base.models, "the same model");
        assert!(base.alt_textures[0].is_empty());
        let swap = &alt.alt_textures[0];
        assert_eq!(swap.len(), 1);
        assert_eq!(swap[0].shape, "Outfit");
        // TX00 diffuse, TX01 normal, TX02 environment mask (nif slot 5), TX05 environment (nif slot 4).
        assert_eq!(swap[0].textures[0], "textures/armor/x/alt.dds");
        assert_eq!(swap[0].textures[1], "textures/armor/x/alt_n.dds");
        assert_eq!(swap[0].textures[5], "textures/armor/x/alt_m.dds");
        assert_eq!(swap[0].textures[4], "textures/cubemaps/c.dds");
        assert_eq!(swap[0].textures[2], "");
    }

    #[test]
    fn texture_swaps_change_only_the_named_shape_and_only_the_slots_they_give() {
        let shape = |name: &str| crate::nif::Mesh { name: name.into(), textures: vec!["a.dds".into(), "a_n.dds".into()], ..Default::default() };
        let mut nif = crate::nif::Nif { meshes: vec![shape("Outfit"), shape("Boots")], ..Default::default() };
        let mut textures = vec![String::new(); 8];
        textures[0] = "textures/alt.dds".into();
        apply_alternate_textures(&mut nif, &[AltTexture { shape: "outfit".into(), textures }]);
        assert_eq!(nif.meshes[0].textures[0], "textures/alt.dds");
        assert_eq!(nif.meshes[0].textures[1], "a_n.dds", "a slot the swap leaves empty is kept");
        assert_eq!(nif.meshes[0].textures.len(), 8);
        assert_eq!(nif.meshes[1].textures, ["a.dds", "a_n.dds"], "another shape is untouched");
    }

    #[test]
    fn damaged_swap_lists_do_not_panic() {
        let (data, _) = install("arm_alt_damaged");
        let file = PluginFile::open(data.join("Skyrim.esm")).unwrap();
        let good = alt_subrecord(&[("Shape", 0x300, 0), ("Other", 0x301, 1)]);
        assert_eq!(read_alt_refs(&file, &good).len(), 2);
        for cut in 0..good.len() {
            let _ = read_alt_refs(&file, &good[..cut]);
        }
        let mut huge = good.clone();
        huge[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(read_alt_refs(&file, &huge).len(), 2, "a count bigger than the data stops at the data");
        assert!(read_alt_refs(&file, &[]).is_empty());
    }

    #[test]
    fn slot_names() {
        let a = Armour {
            key: FormKey { plugin: "a.esp".into(), id: 1 },
            plugin: "a.esp".into(),
            defined_in: "A.esp".into(),
            editor_id: "X".into(),
            name: None,
            slots: 0b100 | 0b1000 | (1 << 14),
            models: vec![],
            model_slots: vec![],
            alt_textures: vec![],
            missing_armatures: 0,
        };
        assert_eq!(a.slot_names(), ["Body", "Hands", "slot 44"]);
    }

    /// Read-only run over a real install, skipped unless `SKYPLUG_TEST_GAME_DIR` is set (see the
    /// crate README): every enabled plugin's armours, and whether the models they name exist.
    #[test]
    fn real_install_armours() {
        use crate::GamePaths;
        use crate::steam::{DEFAULT_APPID, GameInstall};
        let Some(dir) = std::env::var_os("SKYPLUG_TEST_GAME_DIR") else {
            eprintln!("SKYPLUG_TEST_GAME_DIR not set; skipping");
            return;
        };
        let prefix = std::env::var_os("SKYPLUG_TEST_PREFIX").map(std::path::PathBuf::from);
        let paths = GamePaths::new(&GameInstall::new(std::path::PathBuf::from(dir), prefix).unwrap(), DEFAULT_APPID);
        let started = std::time::Instant::now();
        let catalog = ArmourCatalog::for_game(&paths).unwrap();
        let resolver = AssetResolver::for_game(&paths).unwrap();
        eprintln!("catalog built in {:?}; problems: {:?}", started.elapsed(), catalog.problems());

        let plugins = catalog.plugins_with_armours();
        eprintln!("{} plugins with armour records, {} armatures", plugins.len(), catalog.armas.len());
        let (mut total, mut named, mut unnamed, mut no_models, mut missing_armatures) = (0, 0, 0, 0, 0);
        let (mut models, mut missing_files) = (0, Vec::new());
        let mut unnamed_by = std::collections::BTreeMap::new();
        for (name, _) in &plugins {
            for a in catalog.armours_in(name, Some(&resolver)).unwrap() {
                total += 1;
                if a.name.is_some() {
                    named += 1
                } else {
                    unnamed += 1;
                    *unnamed_by.entry(a.plugin.clone()).or_insert(0usize) += 1;
                }
                if a.models.is_empty() { no_models += 1 }
                missing_armatures += a.missing_armatures;
                for m in &a.models {
                    models += 1;
                    if resolver.resolve(m).is_none() {
                        missing_files.push(format!("{} / {} -> {m}", a.plugin, a.label()));
                    }
                }
            }
        }
        eprintln!("{total} armour records: {named} named, {unnamed} unnamed, {no_models} with no models, {missing_armatures} armatures not defined anywhere");
        eprintln!("unnamed, by plugin: {unnamed_by:?}");
        eprintln!("{models} model paths, {} not found as files:", missing_files.len());
        for m in missing_files.iter().take(12) {
            eprintln!("   {m}");
        }

        // The visibility rule against every armour's models: the game draws all of these, so
        // anything reported is either a real mistake in a mod or a flaw in the rule.
        let mut flagged: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        let mut checked = 0;
        for (name, _) in &plugins {
            for a in catalog.armours_in(name, Some(&resolver)).unwrap() {
                checked += a.models.len();
                for p in a.visibility_problems(&resolver) {
                    flagged.entry(a.plugin.clone()).or_default().push(format!("{}: {p}", a.label()));
                }
            }
        }
        // Texture swaps: how many armours have them, and do the textures they name exist?
        let (mut swapped, mut swap_paths, mut swap_missing) = (0, 0, Vec::new());
        for (name, _) in &plugins {
            for a in catalog.armours_in(name, Some(&resolver)).unwrap() {
                let swaps: Vec<&AltTexture> = a.alt_textures.iter().flatten().collect();
                swapped += usize::from(!swaps.is_empty());
                for alt in swaps {
                    for t in alt.textures.iter().filter(|t| !t.is_empty()) {
                        swap_paths += 1;
                        if resolver.resolve(t).is_none() {
                            swap_missing.push(format!("{} / {}: {t}", a.plugin, a.label()));
                        }
                    }
                }
            }
        }
        eprintln!("{swapped} armours swap textures; {swap_paths} swapped-in textures, {} not found:", swap_missing.len());
        for m in swap_missing.iter().take(6) {
            eprintln!("   {m}");
        }
        eprintln!("{checked} models checked against their armatures' slots; {} plugins have a mismatch:", flagged.len());
        for (plugin, v) in &flagged {
            eprintln!("  {plugin}: {}", v.len());
            for line in v.iter().take(3) {
                eprintln!("      {line}");
            }
        }

        let vanilla = catalog.armours_in("Skyrim.esm", Some(&resolver)).unwrap();
        let iron = vanilla.iter().find(|a| a.editor_id == "ArmorIronCuirass").expect("the iron cuirass");
        eprintln!("iron cuirass: {:?} slots {:?} models {:?}", iron.name, iron.slot_names(), iron.models);
        assert_eq!(iron.name.as_deref(), Some("Iron Armor"));
        assert!(!iron.models.is_empty());
        assert!(vanilla.len() > 2000);
        assert!(named * 10 > total * 9, "most armours should have names");
    }
}
