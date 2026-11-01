//! Reading the records of Skyrim plugins (`.esp`, `.esm`, `.esl`).
//!
//! A plugin is a header record (`TES4`, holding the flags and the list of
//! masters) followed by top-level groups, one per record type (`ARMO`,
//! `ARMA`, ...). [`PluginFile::open`] reads the header and finds the groups
//! without reading them; [`PluginFile::records`] then reads one group, so a
//! 250 MB `Skyrim.esm` costs only the few megabytes of the type asked for.
//!
//! Only reading, and only what is needed to look things up: records of the
//! types in simple top-level groups (no nested cell or world groups), their
//! subrecords, zlib-compressed bodies, and FormIDs resolved to the plugin
//! that defines them. It never writes a plugin.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// `TES4` flags.
pub const FLAG_MASTER: u32 = 0x0000_0001;
pub const FLAG_LOCALIZED: u32 = 0x0000_0080;
pub const FLAG_LIGHT: u32 = 0x0000_0200;
/// A record's body is zlib-compressed, after a 4-byte uncompressed size.
const RECORD_COMPRESSED: u32 = 0x0004_0000;

const HEADER_LEN: usize = 24;
/// No record or group we read is anywhere near this large; a bigger declared
/// size means a corrupt file, and we refuse to allocate for it.
const MAX_GROUP_LEN: u64 = 1 << 30;
const MAX_RECORD_LEN: usize = 1 << 26;

/// A plugin's header and where its top-level groups are.
#[derive(Debug, Clone)]
pub struct PluginFile {
    path: PathBuf,
    /// The file name, as it is on disk.
    pub name: String,
    /// The `TES4` record's flags: see [`FLAG_MASTER`], [`FLAG_LOCALIZED`], [`FLAG_LIGHT`].
    pub flags: u32,
    /// The plugins this one lists as masters, in order. A FormID's top byte
    /// indexes this list; an index past its end means this plugin.
    pub masters: Vec<String>,
    groups: Vec<Group>,
}

#[derive(Debug, Clone)]
struct Group {
    label: [u8; 4],
    /// Where the group's records start, and where the group ends.
    start: u64,
    end: u64,
}

/// One record, with its body already decompressed.
#[derive(Debug, Clone)]
pub struct Record {
    pub kind: [u8; 4],
    pub flags: u32,
    /// As stored in the plugin: the top byte indexes the plugin's masters.
    pub form_id: u32,
    data: Vec<u8>,
}

/// The records of one group, and what couldn't be read.
#[derive(Debug, Default)]
pub struct RecordList {
    pub records: Vec<Record>,
    /// Records skipped, with why (a compressed body that wouldn't decompress).
    pub problems: Vec<String>,
}

/// One subrecord of a record: a four-letter type and its bytes.
#[derive(Debug, Clone, Copy)]
pub struct Sub<'a> {
    pub kind: [u8; 4],
    pub data: &'a [u8],
}

impl Sub<'_> {
    /// A NUL-terminated string (Windows-1252, like the game's).
    pub fn string(&self) -> String {
        let end = self.data.iter().position(|&b| b == 0).unwrap_or(self.data.len());
        decode_text(&self.data[..end])
    }

    /// The first four bytes as a number (a FormID, or a string table index).
    pub fn u32(&self) -> Option<u32> {
        Some(u32::from_le_bytes(self.data.get(..4)?.try_into().ok()?))
    }

    pub fn is(&self, kind: &[u8; 4]) -> bool {
        &self.kind == kind
    }
}

/// Text from the game's files: UTF-8 if it is, else Windows-1252.
pub fn decode_text(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    }
}

