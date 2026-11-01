//! Reading Skyrim Special Edition `.bsa` archives (format version 105).
//!
//! Only reads: opening an archive reads its index (folders, file names,
//! sizes, offsets) and nothing else, so even the multi-gigabyte texture
//! archives open quickly. A file's bytes are read on request.
//!
//! Version 105 is what Skyrim SE and its mods use. Oldrim's version 104
//! (zlib) and Fallout 3/New Vegas' 103 are rejected with an error naming the
//! version. Archives that don't store folder and file names can't be looked
//! up by path and are rejected too.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

const MAGIC: &[u8; 4] = b"BSA\0";
const VERSION_SSE: u32 = 105;
const HEADER_LEN: u64 = 36;
const FOLDER_RECORD_LEN: u64 = 24;
const FILE_RECORD_LEN: u64 = 16;

// Archive flags.
const FLAG_FOLDER_NAMES: u32 = 0x1;
const FLAG_FILE_NAMES: u32 = 0x2;
const FLAG_COMPRESSED: u32 = 0x4;
const FLAG_EMBEDDED_NAMES: u32 = 0x100;

/// In a file record's size field: this file's compression is the opposite of
/// the archive's default.
const SIZE_TOGGLE_COMPRESSION: u32 = 0x4000_0000;

/// No file in a Skyrim archive is anywhere near this large; a bigger
/// declared size means a corrupt archive, and we refuse to allocate for it.
const MAX_FILE_LEN: u64 = 1 << 30;

/// One file in an archive.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Folder and file name as stored (original case), joined with `\`.
    pub path: String,
    /// Bytes stored in the archive, compressed if `compressed`.
    pub stored_size: u32,
    offset: u32,
    pub compressed: bool,
}

/// An opened archive's index. Cheap to keep around: file contents stay on
/// disk until [`Archive::read`] asks for them.
#[derive(Debug)]
pub struct Archive {
    path: PathBuf,
    flags: u32,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
}

/// Lower-case a path and use `\` between folders, the form archives and the
/// game compare by. Doesn't strip a leading `Data\`; see
/// [`crate::assets::normalize`] for that.
pub fn normalize_path(path: &str) -> String {
    path.replace('/', "\\").to_ascii_lowercase()
}

