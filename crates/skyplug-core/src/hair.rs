//! Hair: the head parts a character can pick, and a patch plugin that hides the ones you don't want.
//!
//! The character creation screen lists every `HDPT` record that is a hair, flagged playable, and
//! valid for the player's race. Hair packs add hundreds of them, so the list is too long to use.
//! [`HairCatalog`] reads them from a load order; [`HairCatalog::write_patch`] writes a small light
//! plugin that overrides the ones to hide with the playable flag cleared. Nothing else about a
//! hair changes, so its files can stay where they are, and the patch can be removed to bring
//! every hair back. A hair's extra parts (hairlines and the like) are separate records flagged as
//! extras; they're never listed on their own, so they aren't touched.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result, ensure};

use crate::armour::{FormKey, game_path};
use crate::records::{FLAG_LIGHT, PluginFile, Record};

/// The patch's file name; [`HairCatalog::open`] leaves it out so the hairs read as the mods made
/// them, and [`hidden_by_patch`] reads what it hides.
pub const PATCH_NAME: &str = "Arcanaeum Hair Picks.esp";

const PLAYABLE: u8 = 1;
const MALE: u8 = 2;
const FEMALE: u8 = 4;
const EXTRA_PART: u8 = 8;
/// A record's body is zlib-compressed, which the patch never is.
pub(crate) const COMPRESSED: u32 = 0x0004_0000;
/// The form version of Skyrim Special Edition's records.
pub(crate) const FORM_VERSION: u16 = 44;

/// What kind of head part a record is (`PNAM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartKind {
    Misc,
    Face,
    Eyes,
    Hair,
    FacialHair,
    Scar,
    Eyebrows,
    Other(u32),
}

impl From<u32> for PartKind {
    fn from(n: u32) -> Self {
        match n {
            0 => PartKind::Misc,
            1 => PartKind::Face,
            2 => PartKind::Eyes,
            3 => PartKind::Hair,
            4 => PartKind::FacialHair,
            5 => PartKind::Scar,
            6 => PartKind::Eyebrows,
            n => PartKind::Other(n),
        }
    }
}

/// A head part as the load order has it: the last plugin to change the record wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hair {
    pub key: FormKey,
    /// The plugin whose version of the record this is.
    pub plugin: String,
    /// The plugin that defines the record, with its name as on disk.
    pub defined_in: String,
    pub editor_id: String,
    /// The in-game name (`FULL`), when the plugin stores it inline.
    pub name: Option<String>,
    pub kind: PartKind,
    /// Shown on the character creation screen (before any patch of ours).
    pub playable: bool,
    pub male: bool,
    pub female: bool,
    /// An attachment of another head part, never listed itself.
    pub extra_part: bool,
    /// The model as a game path, like `meshes/ks hairdo's/zod.nif`; empty if it has none.
    pub model: String,
    /// The texture set (`TNAM`) that overrides the model's own textures, if it has one.
    pub texture_set: Option<FormKey>,
}

impl Hair {
    /// The name, or the editor ID if the name couldn't be read.
    pub fn label(&self) -> &str {
        self.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.editor_id)
    }

    /// Something a player picks from a list: a hair or facial hair, not an extra part.
    pub fn is_pickable(&self) -> bool {
        matches!(self.kind, PartKind::Hair | PartKind::FacialHair) && !self.extra_part
    }
}

/// A record kept to copy into the patch, with what's needed to read its FormIDs.
#[derive(Debug, Clone)]
struct Source {
    record: Record,
    /// The masters of the plugin it came from, and that plugin's own name.
    masters: Vec<String>,
    plugin: String,
}

#[derive(Debug)]
pub struct HairCatalog {
    /// Winning entries, in the order records were first seen (load order).
    hairs: Vec<Hair>,
    sources: HashMap<FormKey, Source>,
    /// Plugins in load order, by lower-case name, for ordering the patch's masters.
    order: Vec<String>,
    problems: Vec<String>,
}

