use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

const MAGIC: &[u8] = b"TESV_SAVEGAME";
/// 13-byte magic string + u32 header_size.
const PREFIX_LEN: usize = MAGIC.len() + 4;
/// Header `version` value written by Skyrim Special Edition (LE uses 8/9).
const SE_VERSION: u32 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sex {
    Male,
    Female,
    Unknown(u16),
}

impl From<u16> for Sex {
    fn from(raw: u16) -> Self {
        match raw {
            0 => Sex::Male,
            1 => Sex::Female,
            other => Sex::Unknown(other),
        }
    }
}

/// The save's header block (everything between the header-size field and
/// the screenshot). See UESP's "Skyrim Mod:Save File Format".
#[derive(Debug, Clone)]
pub struct SaveHeader {
    pub version: u32,
    pub save_number: u32,
    pub player_name: String,
    pub player_level: u32,
    pub player_location: String,
    pub game_date: String,
    pub player_race_editor_id: String,
    pub player_sex: Sex,
    pub player_current_xp: f32,
    pub player_level_up_xp: f32,
    /// Windows FILETIME (100ns intervals since 1601-01-01 UTC) of when the
    /// save was written.
    pub filetime: u64,
    pub screenshot_width: u32,
    pub screenshot_height: u32,
    /// SE only (always 0 for LE): 0 = none, 1 = zlib, 2 = LZ4 block.
    pub compression_type: u16,
    pub is_se: bool,
}

impl SaveHeader {
    /// Bytes per screenshot pixel: RGBA on SE, RGB on LE.
    pub fn screenshot_bytes_per_pixel(&self) -> usize {
        if self.is_se { 4 } else { 3 }
    }

    pub fn screenshot_len(&self) -> usize {
        self.screenshot_bytes_per_pixel() * self.screenshot_width as usize * self.screenshot_height as usize
    }
}