impl Archive {
    /// Read the index of the archive at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let file_len = file.metadata()?.len();
        Self::read_index(BufReader::new(file), file_len, path)
            .with_context(|| format!("reading the index of {}", path.display()))
    }

    fn read_index<R: Read>(mut r: R, file_len: u64, path: &Path) -> Result<Self> {
        let mut h = [0u8; HEADER_LEN as usize];
        r.read_exact(&mut h).context("file is shorter than a BSA header")?;
        ensure!(&h[0..4] == MAGIC, "not a BSA (bad magic)");
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let version = u32_at(4);
        if version != VERSION_SSE {
            bail!("BSA version {version} isn't supported (only Skyrim SE's version {VERSION_SSE})");
        }
        let folder_offset = u32_at(8) as u64;
        let flags = u32_at(12);
        let folder_count = u32_at(16) as u64;
        let file_count = u32_at(20) as u64;
        let folder_names_len = u32_at(24) as u64;
        let file_names_len = u32_at(28) as u64;
        ensure!(
            flags & FLAG_FOLDER_NAMES != 0 && flags & FLAG_FILE_NAMES != 0,
            "archive doesn't store folder and file names, so files can't be found by path"
        );
        ensure!(folder_offset == HEADER_LEN, "unexpected folder records offset {folder_offset}");
        // Everything the index needs must fit in the file: guards against
        // allocating for a corrupt count.
        let index_len = HEADER_LEN
            + FOLDER_RECORD_LEN * folder_count
            + FILE_RECORD_LEN * file_count
            + folder_names_len
            + file_names_len;
        ensure!(index_len <= file_len, "index ({index_len} bytes) is larger than the file ({file_len})");

        // Folder records: only the file count of each is needed; the file
        // record blocks that follow are read in the same order.
        let mut folder_file_counts = Vec::with_capacity(folder_count as usize);
        let mut rec = [0u8; FOLDER_RECORD_LEN as usize];
        for _ in 0..folder_count {
            r.read_exact(&mut rec)?;
            folder_file_counts.push(u32::from_le_bytes(rec[8..12].try_into().unwrap()));
        }
        ensure!(
            folder_file_counts.iter().map(|&c| c as u64).sum::<u64>() == file_count,
            "folder records disagree with the header's file count"
        );

        // File record blocks: per folder, its name then one record per file.
        struct Raw {
            folder: String,
            size: u32,
            offset: u32,
        }
        let mut raw = Vec::with_capacity(file_count as usize);
        for &count in &folder_file_counts {
            let mut len = [0u8; 1];
            r.read_exact(&mut len)?;
            let mut name = vec![0u8; len[0] as usize];
            r.read_exact(&mut name)?;
            let folder = cp1252(name.strip_suffix(&[0]).unwrap_or(&name));
            for _ in 0..count {
                let mut f = [0u8; FILE_RECORD_LEN as usize];
                r.read_exact(&mut f)?;
                raw.push(Raw {
                    folder: folder.clone(),
                    size: u32::from_le_bytes(f[8..12].try_into().unwrap()),
                    offset: u32::from_le_bytes(f[12..16].try_into().unwrap()),
                });
            }
        }

        // File names: one NUL-terminated name per file, in the same order.
        let mut names = vec![0u8; file_names_len as usize];
        r.read_exact(&mut names)?;
        let mut names = names.split(|&b| b == 0);

        let default_compressed = flags & FLAG_COMPRESSED != 0;
        let mut entries = Vec::with_capacity(raw.len());
        let mut index = HashMap::with_capacity(raw.len());
        for (i, f) in raw.into_iter().enumerate() {
            let name = names.next().with_context(|| format!("file name {i} is missing"))?;
            let path = format!("{}\\{}", f.folder, cp1252(name));
            let compressed = default_compressed != (f.size & SIZE_TOGGLE_COMPRESSION != 0);
            index.insert(normalize_path(&path), entries.len());
            entries.push(Entry { path, stored_size: f.size & !SIZE_TOGGLE_COMPRESSION & 0x3FFF_FFFF, offset: f.offset, compressed });
        }
        Ok(Archive { path: path.to_path_buf(), flags, entries, index })
    }

    /// Where the archive is on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every file, in archive order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Index of `path` (any case, `/` or `\`) in [`Self::entries`].
    pub fn find(&self, path: &str) -> Option<usize> {
        self.index.get(&normalize_path(path)).copied()
    }

    /// The decompressed contents of the file at `entries()[i]`.
    pub fn read(&self, i: usize) -> Result<Vec<u8>> {
        let e = self.entries.get(i).context("no such entry")?;
        self.read_entry(e).with_context(|| format!("reading {} from {}", e.path, self.path.display()))
    }

    /// The decompressed contents of `path`, or `None` if it isn't in the archive.
    pub fn read_path(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.find(path).map(|i| self.read(i)).transpose()
    }

    fn read_entry(&self, e: &Entry) -> Result<Vec<u8>> {
        ensure!((e.stored_size as u64) <= MAX_FILE_LEN, "stored size {} is implausible", e.stored_size);
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(e.offset as u64))?;
        let mut data = vec![0u8; e.stored_size as usize];
        file.read_exact(&mut data).context("archive is shorter than its index says")?;
        let mut body = &data[..];
        if self.flags & FLAG_EMBEDDED_NAMES != 0 {
            // The full path is stored in front of the data: a length byte, then the name.
            let n = *body.first().context("empty file")? as usize;
            ensure!(body.len() > n, "embedded name is longer than the file");
            body = &body[1 + n..];
        }
        if !e.compressed {
            return Ok(body.to_vec());
        }
        ensure!(body.len() >= 4, "compressed file has no size prefix");
        let original = u32::from_le_bytes(body[0..4].try_into().unwrap()) as u64;
        ensure!(original <= MAX_FILE_LEN, "uncompressed size {original} is implausible");
        let mut out = Vec::with_capacity(original as usize);
        // Read one byte past the declared size so a stream that's too long is noticed.
        lz4_flex::frame::FrameDecoder::new(&body[4..]).take(original + 1).read_to_end(&mut out).context("decompressing (lz4)")?;
        ensure!(out.len() as u64 == original, "decompressed {} bytes, expected {original}", out.len());
        Ok(out)
    }
}