impl HairCatalog {
    /// Read the head parts of `plugins`, given in load order. A plugin named [`PATCH_NAME`] is skipped.
    pub fn new(plugins: Vec<PluginFile>) -> Self {
        let mut index: HashMap<FormKey, usize> = HashMap::new();
        let mut hairs: Vec<Hair> = Vec::new();
        let mut sources = HashMap::new();
        let mut order = Vec::new();
        let mut problems = Vec::new();
        for plugin in plugins.iter().filter(|p| !p.name.eq_ignore_ascii_case(PATCH_NAME)) {
            order.push(plugin.name.to_ascii_lowercase());
            if !plugin.has_group(b"HDPT") {
                continue;
            }
            let list = match plugin.records(b"HDPT") {
                Ok(l) => l,
                Err(e) => {
                    problems.push(format!("{}: {e:#}", plugin.name));
                    continue;
                }
            };
            problems.extend(list.problems.iter().map(|p| format!("{}: {p}", plugin.name)));
            for r in list.records {
                let (owner, id) = plugin.resolve(r.form_id);
                let key = FormKey { plugin: owner, id };
                let hair = read_hair(plugin, &r, key.clone());
                match index.get(&key) {
                    Some(&i) => hairs[i] = hair,
                    None => {
                        index.insert(key.clone(), hairs.len());
                        hairs.push(hair);
                    }
                }
                sources.insert(key, Source { record: r, masters: plugin.masters.clone(), plugin: plugin.name.clone() });
            }
        }
        HairCatalog { hairs, sources, order, problems }
    }

    /// Open the plugins named in `load_order` (lowest first) from `data_dir`; names with no file,
    /// and files that can't be read, are skipped and reported in [`Self::problems`].
    pub fn open(data_dir: &Path, load_order: &[String]) -> Self {
        let on_disk: HashMap<String, String> = std::fs::read_dir(data_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .map(|n| (n.to_ascii_lowercase(), n))
            .collect();
        let (mut plugins, mut problems) = (Vec::new(), Vec::new());
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

    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// Every head part the load order has, as it ends up.
    pub fn all(&self) -> &[Hair] {
        &self.hairs
    }

    /// The hairs and facial hairs a player could pick, in load order of their first plugin.
    pub fn pickable(&self) -> impl Iterator<Item = &Hair> {
        self.hairs.iter().filter(|h| h.is_pickable())
    }

    /// The plugins that have pickable hairs, with how many, in load order.
    pub fn plugins_with_hair(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = Vec::new();
        for h in self.pickable() {
            match out.iter_mut().find(|(p, _)| p.eq_ignore_ascii_case(&h.defined_in)) {
                Some((_, n)) => *n += 1,
                None => out.push((h.defined_in.clone(), 1)),
            }
        }
        out.sort_by_key(|(p, _)| self.position(p));
        out
    }

    /// The patch's bytes: a light plugin overriding each of `hidden` with the playable flag
    /// cleared. Keys no plugin defines, and records already not playable, are left out.
    /// Its masters are the plugins the records come from, in load order, so it has to load after them.
    pub fn patch_bytes(&self, hidden: &[FormKey]) -> Result<Vec<u8>> {
        let mut picked: Vec<(&FormKey, &Source)> = Vec::new();
        for key in hidden {
            if let Some(src) = self.sources.get(key)
                && self.hairs.iter().any(|h| h.key == *key && h.playable)
            {
                picked.push((key, src));
            }
        }
        picked.sort_by_key(|(k, _)| (self.position(&k.plugin), k.id));
        picked.dedup_by_key(|(k, _)| (*k).clone());
        // Masters: everything a record's FormIDs can point to, in load order.
        let mut needed: BTreeSet<(usize, String)> = BTreeSet::new();
        for (_, src) in &picked {
            for m in src.masters.iter().chain(std::iter::once(&src.plugin)) {
                needed.insert((self.position(m), m.clone()));
            }
        }
        let masters: Vec<String> = needed.into_iter().map(|(_, m)| m).collect();
        ensure!(masters.len() < 0xFE, "the patch would need {} masters", masters.len());
        let index_of = |name: &str| masters.iter().position(|m| m.eq_ignore_ascii_case(name));

        let mut records = Vec::new();
        for (_, src) in &picked {
            records.push(patched_record(src, &index_of)?);
        }
        Ok(build_plugin(&masters, b"HDPT", "Hides the hairs left unticked in Arcanaeum's hair picker.", &records))
    }

    /// Write the patch to `path` (see [`Self::patch_bytes`]); with nothing to hide, any patch there is removed.
    pub fn write_patch(&self, hidden: &[FormKey], path: &Path) -> Result<()> {
        let bytes = self.patch_bytes(hidden)?;
        if bytes.is_empty() {
            return match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("removing {}", path.display())),
                _ => Ok(()),
            };
        }
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    fn position(&self, plugin: &str) -> usize {
        self.order.iter().position(|o| o.eq_ignore_ascii_case(plugin)).unwrap_or(usize::MAX)
    }
}

/// What the patch at `path` hides: the head parts it overrides with the playable flag cleared.
/// A missing patch hides nothing.
pub fn hidden_by_patch(path: &Path) -> Result<Vec<FormKey>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let plugin = PluginFile::open(path)?;
    if !plugin.has_group(b"HDPT") {
        return Ok(Vec::new());
    }
    let list = plugin.records(b"HDPT")?;
    let mut out = Vec::new();
    for r in &list.records {
        let flags = r.subrecords().find(|s| s.is(b"DATA")).and_then(|s| s.data.first().copied()).unwrap_or(0);
        if flags & PLAYABLE == 0 {
            let (owner, id) = plugin.resolve(r.form_id);
            out.push(FormKey { plugin: owner, id });
        }
    }
    Ok(out)
}