/// A fully parsed save: header, raw screenshot pixels, and the plugins that
/// were active when it was written.
#[derive(Debug, Clone)]
pub struct SaveFile {
    pub header: SaveHeader,
    /// Raw pixels, `screenshot_width * screenshot_height` of RGBA (SE) or
    /// RGB (LE); see [`SaveHeader::screenshot_bytes_per_pixel`].
    pub screenshot: Vec<u8>,
    pub form_version: u8,
    pub plugins: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SaveEntry {
    pub file_name: String,
    pub path: PathBuf,
    pub header: SaveHeader,
}

fn read_u8(buf: &[u8], pos: usize) -> Result<(u8, usize)> {
    let b = *buf.get(pos).context("unexpected end of save data")?;
    Ok((b, pos + 1))
}

fn read_u16(buf: &[u8], pos: usize) -> Result<(u16, usize)> {
    let end = pos + 2;
    let chunk = buf.get(pos..end).context("unexpected end of save data")?;
    Ok((u16::from_le_bytes(chunk.try_into().unwrap()), end))
}

fn read_u32(buf: &[u8], pos: usize) -> Result<(u32, usize)> {
    let end = pos + 4;
    let chunk = buf.get(pos..end).context("unexpected end of save data")?;
    Ok((u32::from_le_bytes(chunk.try_into().unwrap()), end))
}

fn read_f32(buf: &[u8], pos: usize) -> Result<(f32, usize)> {
    let (bits, end) = read_u32(buf, pos)?;
    Ok((f32::from_bits(bits), end))
}

fn read_bytes(buf: &[u8], pos: usize, len: usize) -> Result<(&[u8], usize)> {
    let end = pos + len;
    let chunk = buf.get(pos..end).context("unexpected end of save data")?;
    Ok((chunk, end))
}

/// A length-prefixed (u16) string, as used throughout the save format.
fn read_wstring(buf: &[u8], pos: usize) -> Result<(String, usize)> {
    let (len, pos) = read_u16(buf, pos)?;
    let (chunk, end) = read_bytes(buf, pos, len as usize)?;
    Ok((String::from_utf8_lossy(chunk).into_owned(), end))
}

/// Validate the magic string and return the header block's size.
fn read_prefix(buf: &[u8]) -> Result<u32> {
    ensure!(buf.len() >= PREFIX_LEN, "unexpected end of save data");
    ensure!(&buf[..MAGIC.len()] == MAGIC, "not a Skyrim save file (bad magic string)");
    Ok(read_u32(buf, MAGIC.len())?.0)
}

/// Parse the header fields out of the header block (the `header_size`
/// bytes immediately following the magic string and header-size fields —
/// NOT including the screenshot or anything after it).
fn read_header(buf: &[u8], pos: usize) -> Result<(SaveHeader, usize)> {
    let (version, pos) = read_u32(buf, pos)?;
    let is_se = version == SE_VERSION;

    let (save_number, pos) = read_u32(buf, pos)?;
    let (player_name, pos) = read_wstring(buf, pos)?;
    let (player_level, pos) = read_u32(buf, pos)?;
    let (player_location, pos) = read_wstring(buf, pos)?;
    let (game_date, pos) = read_wstring(buf, pos)?;
    let (player_race_editor_id, pos) = read_wstring(buf, pos)?;
    let (player_sex, pos) = read_u16(buf, pos)?;
    let (player_current_xp, pos) = read_f32(buf, pos)?;
    let (player_level_up_xp, pos) = read_f32(buf, pos)?;
    let (filetime_low, pos) = read_u32(buf, pos)?;
    let (filetime_high, pos) = read_u32(buf, pos)?;
    let (screenshot_width, pos) = read_u32(buf, pos)?;
    let (screenshot_height, pos) = read_u32(buf, pos)?;
    let (compression_type, pos) = if is_se { read_u16(buf, pos)? } else { (0, pos) };

    Ok((
        SaveHeader {
            version,
            save_number,
            player_name,
            player_level,
            player_location,
            game_date,
            player_race_editor_id,
            player_sex: Sex::from(player_sex),
            player_current_xp,
            player_level_up_xp,
            filetime: (u64::from(filetime_high) << 32) | u64::from(filetime_low),
            screenshot_width,
            screenshot_height,
            compression_type,
            is_se,
        },
        pos,
    ))
}

/// Parse just the header from the start of a save's bytes. `buf` only
/// needs to hold the prefix and header block, not the whole file.
pub fn parse_header(buf: &[u8]) -> Result<SaveHeader> {
    let header_size = read_prefix(buf)? as usize;
    let (block, _) = read_bytes(buf, PREFIX_LEN, header_size)?;
    Ok(read_header(block, 0)?.0)
}

/// Parse a whole save held in memory: header, screenshot, and (after
/// decompressing the body if needed) the plugin list.
pub fn parse_save(buf: &[u8]) -> Result<SaveFile> {
    let header_size = read_prefix(buf)? as usize;
    let (block, pos) = read_bytes(buf, PREFIX_LEN, header_size)?;
    let (header, _) = read_header(block, 0)?;

    let (screenshot, pos) = read_bytes(buf, pos, header.screenshot_len())?;

    let (uncompressed_length, pos) = if header.is_se {
        let (uncompressed, pos) = read_u32(buf, pos)?;
        let (_compressed, pos) = read_u32(buf, pos)?;
        (uncompressed, pos)
    } else {
        (0, pos)
    };

    let body = &buf[pos..];
    let decompressed;
    let body: &[u8] = match header.compression_type {
        0 => body,
        2 => {
            decompressed = lz4_flex::block::decompress(body, uncompressed_length as usize)
                .context("decompressing save body (lz4)")?;
            &decompressed
        }
        other => bail!("unsupported save compression type {other}"),
    };

    let (form_version, pos) = read_u8(body, 0)?;
    let (_plugin_info_size, pos) = read_u32(body, pos)?;
    let (plugin_count, mut pos) = read_u8(body, pos)?;

    let mut plugins = Vec::with_capacity(plugin_count as usize);
    for _ in 0..plugin_count {
        let (name, next) = read_wstring(body, pos)?;
        plugins.push(name);
        pos = next;
    }

    Ok(SaveFile { header, screenshot: screenshot.to_vec(), form_version, plugins })
}

/// Read just the header of a `.ess` save — cheap: only the magic string,
/// header-size prefix, and the header block itself are read from disk, so
/// this doesn't touch the (often multi-MB) screenshot or save body.
pub fn read_header_from_file(path: &Path) -> Result<SaveHeader> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;

    let mut buf = vec![0u8; PREFIX_LEN];
    file.read_exact(&mut buf)
        .with_context(|| format!("reading header prefix of {}", path.display()))?;
    let header_size = read_prefix(&buf).with_context(|| path.display().to_string())?;

