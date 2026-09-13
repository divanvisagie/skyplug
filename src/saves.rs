use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

const MAGIC: &str = "TESV_SAVEGAME";

/// The handful of header fields worth surfacing on the command line.
/// Deliberately doesn't carry sex/XP/filetime/screenshot bytes — nothing
/// downstream needs them, and skipping them keeps parsing to the small
/// header prefix only (no need to touch the multi-MB screenshot/body).
#[derive(Debug, Clone)]
pub struct SaveHeader {
    pub player_name: String,
    pub player_level: u32,
    pub player_location: String,
    pub game_date: String,
    pub player_race_editor_id: String,
    pub save_number: u32,
    pub screenshot_width: u32,
    pub screenshot_height: u32,
    pub compression_type: u16,
    pub is_se: bool,
}

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

/// A length-prefixed (u16) string, as used throughout the save format.
fn read_wstring(buf: &[u8], pos: usize) -> Result<(String, usize)> {
    let (len, pos) = read_u16(buf, pos)?;
    let end = pos + len as usize;
    let chunk = buf.get(pos..end).context("unexpected end of save data")?;
    Ok((String::from_utf8_lossy(chunk).into_owned(), end))
}

/// Parse the header fields out of the header block (the `header_size`
/// bytes immediately following the magic string and header-size fields —
/// NOT including the screenshot or anything after it).
fn read_header(buf: &[u8], pos: usize) -> Result<(SaveHeader, usize)> {
    let (version, pos) = read_u32(buf, pos)?;
    let is_se = version == 12;

    let (save_number, pos) = read_u32(buf, pos)?;
    let (player_name, pos) = read_wstring(buf, pos)?;
    let (player_level, pos) = read_u32(buf, pos)?;
    let (player_location, pos) = read_wstring(buf, pos)?;
    let (game_date, pos) = read_wstring(buf, pos)?;
    let (player_race_editor_id, pos) = read_wstring(buf, pos)?;
    let (_player_sex, pos) = read_u16(buf, pos)?;
    let (_player_current_xp, pos) = read_f32(buf, pos)?;
    let (_player_level_up_xp, pos) = read_f32(buf, pos)?;
    let (_filetime_low, pos) = read_u32(buf, pos)?;
    let (_filetime_high, pos) = read_u32(buf, pos)?;
    let (screenshot_width, pos) = read_u32(buf, pos)?;
    let (screenshot_height, pos) = read_u32(buf, pos)?;
    let (compression_type, pos) = if is_se { read_u16(buf, pos)? } else { (0, pos) };

    Ok((
        SaveHeader {
            player_name,
            player_level,
            player_location,
            game_date,
            player_race_editor_id,
            save_number,
            screenshot_width,
            screenshot_height,
            compression_type,
            is_se,
        },
        pos,
    ))
}

/// Read just the header of a `.ess` save — cheap: only the magic string,
/// header-size prefix, and the header block itself are read from disk, so
/// this doesn't touch the (often multi-MB) screenshot or save body.
pub fn read_header_from_file(path: &Path) -> Result<SaveHeader> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;

    let mut prefix = [0u8; 17]; // 13-byte magic string + u32 header_size
    file.read_exact(&mut prefix)
        .with_context(|| format!("reading header prefix of {}", path.display()))?;
    let magic = String::from_utf8_lossy(&prefix[0..13]);
    ensure!(magic == MAGIC, "{}: not a Skyrim save file (bad magic string)", path.display());
    let header_size = u32::from_le_bytes(prefix[13..17].try_into().unwrap());

    let mut header_buf = vec![0u8; header_size as usize];
    file.read_exact(&mut header_buf)
        .with_context(|| format!("reading header of {}", path.display()))?;

    let (header, _) = read_header(&header_buf, 0)?;
    Ok(header)
}

/// List every `.ess` save in `dir`, newest first by filename (Skyrim embeds
/// a `YYYYMMDDHHMMSS` timestamp in the filename, so this is also
/// chronological). Files whose header can't be parsed are skipped with a
/// warning on stderr rather than failing the whole listing.
pub fn list_saves(dir: &Path) -> Result<Vec<SaveEntry>> {
    let mut entries = Vec::new();
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
            Ok(header) => entries.push(SaveEntry { file_name, path, header }),
            Err(e) => eprintln!("warning: skipping {file_name}: {e}"),
        }
    }

    entries.sort_by(|a, b| b.file_name.cmp(&a.file_name));
    Ok(entries)
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

/// Full parse of a save's plugin list: the list of `.esp`/`.esm`/`.esl`
/// files that were active when it was written. Unlike `read_header_from_file`
/// this reads (and, if needed, decompresses) the whole save body.
pub fn read_plugins_from_file(path: &Path) -> Result<Vec<String>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;

    let mut prefix = [0u8; 17];
    file.read_exact(&mut prefix).context("reading header prefix")?;
    let magic = String::from_utf8_lossy(&prefix[0..13]);
    ensure!(magic == MAGIC, "{}: not a Skyrim save file (bad magic string)", path.display());
    let header_size = u32::from_le_bytes(prefix[13..17].try_into().unwrap());

    let mut header_buf = vec![0u8; header_size as usize];
    file.read_exact(&mut header_buf).context("reading header")?;
    let (header, _) = read_header(&header_buf, 0)?;

    let multiplier = if header.is_se { 4 } else { 3 };
    let screenshot_len = (multiplier * header.screenshot_width * header.screenshot_height) as i64;
    file.seek(SeekFrom::Current(screenshot_len)).context("seeking past screenshot data")?;

    let uncompressed_length = if header.is_se {
        let mut lens = [0u8; 8];
        file.read_exact(&mut lens).context("reading compression lengths")?;
        u32::from_le_bytes(lens[0..4].try_into().unwrap())
    } else {
        0
    };

    let mut body = Vec::new();
    file.read_to_end(&mut body).context("reading save body")?;

    let decompressed;
    let body: &[u8] = match header.compression_type {
        0 => &body,
        2 => {
            decompressed = lz4_flex::block::decompress(&body, uncompressed_length as usize)
                .context("decompressing save body (lz4)")?;
            &decompressed
        }
        other => bail!("{}: unsupported save compression type {other}", path.display()),
    };

    let (_form_version, pos) = read_u8(body, 0)?;
    let (_plugin_info_size, pos) = read_u32(body, pos)?;
    let (plugin_count, mut pos) = read_u8(body, pos)?;

    let mut plugins = Vec::with_capacity(plugin_count as usize);
    for _ in 0..plugin_count {
        let (name, next) = read_wstring(body, pos)?;
        plugins.push(name);
        pos = next;
    }

    Ok(plugins)
}