fn read_hair(plugin: &PluginFile, r: &Record, key: FormKey) -> Hair {
    let mut h = Hair {
        key,
        plugin: plugin.name.clone(),
        defined_in: plugin.masters.get((r.form_id >> 24) as usize).unwrap_or(&plugin.name).clone(),
        editor_id: r.editor_id().unwrap_or_default(),
        name: None,
        kind: PartKind::Misc,
        playable: false,
        male: false,
        female: false,
        extra_part: false,
        model: String::new(),
        texture_set: None,
    };
    for s in r.subrecords() {
        match &s.kind {
            b"FULL" if !plugin.is_localized() => h.name = Some(s.string()),
            b"PNAM" => h.kind = s.u32().unwrap_or(0).into(),
            b"MODL" => h.model = game_path(&s.string()),
            b"TNAM" => {
                h.texture_set = s.u32().map(|f| {
                    let (plugin, id) = plugin.resolve(f);
                    FormKey { plugin, id }
                })
            }
            b"DATA" => {
                let f = s.data.first().copied().unwrap_or(0);
                (h.playable, h.male, h.female, h.extra_part) = (f & PLAYABLE != 0, f & MALE != 0, f & FEMALE != 0, f & EXTRA_PART != 0);
            }
            _ => {}
        }
    }
    h
}

/// The subrecords of a head part that hold a FormID.
const FORM_SUBS: [&[u8; 4]; 4] = [b"HNAM", b"TNAM", b"CNAM", b"RNAM"];

