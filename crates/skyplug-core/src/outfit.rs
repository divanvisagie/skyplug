//! What the player is wearing, read from a save.
//!
//! The player's inventory is in the body of the save, in the change form of the player
//! reference (`0x14`). Each item carries a list of extra data, and a worn item has `Worn` or
//! `WornLeft` in it. The layout follows UESP's "Skyrim Mod:Save File Format" (REFR/ACHR change
//! forms, extra data); the leading fields of the player's form aren't decoded, so the inventory
//! is found by looking for the block that parses cleanly from its first byte to its last.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::armour::FormKey;
use crate::saves::{open, read_bytes, read_u8, read_u16, read_u32, read_wstring};

/// The player's own reference.
const PLAYER: u32 = 0x14;
/// Change form type 1 is an `ACHR`.
const ACHR: u8 = 1;
/// The lowest-numbered light plugin FormID is `0xFE000800`; a light plugin's index is 12 bits.
const LIGHT: u32 = 0xFE;
const EXTRA_WORN: u8 = 22;
const EXTRA_WORN_LEFT: u8 = 23;
/// Most extra data lists an item can plausibly have; more means the bytes aren't an inventory.
const MAX_LISTS: usize = 20;
const MAX_ITEMS: usize = 5000;

/// One worn item: the record the save says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WornItem {
    pub key: FormKey,
}

/// The items the player wears in a save's bytes, in inventory order. Items made in game
/// (enchanted or renamed copies, FormIDs starting `FF`) have no record in a plugin and are left out;
/// `skipped` counts them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outfit {
    pub worn: Vec<WornItem>,
    pub skipped: usize,
}

/// Read a save file and say what the player wears.
pub fn read_outfit_from_file(path: &Path) -> Result<Outfit> {
    let buf = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    outfit(&buf).with_context(|| path.display().to_string())
}

/// What the player wears in a save held in memory.
pub fn outfit(buf: &[u8]) -> Result<Outfit> {
    let opened = open(buf)?;
    ensure!(opened.header.is_se, "only Special Edition saves can be read for an outfit");
    let body: &[u8] = &opened.body;

    // Plugins, then light plugins, then the file location table.
    let (form_version, pos) = read_u8(body, 0)?;
    let (_size, mut pos) = read_u32(body, pos)?;
    let (count, next) = read_u8(body, pos)?;
    pos = next;
    let plugins = names(body, &mut pos, count as usize)?;
    let lights = if form_version >= 78 {
        let (count, next) = read_u16(body, pos)?;
        pos = next;
        names(body, &mut pos, count as usize)?
    } else {
        Vec::new()
    };
    // formIDArrayCountOffset, unknownTable3Offset, table 1, table 2, change forms, table 3,
    // three table counts, change form count.
    let mut table = [0u32; 10];
    for slot in &mut table {
        (*slot, pos) = read_u32(body, pos)?;
    }
    let (form_id_offset, change_forms_offset, change_form_count) = (table[0] as usize, table[4] as usize, table[9] as usize);
    // The offsets count from the start of the file.
    let at = |offset: usize| offset.checked_sub(opened.body_start).context("an offset in the save points before its body");

    let mut p = at(form_id_offset)?;
    let (n, next) = read_u32(body, p)?;
    p = next;
    let mut ids = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let (id, next) = read_u32(body, p)?;
        ids.push(id);
        p = next;
    }
    let names = Names { plugins: &plugins, lights: &lights, ids: &ids };

    let mut p = at(change_forms_offset)?;
    for _ in 0..change_form_count {
        let (r, next) = read_bytes(body, p, 3)?;
        let (_flags, next) = read_u32(body, next)?;
        let (kind, next) = read_u8(body, next)?;
        let (_version, next) = read_u8(body, next)?;
        let (len1, len2, next) = match kind >> 6 {
            0 => {
                let (a, n) = read_u8(body, next)?;
                let (b, n) = read_u8(body, n)?;
                (a as usize, b as usize, n)
            }
            1 => {
                let (a, n) = read_u16(body, next)?;
                let (b, n) = read_u16(body, n)?;
                (a as usize, b as usize, n)
            }
            _ => {
                let (a, n) = read_u32(body, next)?;
                let (b, n) = read_u32(body, n)?;
                (a as usize, b as usize, n)
            }
        };
        let (data, next) = read_bytes(body, next, len1)?;
        p = next;
        if kind & 0x3F != ACHR || names.form_id(ref_id(r)) != Some(PLAYER) {
            continue;
        }
        let data = if len2 != 0 {
            std::borrow::Cow::Owned(lz4_flex::block::decompress(data, len2).context("decompressing the player's change form")?)
        } else {
            std::borrow::Cow::Borrowed(data)
        };
        return worn(&data, &names);
    }
    bail!("the save has no change form for the player")
}