    buf.resize(PREFIX_LEN + header_size as usize, 0);
    file.read_exact(&mut buf[PREFIX_LEN..])
        .with_context(|| format!("reading header of {}", path.display()))?;

    parse_header(&buf).with_context(|| path.display().to_string())
}

/// Read and fully parse a save file; see [`parse_save`].
pub fn read_save_from_file(path: &Path) -> Result<SaveFile> {
    let buf = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_save(&buf).with_context(|| path.display().to_string())
}

/// The list of `.esp`/`.esm`/`.esl` files that were active when a save was
/// written. Reads (and, if needed, decompresses) the whole save body.
pub fn read_plugins_from_file(path: &Path) -> Result<Vec<String>> {
    Ok(read_save_from_file(path)?.plugins)
}

/// A `.ess` file [`list_saves`] found but couldn't parse.
#[derive(Debug)]
pub struct SkippedSave {
    pub file_name: String,
    pub error: anyhow::Error,
}

#[derive(Debug, Default)]
pub struct SaveList {
    /// Newest first.
    pub entries: Vec<SaveEntry>,
    pub skipped: Vec<SkippedSave>,
}

/// List every `.ess` save in `dir`, newest first by filename (Skyrim embeds
/// a `YYYYMMDDHHMMSS` timestamp in the filename, so this is also
/// chronological). Files whose header can't be parsed are reported in
/// `skipped` rather than failing the whole listing.
pub fn list_saves(dir: &Path) -> Result<SaveList> {
    let mut list = SaveList::default();
    let read_dir = std::fs::read_dir(dir).with_context(|| format!("reading saves directory {}", dir.display()))?;

    for entry in read_dir {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("ess")) != Some(true) {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().into_owned();
        match read_header_from_file(&path) {
            Ok(header) => list.entries.push(SaveEntry { file_name, path, header }),
            Err(error) => list.skipped.push(SkippedSave { file_name, error }),
        }
    }

    list.entries.sort_by(|a, b| b.file_name.cmp(&a.file_name));
    Ok(list)
}

/// Extract the `YYYYMMDDHHMMSS` timestamp Skyrim embeds in save filenames
/// (e.g. `Save1_..._20260913135623_1_1.ess`) and format it as
/// `YYYY-MM-DD HH:MM:SS`. Returns `None` if the filename doesn't have one.
pub fn save_timestamp(file_name: &str) -> Option<String> {
    file_name
        .split('_')
        .find(|part| part.len() == 14 && part.bytes().all(|b| b.is_ascii_digit()))
        .map(|ts| {
            format!(
                "{}-{}-{} {}:{}:{}",
                &ts[0..4],
                &ts[4..6],
                &ts[6..8],
                &ts[8..10],
                &ts[10..12],
                &ts[12..14]
            )
        })
}

/// One character across a set of saves.
#[derive(Debug, Clone)]
pub struct Character<'a> {
    pub name: &'a str,
    pub save_count: usize,
    pub latest: &'a SaveEntry,
}

/// Group newest-first `entries` by player name. Characters are returned in
/// order of their most recent save, newest first.
pub fn characters(entries: &[SaveEntry]) -> Vec<Character<'_>> {
    let mut out: Vec<Character> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for entry in entries {
        let name = entry.header.player_name.as_str();
        match index.get(name) {
            Some(&i) => out[i].save_count += 1,
            None => {
                index.insert(name, out.len());
                out.push(Character { name, save_count: 1, latest: entry });
            }
        }
    }
    out
}