/// The record with the playable flag cleared, uncompressed, and its FormIDs numbered by the
/// patch's masters.
fn patched_record(src: &Source, index_of: &dyn Fn(&str) -> Option<usize>) -> Result<Vec<u8>> {
    // A FormID as written in the source plugin, as the patch writes it.
    let remap = |form: u32| -> Result<u32> {
        let i = (form >> 24) as usize;
        let owner = src.masters.get(i).unwrap_or(&src.plugin);
        let n = index_of(owner).with_context(|| format!("{owner} isn't among the patch's masters"))?;
        Ok(((n as u32) << 24) | (form & 0x00FF_FFFF))
    };
    let mut body = Vec::with_capacity(src.record.data().len());
    for s in src.record.subrecords() {
        let mut data = s.data.to_vec();
        if s.is(b"DATA") {
            if let Some(f) = data.first_mut() {
                *f &= !PLAYABLE;
            }
        } else if FORM_SUBS.iter().any(|k| s.is(k)) {
            // HNAM and RNAM are one FormID each; TNAM and CNAM too.
            if let Some(chunk) = data.get_mut(..4) {
                let form = u32::from_le_bytes((&*chunk).try_into().unwrap());
                chunk.copy_from_slice(&remap(form)?.to_le_bytes());
            }
        }
        ensure!(data.len() <= u16::MAX as usize, "a subrecord is too large to copy");
        body.extend_from_slice(&s.kind);
        body.extend_from_slice(&(data.len() as u16).to_le_bytes());
        body.extend_from_slice(&data);
    }
    let mut out = b"HDPT".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&(src.record.flags & !COMPRESSED).to_le_bytes());
    out.extend_from_slice(&remap(src.record.form_id)?.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // version control
    out.extend_from_slice(&FORM_VERSION.to_le_bytes());
    out.extend_from_slice(&[0u8; 2]);
    out.extend_from_slice(&body);
    Ok(out)
}