fn names(body: &[u8], pos: &mut usize, count: usize) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let (name, next) = read_wstring(body, *pos)?;
        out.push(name);
        *pos = next;
    }
    Ok(out)
}

/// A 3-byte `RefID`: two bits of kind, then 22 bits.
fn ref_id(b: &[u8]) -> u32 {
    (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])
}

/// Turns the save's own numbering into plugins and records.
struct Names<'a> {
    plugins: &'a [String],
    lights: &'a [String],
    /// The save's form ID array, which a `RefID` of kind 0 indexes (from 1).
    ids: &'a [u32],
}

impl Names<'_> {
    /// The full FormID a `RefID` stands for; `None` for a kind that isn't known.
    fn form_id(&self, r: u32) -> Option<u32> {
        let value = r & 0x003F_FFFF;
        match r >> 22 {
            0 => (value as usize).checked_sub(1).and_then(|i| self.ids.get(i)).copied().or(Some(0)),
            1 => Some(value),
            2 => Some(0xFF00_0000 | value),
            _ => None,
        }
    }

    /// The plugin and record a FormID in the save's load order names; `None` for one made in game.
    fn key(&self, form_id: u32) -> Option<FormKey> {
        let index = form_id >> 24;
        if index == LIGHT {
            let plugin = self.lights.get(((form_id >> 12) & 0xFFF) as usize)?;
            return Some(FormKey { plugin: plugin.to_ascii_lowercase(), id: form_id & 0xFFF });
        }
        let plugin = self.plugins.get(index as usize)?;
        Some(FormKey { plugin: plugin.to_ascii_lowercase(), id: form_id & 0x00FF_FFFF })
    }
}

/// Find the inventory in the player's change form and list what is worn.
fn worn(data: &[u8], names: &Names) -> Result<Outfit> {
    // The biggest block that parses end to end is the inventory; a stray match is a handful of items.
    let mut best: Option<Vec<Item>> = None;
    for start in 0..data.len() {
        if let Some(items) = inventory(data, start).filter(|i| best.as_ref().is_none_or(|b| i.len() > b.len())) {
            best = Some(items);
        }
    }
    let items = best.context("couldn't find the player's inventory in the save")?;
    let mut out = Outfit::default();
    for item in items.iter().filter(|i| i.worn) {
        match names.form_id(item.reference).and_then(|f| names.key(f)) {
            Some(key) if !out.worn.iter().any(|w| w.key == key) => out.worn.push(WornItem { key }),
            Some(_) => {}
            None => out.skipped += 1,
        }
    }
    Ok(out)
}

struct Item {
    reference: u32,
    worn: bool,
}

/// A variable-size value: the low two bits of the first byte say how many bytes it takes.
fn vsval(b: &[u8], p: usize) -> Option<(usize, usize)> {
    let first = *b.get(p)?;
    match first & 3 {
        0 => Some((usize::from(first >> 2), p + 1)),
        1 => Some((usize::from(u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) >> 2), p + 2)),
        2 => {
            let raw = u32::from_le_bytes(b.get(p..p + 4)?.try_into().ok()?);
            Some(((raw >> 2) as usize, p + 4))
        }
        _ => None,
    }
}

fn skip(b: &[u8], p: usize, n: usize) -> Option<usize> {
    let end = p.checked_add(n)?;
    (end <= b.len()).then_some(end)
}

/// One extra data entry; its type and where it ends. `None` for a type that isn't known, so a
/// block with one is not taken for an inventory rather than guessed at.
fn extra(b: &[u8], p: usize) -> Option<(u8, usize)> {
    let kind = *b.get(p)?;
    let p = p + 1;
    let size = match kind {
        EXTRA_WORN | EXTRA_WORN_LEFT => 0,
        31 | 73 | 150 | 156 => 1,
        36 => 2,
        28 | 33 | 34 | 35 | 69 | 142 => 3,
        30 | 37 | 39 | 40 | 47 | 160 => 4,
        155 => 5,
        159 => 6,
        149 => 7,
        176 => 8,
        136 => {
            let (n, p) = vsval(b, p)?;
            return Some((kind, skip(b, p, n.checked_mul(7)?)?));
        }
        140 => {
            let (n, p) = vsval(b, p)?;
            return Some((kind, skip(b, p, n.checked_mul(3)?)?));
        }
        153 => {
            let p = skip(b, p, 6)?;
            let marker = i32::from_le_bytes(b.get(p..p + 4)?.try_into().ok()?);
            let p = p + 4;
            if marker == -2 {
                let len = usize::from(u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?));
                return Some((kind, skip(b, p + 2, len)?));
            }
            return Some((kind, p));
        }
        _ => return None,
    };
    Some((kind, skip(b, p, size)?))
}