/// Iterates a record's subrecords. A malformed tail ends it quietly.
pub struct Subrecords<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Iterator for Subrecords<'a> {
    type Item = Sub<'a>;

    fn next(&mut self) -> Option<Sub<'a>> {
        let head = self.d.get(self.p..self.p + 6)?;
        let kind: [u8; 4] = head[..4].try_into().ok()?;
        let mut len = u16::from_le_bytes([head[4], head[5]]) as usize;
        self.p += 6;
        // `XXXX` carries the real size of the subrecord after it, which has size 0.
        if &kind == b"XXXX" {
            len = u32::from_le_bytes(self.d.get(self.p..self.p + 4)?.try_into().ok()?) as usize;
            self.p += 4;
            let head = self.d.get(self.p..self.p + 6)?;
            let kind: [u8; 4] = head[..4].try_into().ok()?;
            self.p += 6;
            let data = self.d.get(self.p..self.p.checked_add(len)?)?;
            self.p += len;
            return Some(Sub { kind, data });
        }
        let data = self.d.get(self.p..self.p + len)?;
        self.p += len;
        Some(Sub { kind, data })
    }
}

impl Record {
    pub fn subrecords(&self) -> Subrecords<'_> {
        Subrecords { d: &self.data, p: 0 }
    }

    /// The record's editor ID (`EDID`), if it has one.
    pub fn editor_id(&self) -> Option<String> {
        self.subrecords().find(|s| s.is(b"EDID")).map(|s| s.string())
    }

    /// The record's type as text, for messages.
    pub fn kind_name(&self) -> String {
        String::from_utf8_lossy(&self.kind).into_owned()
    }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

impl PluginFile {
    /// Read the header of the plugin at `path` and find its top-level groups.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len();
        Self::read(BufReader::new(file), len, path).with_context(|| format!("reading {}", path.display()))
    }

    fn read<R: Read + Seek>(mut r: R, len: u64, path: &Path) -> Result<Self> {
        let mut head = [0u8; HEADER_LEN];
        r.read_exact(&mut head).context("file is shorter than a record header")?;
        ensure!(&head[..4] == b"TES4", "not a plugin (no TES4 header record)");
        let size = u32_at(&head, 4) as usize;
        ensure!(size <= MAX_RECORD_LEN && HEADER_LEN as u64 + size as u64 <= len, "header record is larger than the file");
        let flags = u32_at(&head, 8);
        let mut body = vec![0u8; size];
        r.read_exact(&mut body)?;
        let header = Record { kind: *b"TES4", flags: 0, form_id: 0, data: body };
        let masters = header.subrecords().filter(|s| s.is(b"MAST")).map(|s| s.string()).collect();

        let mut groups = Vec::new();
        let mut pos = HEADER_LEN as u64 + size as u64;
        while pos + HEADER_LEN as u64 <= len {
            r.seek(SeekFrom::Start(pos))?;
            r.read_exact(&mut head)?;
            if &head[..4] != b"GRUP" {
                bail!("expected a group at byte {pos}, found {:?}", String::from_utf8_lossy(&head[..4]));
            }
            let size = u32_at(&head, 4) as u64;
            ensure!(size >= HEADER_LEN as u64 && pos + size <= len, "group at byte {pos} runs past the end of the file");
            // Group type 0 is a top-level group, labelled with its record type.
            if u32_at(&head, 12) == 0 {
                groups.push(Group { label: head[8..12].try_into().unwrap(), start: pos + HEADER_LEN as u64, end: pos + size });
            }
            pos += size;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(PluginFile { path: path.to_path_buf(), name, flags, masters, groups })
    }

    /// Where the plugin is on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_localized(&self) -> bool {
        self.flags & FLAG_LOCALIZED != 0
    }

    pub fn is_light(&self) -> bool {
        self.flags & FLAG_LIGHT != 0
    }

    /// Whether the plugin has a top-level group for this record type.
    pub fn has_group(&self, label: &[u8; 4]) -> bool {
        self.groups.iter().any(|g| &g.label == label)
    }

    fn group_bytes(&self, label: &[u8; 4]) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for g in self.groups.iter().filter(|g| &g.label == label) {
            let len = g.end - g.start;
            ensure!(len <= MAX_GROUP_LEN, "the {} group is implausibly large", String::from_utf8_lossy(label));
            let mut f = File::open(&self.path)?;
            f.seek(SeekFrom::Start(g.start))?;
            let mut buf = vec![0u8; len as usize];
            f.read_exact(&mut buf).context("the file is shorter than its group says")?;
            out.push(buf);
        }
        Ok(out)
    }

    /// How many records the group of this type holds (without reading their bodies' contents).
    pub fn count(&self, label: &[u8; 4]) -> Result<usize> {
        let mut n = 0;
        for buf in self.group_bytes(label)? {
            walk(&buf, |_| n += 1, |_| ());
        }
        Ok(n)
    }

    /// Every record of this type (nested groups skipped), bodies decompressed.
    pub fn records(&self, label: &[u8; 4]) -> Result<RecordList> {
        let mut list = RecordList::default();
        for buf in self.group_bytes(label)? {
            walk(
                &buf,
                |raw| match raw.into_record() {
                    Ok(r) => list.records.push(r),
                    Err(e) => list.problems.push(e),
                },
                |_| (),
            );
        }
        Ok(list)
    }

    /// The plugin that defines the record `form_id` refers to: the master its top byte
    /// indexes, or this plugin if the index is past the masters. `id` is the 24-bit part.
    pub fn resolve(&self, form_id: u32) -> (String, u32) {
        let index = (form_id >> 24) as usize;
        (self.masters.get(index).unwrap_or(&self.name).to_ascii_lowercase(), form_id & 0x00FF_FFFF)
    }
}