/// A light plugin with these masters and records of the group `label`; empty if there are no records.
pub(crate) fn build_plugin(masters: &[String], label: &[u8; 4], description: &str, records: &[Vec<u8>]) -> Vec<u8> {
    if records.is_empty() {
        return Vec::new();
    }
    let sub = |kind: &[u8; 4], data: &[u8]| {
        let mut o = kind.to_vec();
        o.extend_from_slice(&(data.len() as u16).to_le_bytes());
        o.extend_from_slice(data);
        o
    };
    let mut hedr = 1.71f32.to_le_bytes().to_vec();
    hedr.extend_from_slice(&(records.len() as u32 + 1).to_le_bytes()); // the records and their group
    hedr.extend_from_slice(&0x800u32.to_le_bytes()); // next object ID: nothing new is defined
    let mut body = sub(b"HEDR", &hedr);
    body.extend(sub(b"CNAM", b"Arcanaeum\0"));
    let mut text = description.as_bytes().to_vec();
    text.push(0);
    body.extend(sub(b"SNAM", &text));
    for m in masters {
        let mut name = m.as_bytes().to_vec();
        name.push(0);
        body.extend(sub(b"MAST", &name));
        body.extend(sub(b"DATA", &[0u8; 8]));
    }
    let mut out = b"TES4".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&FLAG_LIGHT.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // form ID
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&FORM_VERSION.to_le_bytes());
    out.extend_from_slice(&[0u8; 2]);
    out.extend(body);
    let group: Vec<u8> = records.concat();
    out.extend_from_slice(b"GRUP");
    out.extend_from_slice(&((group.len() + 24) as u32).to_le_bytes());
    out.extend_from_slice(label);
    out.extend_from_slice(&[0u8; 12]);
    out.extend(group);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bsa::tests::temp_dir;
    use crate::records::tests::{group, plugin, record, sub, write, zstr};

    /// A head part: type, `DATA` flags, model, texture set and extra part FormIDs as the plugin writes them.
    #[allow(clippy::too_many_arguments)]
    fn hdpt(id: u32, edid: &str, name: &str, kind: u32, flags: u8, model: &str, txst: u32, extra: u32, compress: bool) -> Vec<u8> {
        let subs = vec![
            sub(b"EDID", &zstr(edid)),
            sub(b"FULL", &zstr(name)),
            sub(b"MODL", &zstr(model)),
            sub(b"DATA", &[flags]),
            sub(b"PNAM", &kind.to_le_bytes()),
            sub(b"HNAM", &extra.to_le_bytes()),
            sub(b"TNAM", &txst.to_le_bytes()),
            sub(b"RNAM", &0x0000_0019u32.to_le_bytes()),
        ];
        record(b"HDPT", id, &subs, compress)
    }

    /// Skyrim.esm's hair 0x800 and Hairs.esp (masters Skyrim.esm, Update.esm) with hair 0x800 (female, playable,
    /// compressed, pointing at its own extra part 0x801 and texture set 0x802), the extra part, and a male one.
    fn load_order(tag: &str) -> (std::path::PathBuf, Vec<String>) {
        let dir = temp_dir(tag);
        let sky = plugin(0x1, &[], &[group(b"HDPT", &[hdpt(0x0000_0800, "SkyHair", "Sky Hair", 3, 5, "Actors\\Character\\Hair\\a.nif", 0, 0, false)])]);
        write(&dir, "Skyrim.esm", &sky);
        write(&dir, "Update.esm", &plugin(0x1, &["Skyrim.esm"], &[]));
        let hairs = plugin(
            0,
            &["Skyrim.esm", "Update.esm"],
            &[group(
                b"HDPT",
                &[
                    hdpt(0x0200_0800, "KsZod", "Zod", 3, 5, "KS Hairdo's\\Zod.nif", 0x0200_0802, 0x0200_0801, true),
                    hdpt(0x0200_0801, "KsZodLine", "", 0, 13, "KS Hairdo's\\ZodLine.nif", 0, 0, false),
                    hdpt(0x0200_0803, "KsMan", "Man", 3, 3, "KS Hairdo's\\Man.nif", 0, 0, false),
                    // An override of Skyrim's hair: it wins, and still belongs to Skyrim.esm.
                    hdpt(0x0000_0800, "SkyHair", "Sky Hair 2", 3, 5, "Actors\\Character\\Hair\\b.nif", 0, 0, false),
                ],
            )],
        );
        write(&dir, "Hairs.esp", &hairs);
        (dir, ["Skyrim.esm", "Update.esm", "Hairs.esp"].map(String::from).to_vec())
    }

    #[test]
    fn reads_hairs_and_the_last_plugin_wins() {
        let (dir, order) = load_order("hair_read");
        let c = HairCatalog::open(&dir, &order);
        assert!(c.problems().is_empty(), "{:?}", c.problems());
        // Three pickable hairs (Skyrim's, Zod, Man); the extra part is a record but not pickable.
        assert_eq!(c.all().len(), 4);
        let pick: Vec<&str> = c.pickable().map(|h| h.label()).collect();
        assert_eq!(pick, ["Sky Hair 2", "Zod", "Man"]);
        let zod = c.pickable().find(|h| h.editor_id == "KsZod").unwrap();
        assert_eq!(zod.key, FormKey { plugin: "hairs.esp".into(), id: 0x800 });
        assert_eq!(zod.model, "meshes/ks hairdo's/zod.nif");
        assert_eq!(zod.texture_set, Some(FormKey { plugin: "hairs.esp".into(), id: 0x802 }));
        assert!(zod.playable && zod.female && !zod.male && !zod.extra_part);
        let man = c.pickable().find(|h| h.editor_id == "KsMan").unwrap();
        assert!(man.male && !man.female);
        let sky = c.pickable().find(|h| h.editor_id == "SkyHair").unwrap();
        assert_eq!(sky.key.plugin, "skyrim.esm");
        assert_eq!(sky.plugin, "Hairs.esp", "the override is the one that counts");
        assert_eq!(sky.model, "meshes/actors/character/hair/b.nif");
        assert_eq!(c.plugins_with_hair(), [("Skyrim.esm".to_string(), 1), ("Hairs.esp".to_string(), 2)]);
    }

    #[test]
    fn the_patch_clears_playable_and_keeps_every_link() {
        let (dir, order) = load_order("hair_patch");
        let c = HairCatalog::open(&dir, &order);
        let zod = FormKey { plugin: "hairs.esp".into(), id: 0x800 };
        let sky = FormKey { plugin: "skyrim.esm".into(), id: 0x800 };
        let nobody = FormKey { plugin: "hairs.esp".into(), id: 0x999 };
        let path = dir.join(PATCH_NAME);
        c.write_patch(&[zod.clone(), sky.clone(), nobody], &path).unwrap();

        let p = PluginFile::open(&path).unwrap();
        assert!(p.is_light());
        // Only the plugins the records come from, in load order: Skyrim, Update (Hairs.esp's master), Hairs.
        assert_eq!(p.masters, ["Skyrim.esm", "Update.esm", "Hairs.esp"]);
        let list = p.records(b"HDPT").unwrap();
        assert!(list.problems.is_empty());
        assert_eq!(list.records.len(), 2);
        let by = |edid: &str| list.records.iter().find(|r| r.editor_id().as_deref() == Some(edid)).unwrap();

        let r = by("KsZod");
        assert_eq!(r.flags & COMPRESSED, 0, "written uncompressed");
        // Its own FormID: Hairs.esp is the third master, so index 2.
        assert_eq!(r.form_id, 0x0200_0800);
        let get = |r: &Record, k: &[u8; 4]| r.subrecords().find(|s| s.is(k)).unwrap().data.to_vec();
        assert_eq!(get(r, b"DATA"), [4], "female only, no longer playable");
        assert_eq!(get(r, b"HNAM"), 0x0200_0801u32.to_le_bytes(), "its extra part is still Hairs.esp's");
        assert_eq!(get(r, b"TNAM"), 0x0200_0802u32.to_le_bytes());
        assert_eq!(get(r, b"RNAM"), 0x0000_0019u32.to_le_bytes(), "a Skyrim.esm record keeps index 0");
        assert_eq!(get(r, b"MODL"), zstr("KS Hairdo's\\Zod.nif"));
        assert_eq!(get(r, b"PNAM"), 3u32.to_le_bytes());

        // Skyrim's own hair is overridden under Skyrim.esm's index.
        assert_eq!(by("SkyHair").form_id, 0x0000_0800);

        let mut hidden = hidden_by_patch(&path).unwrap();
        hidden.sort_by_key(|k| k.id);
        assert_eq!(hidden, [sky, zod]);

        // A catalog of the load order with the patch in it reads the hairs as the mods made them.
        let mut with_patch = order.clone();
        with_patch.push(PATCH_NAME.into());
        let c2 = HairCatalog::open(&dir, &with_patch);
        assert_eq!(c2.pickable().count(), 3);
        assert!(c2.pickable().all(|h| h.playable));
    }

    #[test]
    fn nothing_to_hide_removes_the_patch() {
        let (dir, order) = load_order("hair_nothing");
        let c = HairCatalog::open(&dir, &order);
        let path = dir.join(PATCH_NAME);
        c.write_patch(&[FormKey { plugin: "hairs.esp".into(), id: 0x803 }], &path).unwrap();
        assert!(path.exists());
        c.write_patch(&[], &path).unwrap();
        assert!(!path.exists());
        c.write_patch(&[], &path).unwrap(); // nothing there is fine
        assert_eq!(hidden_by_patch(&path).unwrap(), []);
    }

    #[test]
    fn hiding_what_is_already_hidden_leaves_it_out() {
        let dir = temp_dir("hair_already");
        write(&dir, "Skyrim.esm", &plugin(0x1, &[], &[]));
        write(&dir, "H.esp", &plugin(0, &["Skyrim.esm"], &[group(b"HDPT", &[hdpt(0x0100_0800, "NotPlayable", "N", 3, 4, "x.nif", 0, 0, false)])]));
        let c = HairCatalog::open(&dir, &["Skyrim.esm".into(), "H.esp".into()]);
        assert!(!c.pickable().next().unwrap().playable);
        assert!(c.patch_bytes(&[FormKey { plugin: "h.esp".into(), id: 0x800 }]).unwrap().is_empty());
    }
}