/// Read an inventory from `start`: a count, then each item's `RefID`, count and extra data.
fn inventory(b: &[u8], start: usize) -> Option<Vec<Item>> {
    let (n, mut p) = vsval(b, start)?;
    if n == 0 || n >= MAX_ITEMS {
        return None;
    }
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let r = ref_id(b.get(p..p + 3)?);
        if r >> 22 == 3 {
            return None;
        }
        p = skip(b, p, 3 + 4)?; // the RefID, then a signed count
        let (lists, next) = vsval(b, p)?;
        if lists > MAX_LISTS {
            return None;
        }
        p = next;
        let mut worn = false;
        for _ in 0..lists {
            let (entries, next) = vsval(b, p)?;
            if entries > MAX_LISTS {
                return None;
            }
            p = next;
            for _ in 0..entries {
                let (kind, next) = extra(b, p)?;
                worn |= kind == EXTRA_WORN || kind == EXTRA_WORN_LEFT;
                p = next;
            }
        }
        items.push(Item { reference: r, worn });
    }
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names<'a>(plugins: &'a [String], lights: &'a [String], ids: &'a [u32]) -> Names<'a> {
        Names { plugins, lights, ids }
    }

    #[test]
    fn a_vsval_takes_one_two_or_four_bytes() {
        assert_eq!(vsval(&[0x04], 0), Some((1, 1)));
        assert_eq!(vsval(&[0x0C], 0), Some((3, 1)));
        assert_eq!(vsval(&[0x01, 0x01], 0), Some((64, 2)));
        assert_eq!(vsval(&[0x02, 0, 1, 0], 0), Some((0x4000, 4)));
        assert_eq!(vsval(&[0x03], 0), None);
    }

    #[test]
    fn ref_ids_come_from_the_array_the_default_range_or_are_made_in_game() {
        let (plugins, lights, ids) = (vec![], vec![], vec![0x0100_0ABC]);
        let n = names(&plugins, &lights, &ids);
        assert_eq!(n.form_id(1), Some(0x0100_0ABC)); // kind 0: the first array entry
        assert_eq!(n.form_id(0x40_000F), Some(0x0F)); // kind 1: Skyrim.esm's gold
        assert_eq!(n.form_id(0x80_0001), Some(0xFF00_0001)); // kind 2: made in game
        assert_eq!(n.form_id(0xC0_0000), None);
    }

    #[test]
    fn form_ids_name_a_plugin_and_light_plugins_a_light_one() {
        let (plugins, lights, ids) = (vec!["Skyrim.esm".into(), "Mod.esp".into()], vec!["Tiny.esp".into()], vec![]);
        let n = names(&plugins, &lights, &ids);
        assert_eq!(n.key(0x0100_0ABC), Some(FormKey { plugin: "mod.esp".into(), id: 0xABC }));
        assert_eq!(n.key(0xFE00_0808), Some(FormKey { plugin: "tiny.esp".into(), id: 0x808 }));
        assert_eq!(n.key(0xFF00_0001), None);
    }

    /// An inventory of gold, a worn iron cuirass-like item and a worn-left ring.
    fn inventory_bytes() -> Vec<u8> {
        let mut b = vec![0x0C]; // three items
        b.extend([0x40, 0x00, 0x0F, 0x10, 0x27, 0, 0, 0x00]); // gold, 10000, no extra lists
        b.extend([0x40, 0x01, 0x23, 1, 0, 0, 0, 0x04, 0x08, EXTRA_WORN, 0x24, 1, 0]); // worn, with a count of 1
        b.extend([0x40, 0x04, 0x56, 1, 0, 0, 0, 0x04, 0x04, EXTRA_WORN_LEFT]);
        b
    }

    #[test]
    fn an_inventory_lists_what_is_worn() {
        let items = inventory(&inventory_bytes(), 0).unwrap();
        let worn: Vec<u32> = items.iter().filter(|i| i.worn).map(|i| i.reference & 0xFFFF).collect();
        assert_eq!(items.len(), 3);
        assert_eq!(worn, [0x0123, 0x0456]);
    }

    #[test]
    fn bytes_that_are_not_an_inventory_are_not_taken_for_one() {
        let mut b = inventory_bytes();
        b[18] = 250; // an extra data type nobody knows
        assert!(inventory(&b, 0).is_none());
        assert!(inventory(&[0xFF; 40], 0).is_none());
    }

    #[test]
    fn the_biggest_inventory_in_the_bytes_is_found_among_noise() {
        let mut data = vec![0x13, 0x37, 0x04, 0xAA, 0xBB];
        data.extend(inventory_bytes());
        data.extend([1, 2, 3]);
        let (plugins, lights, ids) = (vec!["Skyrim.esm".into()], vec![], vec![]);
        let outfit = worn(&data, &names(&plugins, &lights, &ids)).unwrap();
        assert_eq!(
            outfit.worn,
            [
                WornItem { key: FormKey { plugin: "skyrim.esm".into(), id: 0x0123 } },
                WornItem { key: FormKey { plugin: "skyrim.esm".into(), id: 0x0456 } },
            ]
        );
    }

    fn wstring(out: &mut Vec<u8>, s: &str) {
        out.extend((s.len() as u16).to_le_bytes());
        out.extend(s.as_bytes());
    }

    /// A small uncompressed SE save: two plugins, one light plugin, the player's change form
    /// holding `inventory`, and one other form before it.
    fn sample_save(inventory: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend(12u32.to_le_bytes());
        header.extend(7u32.to_le_bytes());
        wstring(&mut header, "Lydia");
        header.extend(12u32.to_le_bytes());
        for s in ["Whiterun", "000.09.08", "NordRace"] {
            wstring(&mut header, s);
        }
        header.extend(1u16.to_le_bytes());
        header.extend([0u8; 8 + 8]); // xp, level-up xp, filetime
        header.extend(1u32.to_le_bytes()); // a 1x1 screenshot
        header.extend(1u32.to_le_bytes());
        header.extend(0u16.to_le_bytes()); // uncompressed

        let mut head = b"TESV_SAVEGAME".to_vec();
        head.extend((header.len() as u32).to_le_bytes());
        head.extend(&header);
        head.extend([0u8; 4]); // the screenshot's one RGBA pixel
        head.extend([0u8; 8]); // the two sizes of a compressed body
        let base = head.len() - 8; // what the table's offsets count from

        let mut body = vec![78u8];
        body.extend(0u32.to_le_bytes()); // the plugin info's size, which isn't read
        body.push(2);
        wstring(&mut body, "Skyrim.esm");
        wstring(&mut body, "Mod.esp");
        body.extend(1u16.to_le_bytes());
        wstring(&mut body, "Tiny.esp");
        let table = body.len();
        body.extend([0u8; 40 + 60]);

        let ids_at = body.len();
        body.extend(1u32.to_le_bytes());
        body.extend(0x0100_0000u32.to_le_bytes()); // index 1 is a FormID in Mod.esp: unused here
        let forms_at = body.len();
        // Another actor, then the player (RefID kind 1, value 0x14).
        body.extend([0x40, 0x00, 0x07]);
        body.extend(0u32.to_le_bytes());
        body.extend([1, 78, 3, 0, 1, 2, 3]);
        body.extend([0x40, 0x00, 0x14]);
        body.extend(0u32.to_le_bytes());
        body.extend([1, 78, inventory.len() as u8 + 2, 0, 0xAA, 0xBB]);
        body.extend(inventory);

        let file = |at: usize| (at + base) as u32;
        let words = [file(ids_at), 0, 0, 0, file(forms_at), 0, 0, 0, 0, 2];
        for (i, w) in words.iter().enumerate() {
            body[table + i * 4..table + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        head.extend(body);
        head
    }

    #[test]
    fn the_players_worn_items_come_from_a_whole_save() {
        // Item RefIDs are kind 1 (Skyrim.esm's own FormIDs), so `names` reads them as FormIDs
        // in plugin 0 here.
        let save = sample_save(&inventory_bytes());
        let outfit = outfit(&save).unwrap();
        let keys: Vec<(String, u32)> = outfit.worn.iter().map(|w| (w.key.plugin.clone(), w.key.id)).collect();
        assert_eq!(keys, [("skyrim.esm".to_string(), 0x0123), ("skyrim.esm".to_string(), 0x0456)]);
        assert_eq!(outfit.skipped, 0);
    }

    #[test]
    fn a_save_without_a_player_form_says_so() {
        let mut save = sample_save(&inventory_bytes());
        let at = save.windows(3).rposition(|w| w == [0x40, 0x00, 0x14]).unwrap();
        save[at + 2] = 0x15;
        assert!(outfit(&save).unwrap_err().to_string().contains("no change form for the player"));
    }
}