/// Names in archives are Windows-1252. ASCII is the same in both, and a
/// lossy conversion is fine for the few accented names that exist.
fn cp1252(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// Build a version-105 archive in memory. `files` are `(folder, name,
    /// contents, compress)`; `default_compressed` is the archive flag.
    pub(crate) fn build(files: &[(&str, &str, &[u8], bool)], default_compressed: bool, embed_names: bool) -> Vec<u8> {
        // Group by folder, keeping the given order.
        let mut folders: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, f) in files.iter().enumerate() {
            match folders.iter_mut().find(|(n, _)| *n == f.0) {
                Some((_, v)) => v.push(i),
                None => folders.push((f.0, vec![i])),
            }
        }
        let flags = FLAG_FOLDER_NAMES
            | FLAG_FILE_NAMES
            | if default_compressed { FLAG_COMPRESSED } else { 0 }
            | if embed_names { FLAG_EMBEDDED_NAMES } else { 0 };
        let folder_names_len: usize = folders.iter().map(|(n, _)| n.len() + 2).sum();
        let file_names_len: usize = files.iter().map(|f| f.1.len() + 1).sum();
        let index_len = 36 + 24 * folders.len() + 16 * files.len() + folder_names_len + file_names_len;
        // Data blocks, in file order, after the index.
        let mut data = Vec::new();
        let mut loc = vec![(0u32, 0u32); files.len()];
        for (i, (folder, name, content, compress)) in files.iter().enumerate() {
            let mut blob = Vec::new();
            if embed_names {
                let full = format!("{folder}\\{name}");
                blob.push(full.len() as u8);
                blob.extend_from_slice(full.as_bytes());
            }
            if *compress {
                blob.extend_from_slice(&(content.len() as u32).to_le_bytes());
                let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
                enc.write_all(content).unwrap();
                blob.extend_from_slice(&enc.finish().unwrap());
            } else {
                blob.extend_from_slice(content);
            }
            let mut size = blob.len() as u32;
            if *compress != default_compressed {
                size |= SIZE_TOGGLE_COMPRESSION;
            }
            loc[i] = (size, (index_len + data.len()) as u32);
            data.extend_from_slice(&blob);
        }
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        for v in [VERSION_SSE, 36, flags, folders.len() as u32, files.len() as u32, folder_names_len as u32, file_names_len as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&[0u8; 4]); // file flags + padding
        for (_, idx) in &folders {
            out.extend_from_slice(&[0u8; 8]); // hash (unchecked)
            out.extend_from_slice(&(idx.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0u8; 12]); // padding + offset (unused here)
        }
        for (name, idx) in &folders {
            out.push(name.len() as u8 + 1);
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            for &i in idx {
                out.extend_from_slice(&[0u8; 8]);
                out.extend_from_slice(&loc[i].0.to_le_bytes());
                out.extend_from_slice(&loc[i].1.to_le_bytes());
            }
        }
        for name in folders.iter().flat_map(|(_, idx)| idx.iter().map(|&i| files[i].1)) {
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        assert_eq!(out.len(), index_len);
        out.extend_from_slice(&data);
        out
    }

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skyplug_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const A: &[u8] = b"plain file contents";
    fn squashable() -> Vec<u8> {
        b"abcabcabc".iter().cycle().take(5000).copied().collect()
    }

    fn open_built(bytes: &[u8], tag: &str) -> Archive {
        let dir = temp_dir(tag);
        let p = dir.join("t.bsa");
        std::fs::write(&p, bytes).unwrap();
        Archive::open(&p).unwrap()
    }

    #[test]
    fn reads_uncompressed_and_compressed_files() {
        let big = squashable();
        let bytes = build(&[("textures\\a", "One.dds", A, false), ("textures\\a", "Two.dds", &big, true), ("meshes", "x.nif", b"nif", false)], false, false);
        let a = open_built(&bytes, "bsa_basic");
        assert_eq!(a.len(), 3);
        assert_eq!(a.entries()[0].path, "textures\\a\\One.dds");
        assert!(!a.entries()[0].compressed && a.entries()[1].compressed);
        assert_eq!(a.read_path("TEXTURES/A/one.DDS").unwrap().unwrap(), A);
        assert_eq!(a.read_path("textures\\a\\two.dds").unwrap().unwrap(), big);
        assert_eq!(a.read_path("meshes\\x.nif").unwrap().unwrap(), b"nif");
        assert!(a.read_path("meshes\\missing.nif").unwrap().is_none());
    }

    #[test]
    fn archive_wide_compression_and_embedded_names() {
        let big = squashable();
        // Compressed by default, with one file stored plain (toggle bit), names embedded.
        let bytes = build(&[("f", "a.bin", &big, true), ("f", "b.bin", A, false)], true, true);
        let a = open_built(&bytes, "bsa_default_compressed");
        assert!(a.entries()[0].compressed && !a.entries()[1].compressed);
        assert_eq!(a.read(0).unwrap(), big);
        assert_eq!(a.read(1).unwrap(), A);
    }

    #[test]
    fn rejects_other_versions_and_garbage() {
        let mut bytes = build(&[("f", "a", A, false)], false, false);
        bytes[4] = 104;
        let dir = temp_dir("bsa_ver");
        std::fs::write(dir.join("old.bsa"), &bytes).unwrap();
        let e = format!("{:#}", Archive::open(dir.join("old.bsa")).unwrap_err());
        assert!(e.contains("version 104"), "{e}");
        std::fs::write(dir.join("junk.bsa"), b"not an archive at all, but longer than a header....").unwrap();
        assert!(format!("{:#}", Archive::open(dir.join("junk.bsa")).unwrap_err()).contains("bad magic"));
        std::fs::write(dir.join("short.bsa"), b"BSA").unwrap();
        assert!(Archive::open(dir.join("short.bsa")).is_err());
    }

    #[test]
    fn corrupt_counts_do_not_allocate() {
        let mut bytes = build(&[("f", "a", A, false)], false, false);
        bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes()); // file count
        let dir = temp_dir("bsa_corrupt");
        std::fs::write(dir.join("c.bsa"), &bytes).unwrap();
        assert!(Archive::open(dir.join("c.bsa")).is_err());
    }

    #[test]
    fn truncated_data_is_an_error_not_a_panic() {
        let mut bytes = build(&[("f", "a", &squashable(), true)], false, false);
        bytes.truncate(bytes.len() - 10);
        let a = open_built(&bytes, "bsa_trunc");
        assert!(a.read(0).is_err());
    }
}