/// A record before its body is decompressed.
struct Raw<'a> {
    kind: [u8; 4],
    flags: u32,
    form_id: u32,
    body: &'a [u8],
}

impl Raw<'_> {
    fn into_record(self) -> std::result::Result<Record, String> {
        let name = || format!("{} {:08X}", String::from_utf8_lossy(&self.kind), self.form_id);
        let data = if self.flags & RECORD_COMPRESSED != 0 {
            let size = self.body.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or_else(|| format!("{}: compressed body is empty", name()))?;
            if size > MAX_RECORD_LEN {
                return Err(format!("{}: implausible uncompressed size {size}", name()));
            }
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&self.body[4..], size)
                .map_err(|e| format!("{}: {e:?}", name()))?
        } else {
            self.body.to_vec()
        };
        Ok(Record { kind: self.kind, flags: self.flags, form_id: self.form_id, data })
    }
}

/// Walk the records of a group's bytes, calling `record` for each and `group` for each nested
/// group (whose contents are skipped). A damaged tail ends the walk.
fn walk<'a>(buf: &'a [u8], mut record: impl FnMut(Raw<'a>), mut group: impl FnMut(&'a [u8])) {
    let mut p = 0;
    while p + HEADER_LEN <= buf.len() {
        let head = &buf[p..p + HEADER_LEN];
        let size = u32_at(head, 4) as usize;
        if &head[..4] == b"GRUP" {
            // A group's size includes its own header.
            let Some(end) = p.checked_add(size).filter(|&e| size >= HEADER_LEN && e <= buf.len()) else { return };
            group(&buf[p..end]);
            p = end;
        } else {
            let Some(end) = (p + HEADER_LEN).checked_add(size).filter(|&e| e <= buf.len()) else { return };
            record(Raw { kind: head[..4].try_into().unwrap(), flags: u32_at(head, 8), form_id: u32_at(head, 12), body: &buf[p + HEADER_LEN..end] });
            p = end;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A subrecord.
    pub(crate) fn sub(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut o = kind.to_vec();
        o.extend_from_slice(&(data.len() as u16).to_le_bytes());
        o.extend_from_slice(data);
        o
    }

    pub(crate) fn zstr(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    /// A record with these subrecords; `compress` stores the body zlib-compressed.
    pub(crate) fn record(kind: &[u8; 4], form_id: u32, subs: &[Vec<u8>], compress: bool) -> Vec<u8> {
        let body: Vec<u8> = subs.concat();
        let (flags, body) = if compress {
            let mut enc = flate_like_compress(&body);
            let mut with_size = (body.len() as u32).to_le_bytes().to_vec();
            with_size.append(&mut enc);
            (RECORD_COMPRESSED, with_size)
        } else {
            (0, body)
        };
        let mut o = kind.to_vec();
        o.extend_from_slice(&(body.len() as u32).to_le_bytes());
        o.extend_from_slice(&flags.to_le_bytes());
        o.extend_from_slice(&form_id.to_le_bytes());
        o.extend_from_slice(&[0u8; 8]);
        o.extend_from_slice(&body);
        o
    }

    /// zlib "stored" blocks: valid zlib without needing a compressor.
    fn flate_like_compress(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut chunks = data.chunks(65535).peekable();
        if data.is_empty() {
            out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
        }
        while let Some(c) = chunks.next() {
            out.push(if chunks.peek().is_none() { 1 } else { 0 });
            out.extend_from_slice(&(c.len() as u16).to_le_bytes());
            out.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
            out.extend_from_slice(c);
        }
        // Adler-32 of the data.
        let (mut a, mut b) = (1u32, 0u32);
        for &x in data {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    pub(crate) fn group(label: &[u8; 4], records: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = records.concat();
        let mut o = b"GRUP".to_vec();
        o.extend_from_slice(&((body.len() + HEADER_LEN) as u32).to_le_bytes());
        o.extend_from_slice(label);
        o.extend_from_slice(&[0u8; 12]); // type 0, stamp, version
        o.extend_from_slice(&body);
        o
    }

    /// A plugin file: a TES4 header with these flags and masters, then these groups.
    pub(crate) fn plugin(flags: u32, masters: &[&str], groups: &[Vec<u8>]) -> Vec<u8> {
        let mut subs = vec![sub(b"HEDR", &[0u8; 12])];
        for m in masters {
            subs.push(sub(b"MAST", &zstr(m)));
            subs.push(sub(b"DATA", &[0u8; 8]));
        }
        let mut header = record(b"TES4", 0, &subs, false);
        header[8..12].copy_from_slice(&flags.to_le_bytes());
        header.extend(groups.concat());
        header
    }

    pub(crate) fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    fn temp(tag: &str) -> PathBuf {
        crate::bsa::tests::temp_dir(tag)
    }

    #[test]
    fn reads_header_masters_and_groups() {
        let bytes = plugin(
            FLAG_LOCALIZED | FLAG_LIGHT,
            &["Skyrim.esm", "Update.esm"],
            &[
                group(b"ARMO", &[record(b"ARMO", 0x0100_0801, &[sub(b"EDID", &zstr("TestArmour"))], false)]),
                group(b"ARMA", &[]),
            ],
        );
        let p = write(&temp("rec_header"), "Test.esp", &bytes);
        let f = PluginFile::open(&p).unwrap();
        assert_eq!(f.name, "Test.esp");
        assert_eq!(f.masters, ["Skyrim.esm", "Update.esm"]);
        assert!(f.is_localized() && f.is_light());
        assert!(f.has_group(b"ARMO") && f.has_group(b"ARMA") && !f.has_group(b"WEAP"));
        assert_eq!(f.count(b"ARMO").unwrap(), 1);
        assert_eq!(f.count(b"ARMA").unwrap(), 0);
        assert_eq!(f.count(b"WEAP").unwrap(), 0);
    }

    #[test]
    fn reads_records_subrecords_and_compressed_bodies() {
        let subs = vec![sub(b"EDID", &zstr("Iron")), sub(b"MODL", &0x0000_0012u32.to_le_bytes()), sub(b"MODL", &0x0100_0034u32.to_le_bytes())];
        let bytes = plugin(0, &["Skyrim.esm"], &[group(b"ARMO", &[record(b"ARMO", 0x0200_0800, &subs, false), record(b"ARMO", 0x0200_0801, &subs, true)])]);
        let p = write(&temp("rec_records"), "T.esp", &bytes);
        let f = PluginFile::open(&p).unwrap();
        let list = f.records(b"ARMO").unwrap();
        assert!(list.problems.is_empty());
        assert_eq!(list.records.len(), 2);
        for r in &list.records {
            assert_eq!(r.editor_id().as_deref(), Some("Iron"));
            let forms: Vec<u32> = r.subrecords().filter(|s| s.is(b"MODL")).filter_map(|s| s.u32()).collect();
            assert_eq!(forms, [0x12, 0x0100_0034]);
        }
        // The top byte of a FormID indexes the masters; past the end means the plugin itself.
        assert_eq!(f.resolve(0x0000_0012), ("skyrim.esm".to_string(), 0x12));
        assert_eq!(f.resolve(0x0200_0800), ("t.esp".to_string(), 0x800));
        assert_eq!(f.resolve(0x0100_0800), ("t.esp".to_string(), 0x800));
    }

    #[test]
    fn nested_groups_are_skipped_and_xxxx_sizes_are_honoured() {
        let big = vec![7u8; 70_000];
        let mut xxxx = b"XXXX".to_vec();
        xxxx.extend_from_slice(&4u16.to_le_bytes());
        xxxx.extend_from_slice(&(big.len() as u32).to_le_bytes());
        xxxx.extend_from_slice(b"DATA");
        xxxx.extend_from_slice(&0u16.to_le_bytes());
        xxxx.extend_from_slice(&big);
        let rec = record(b"ARMO", 1, &[xxxx, sub(b"EDID", &zstr("AfterBig"))], false);
        let nested = group(b"\x01\0\0\0", &[record(b"ARMO", 99, &[], false)]);
        let bytes = plugin(0, &[], &[group(b"ARMO", &[nested, rec])]);
        let p = write(&temp("rec_xxxx"), "X.esp", &bytes);
        let list = PluginFile::open(&p).unwrap().records(b"ARMO").unwrap();
        assert_eq!(list.records.len(), 1, "the nested group's record isn't read");
        let subs: Vec<_> = list.records[0].subrecords().collect();
        assert_eq!(subs[0].data.len(), 70_000);
        assert_eq!(list.records[0].editor_id().as_deref(), Some("AfterBig"));
    }

    #[test]
    fn damaged_files_are_errors_or_partial_never_panics() {
        let bytes = plugin(0, &["Skyrim.esm"], &[group(b"ARMO", &[record(b"ARMO", 1, &[sub(b"EDID", &zstr("A"))], true), record(b"ARMO", 2, &[sub(b"EDID", &zstr("B"))], false)])]);
        let dir = temp("rec_damage");
        // Cut at every length, and set each byte to a few values.
        for cut in 0..bytes.len() {
            let p = write(&dir, "cut.esp", &bytes[..cut]);
            if let Ok(f) = PluginFile::open(&p) {
                let _ = f.records(b"ARMO");
                let _ = f.count(b"ARMO");
            }
        }
        for i in 0..bytes.len() {
            for v in [0x00u8, 0xFF, 0x7F] {
                let mut b = bytes.clone();
                b[i] = v;
                let p = write(&dir, "flip.esp", &b);
                if let Ok(f) = PluginFile::open(&p) {
                    let _ = f.records(b"ARMO");
                }
            }
        }
        // A compressed body that isn't zlib is a problem listed, and the next record still reads.
        let mut broken = record(b"ARMO", 1, &[sub(b"EDID", &zstr("A"))], true);
        let n = broken.len();
        broken[n - 6..].copy_from_slice(&[9, 9, 9, 9, 9, 9]);
        let bytes = plugin(0, &[], &[group(b"ARMO", &[broken, record(b"ARMO", 2, &[sub(b"EDID", &zstr("B"))], false)])]);
        let list = PluginFile::open(write(&dir, "broken.esp", &bytes)).unwrap().records(b"ARMO").unwrap();
        assert_eq!(list.problems.len(), 1, "{:?}", list.problems);
        assert_eq!(list.records.len(), 1);
        assert_eq!(list.records[0].editor_id().as_deref(), Some("B"));
    }

    #[test]
    fn not_a_plugin() {
        let dir = temp("rec_not");
        assert!(PluginFile::open(write(&dir, "a.esp", b"hello world, definitely not a plugin..")).is_err());
        assert!(PluginFile::open(dir.join("missing.esp")).is_err());
    }

    #[test]
    fn text_decodes_utf8_or_windows_1252() {
        assert_eq!(decode_text("Nørd".as_bytes()), "Nørd");
        assert_eq!(decode_text(&[b'N', 0xF8, b'r', b'd']), "Nørd");
    }
}