/// Resolve a user-supplied character-name query to the exact `player_name`
/// as recorded in the saves: an exact case-insensitive match first, then a
/// case-insensitive substring match if that's unambiguous.
pub fn resolve_character(query: &str, entries: &[SaveEntry]) -> Result<String> {
    let mut names: Vec<&str> = entries.iter().map(|e| e.header.player_name.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    if let Some(exact) = names.iter().find(|n| n.eq_ignore_ascii_case(query)) {
        return Ok(exact.to_string());
    }

    let query_lower = query.to_lowercase();
    let matches: Vec<&str> = names.iter().copied().filter(|n| n.to_lowercase().contains(&query_lower)).collect();
    match matches.as_slice() {
        [only] => Ok(only.to_string()),
        [] => bail!("no character matching \"{query}\" found; known characters: {}", names.join(", ")),
        many => bail!("\"{query}\" is ambiguous, matches: {}", many.join(", ")),
    }
}

/// Resolve a user-supplied save query (exact filename, filename without
/// `.ess`, or a substring) to one entry.
pub fn resolve_save<'a>(query: &str, entries: &'a [SaveEntry]) -> Result<&'a SaveEntry> {
    if let Some(exact) = entries.iter().find(|e| e.file_name.eq_ignore_ascii_case(query)) {
        return Ok(exact);
    }

    let stem_matches: Vec<&SaveEntry> = entries
        .iter()
        .filter(|e| {
            let stem = e.file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&e.file_name);
            stem.eq_ignore_ascii_case(query)
        })
        .collect();
    if let [only] = stem_matches.as_slice() {
        return Ok(only);
    }

    let query_lower = query.to_lowercase();
    let substr_matches: Vec<&SaveEntry> = entries.iter().filter(|e| e.file_name.to_lowercase().contains(&query_lower)).collect();
    match substr_matches.as_slice() {
        [only] => Ok(only),
        [] => bail!("no save matching \"{query}\" found"),
        many => {
            let list = many.iter().map(|e| e.file_name.as_str()).collect::<Vec<_>>().join("\n  ");
            bail!("\"{query}\" is ambiguous, matches:\n  {list}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wstring(out: &mut Vec<u8>, s: &str) {
        out.extend((s.len() as u16).to_le_bytes());
        out.extend(s.as_bytes());
    }

    /// Build a minimal uncompressed SE save with a 1x1 screenshot.
    fn sample_save(plugins: &[&str]) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend(SE_VERSION.to_le_bytes());
        header.extend(7u32.to_le_bytes()); // save number
        wstring(&mut header, "Lydia");
        header.extend(12u32.to_le_bytes());
        wstring(&mut header, "Whiterun");
        wstring(&mut header, "000.09.08");
        wstring(&mut header, "NordRace");
        header.extend(1u16.to_le_bytes()); // female
        header.extend(1.5f32.to_le_bytes());
        header.extend(100f32.to_le_bytes());
        header.extend(0x1111_2222u32.to_le_bytes()); // filetime low
        header.extend(0x0000_0001u32.to_le_bytes()); // filetime high
        header.extend(1u32.to_le_bytes()); // screenshot w
        header.extend(1u32.to_le_bytes()); // screenshot h
        header.extend(0u16.to_le_bytes()); // uncompressed

        let mut body = vec![78u8]; // form version
        body.extend(0u32.to_le_bytes()); // plugin info size (unused)
        body.push(plugins.len() as u8);
        for p in plugins {
            wstring(&mut body, p);
        }

        let mut buf = MAGIC.to_vec();
        buf.extend((header.len() as u32).to_le_bytes());
        buf.extend(header);
        buf.extend([1, 2, 3, 4]); // RGBA pixel
        buf.extend((body.len() as u32).to_le_bytes());
        buf.extend((body.len() as u32).to_le_bytes());
        buf.extend(body);
        buf
    }

    #[test]
    fn parses_header_and_plugins() {
        let buf = sample_save(&["Skyrim.esm", "Update.esm"]);
        let save = parse_save(&buf).unwrap();
        assert_eq!(save.header.player_name, "Lydia");
        assert_eq!(save.header.player_level, 12);
        assert_eq!(save.header.player_sex, Sex::Female);
        assert_eq!(save.header.filetime, 0x0000_0001_1111_2222);
        assert!(save.header.is_se);
        assert_eq!(save.screenshot, vec![1, 2, 3, 4]);
        assert_eq!(save.form_version, 78);
        assert_eq!(save.plugins, vec!["Skyrim.esm", "Update.esm"]);
    }

    #[test]
    fn header_only_needs_prefix_and_header_block() {
        let buf = sample_save(&[]);
        let header_len = PREFIX_LEN + read_prefix(&buf).unwrap() as usize;
        let header = parse_header(&buf[..header_len]).unwrap();
        assert_eq!(header.player_location, "Whiterun");
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = sample_save(&[]);
        buf[0] = b'X';
        assert!(parse_save(&buf).is_err());
    }
}
