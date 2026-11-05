//! Finding a game asset (a mesh, a texture) the way the game does: a loose
//! file in `Data` first, then the archives in load order, the later archive
//! winning.
//!
//! The question this answers is "which file will the game actually use for
//! `textures\foo.dds`, and what does it hide?", so [`AssetResolver::resolve`]
//! reports the winner and everything it shadows. It never writes anything.
//!
//! Rules, as the game and this install behave:
//!
//! - Loose files beat archives.
//! - Archives load in this order, and a later one beats an earlier one: the
//!   base game's, as listed by `sResourceArchiveList` and `sResourceArchiveList2`
//!   in `Skyrim_Default.ini` and then `Skyrim.ini` and `SkyrimCustom.ini`; then,
//!   for each enabled plugin in load order, `<plugin>.bsa` and
//!   `<plugin> - Textures.bsa`.
//! - Paths compare case-insensitively, `/` and `\` alike, and may start with
//!   `Data\`, which is ignored.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::GamePaths;
use crate::bsa::Archive;
use crate::plugins::State;

/// The base game's archives if no `.ini` lists any, as in Skyrim SE's own
/// `Skyrim_Default.ini`.
const DEFAULT_BASE_ARCHIVES: &str = "Skyrim - Misc.bsa, Skyrim - Shaders.bsa, Skyrim - Interface.bsa, \
     Skyrim - Animations.bsa, Skyrim - Meshes0.bsa, Skyrim - Meshes1.bsa, Skyrim - Sounds.bsa, \
     Skyrim - Voices_en0.bsa, Skyrim - Textures0.bsa, Skyrim - Textures1.bsa, Skyrim - Textures2.bsa, \
     Skyrim - Textures3.bsa, Skyrim - Textures4.bsa, Skyrim - Textures5.bsa, Skyrim - Textures6.bsa, \
     Skyrim - Textures7.bsa, Skyrim - Textures8.bsa, Skyrim - Patch.bsa";

/// A path in the form assets are compared by: lower case, `\` between
/// folders, no leading `\` and no leading `Data\`.
pub fn normalize(path: &str) -> String {
    let p = crate::bsa::normalize_path(path);
    let p = p.trim_start_matches('\\');
    p.strip_prefix("data\\").unwrap_or(p).to_string()
}

/// Where a file came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file under `Data`, at this real path.
    Loose(PathBuf),
    /// Entry `entry` of archive `archive` (indexes into [`AssetResolver::archives`]).
    Archive { archive: usize, entry: usize },
}

/// The result of looking an asset up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The path asked for, normalized.
    pub path: String,
    /// What the game uses.
    pub winner: Source,
    /// Other copies the winner hides, highest priority first.
    pub shadowed: Vec<Source>,
}

type DirIndex = Arc<HashMap<String, String>>;

/// Looks assets up across `Data` and a list of archives.
pub struct AssetResolver {
    data_dir: PathBuf,
    archives: Vec<Archive>,
    warnings: Vec<String>,
    // Directory listings, keyed by real path, mapping lower-case name to real name.
    dirs: Mutex<HashMap<PathBuf, DirIndex>>,
}

impl AssetResolver {
    /// A resolver over `data_dir` and `archives`, given lowest priority first.
    /// An archive that doesn't exist is skipped; one that can't be read is
    /// skipped and noted in [`Self::warnings`].
    pub fn new(data_dir: impl Into<PathBuf>, archives: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut opened = Vec::new();
        let mut warnings = Vec::new();
        for path in archives {
            if !path.exists() {
                continue;
            }
            match Archive::open(&path) {
                Ok(a) => opened.push(a),
                Err(e) => warnings.push(format!("{:#}", e)),
            }
        }
        AssetResolver { data_dir: data_dir.into(), archives: opened, warnings, dirs: Mutex::new(HashMap::new()) }
    }

    /// A resolver for an install: the base game's archives, then the archives
    /// of each enabled plugin in load order (see the module docs).
    pub fn for_game(paths: &GamePaths) -> Result<Self> {
        let game_dir = paths.data_dir.parent().context("Data folder has no parent")?;
        let docs = paths.saves_dir.parent();
        let mut inis = vec![read_text(&game_dir.join("Skyrim_Default.ini"))];
        if let Some(docs) = docs {
            inis.push(read_text(&docs.join("Skyrim.ini")));
            inis.push(read_text(&docs.join("SkyrimCustom.ini")));
        }
        let inis: Vec<String> = inis.into_iter().flatten().collect();
        let mut base = archive_names_from_ini(&inis.iter().map(String::as_str).collect::<Vec<_>>());
        if base.is_empty() {
            base = split_list(DEFAULT_BASE_ARCHIVES);
        }

        let mut enabled: Vec<_> = paths.plugin_status()?.into_iter().filter(|p| p.state == State::Enabled).collect();
        enabled.sort_by_key(|p| p.load_order);
        let plugins: Vec<String> = enabled.into_iter().map(|p| p.name).collect();

        let files = list_dir(&paths.data_dir);
        let order = archive_order(&files, &base, &plugins);
        Ok(Self::new(paths.data_dir.clone(), order.into_iter().map(|n| paths.data_dir.join(n))))
    }

    /// The archives that opened, lowest priority first.
    pub fn archives(&self) -> &[Archive] {
        &self.archives
    }

    /// Archives that exist but couldn't be read, with why.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Forget the cached `Data` folder listings, so files added or removed
    /// since are seen. Archives are not re-read.
    pub fn clear_cache(&self) {
        self.dirs.lock().unwrap().clear();
    }

    /// Find `path`: the file the game would use and the copies it hides.
    pub fn resolve(&self, path: &str) -> Option<Resolved> {
        let path = normalize(path);
        let mut found = Vec::new();
        if let Some(p) = self.find_loose(&path) {
            found.push(Source::Loose(p));
        }
        for (a, archive) in self.archives.iter().enumerate().rev() {
            if let Some(entry) = archive.find(&path) {
                found.push(Source::Archive { archive: a, entry });
            }
        }
        if found.is_empty() {
            return None;
        }
        let winner = found.remove(0);
        Some(Resolved { path, winner, shadowed: found })
    }

    /// The bytes of a source found by [`Self::resolve`].
    pub fn read(&self, source: &Source) -> Result<Vec<u8>> {
        match source {
            Source::Loose(p) => std::fs::read(p).with_context(|| format!("reading {}", p.display())),
            Source::Archive { archive, entry } => {
                self.archives.get(*archive).context("no such archive")?.read(*entry)
            }
        }
    }

    /// Resolve `path` and read the winner, or `None` if the game has no such file.
    pub fn read_asset(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.resolve(path).map(|r| self.read(&r.winner)).transpose()
    }

    /// A short description of where a source is: the real path of a loose
    /// file relative to `Data`, or the archive's file name.
    pub fn describe(&self, source: &Source) -> String {
        match source {
            // Game paths use `/` everywhere; on Windows the file's own path has `\`.
            Source::Loose(p) => format!("loose: {}", p.strip_prefix(&self.data_dir).unwrap_or(p).to_string_lossy().replace('\\', "/")),
            Source::Archive { archive, .. } => self
                .archives
                .get(*archive)
                .and_then(|a| a.path().file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    fn find_loose(&self, path: &str) -> Option<PathBuf> {
        let mut cur = self.data_dir.clone();
        for comp in path.split('\\') {
            if comp.is_empty() || comp == "." || comp == ".." {
                return None;
            }
            let real = self.listing(&cur).get(comp)?.clone();
            cur.push(real);
        }
        cur.is_file().then_some(cur)
    }

    fn listing(&self, dir: &Path) -> DirIndex {
        if let Some(l) = self.dirs.lock().unwrap().get(dir) {
            return l.clone();
        }
        let map: HashMap<String, String> =
            list_dir(dir).into_iter().map(|n| (n.to_ascii_lowercase(), n)).collect();
        let map = Arc::new(map);
        self.dirs.lock().unwrap().insert(dir.to_path_buf(), map.clone());
        map
    }
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn list_dir(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect()
}

/// The base game's archive names from `.ini` file contents, given lowest
/// priority first (the default `.ini`, then the user's): the value of
/// `sResourceArchiveList` then `sResourceArchiveList2`, each taken from the
/// last file that sets it.
pub fn archive_names_from_ini(inis: &[&str]) -> Vec<String> {
    let mut lists: [Option<Vec<String>>; 2] = [None, None];
    for text in inis {
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let slot = match key.trim().to_ascii_lowercase().as_str() {
                "sresourcearchivelist" => 0,
                "sresourcearchivelist2" => 1,
                _ => continue,
            };
            lists[slot] = Some(split_list(value));
        }
    }
    lists.into_iter().flatten().flatten().collect()
}

/// The archive file names to load, lowest priority first: `base` as given,
/// then for each plugin in `plugins` (load order) its `<name>.bsa` and
/// `<name> - Textures.bsa` if `files` (the names in `Data`) has them, in
/// whatever case. Names are returned as they are spelled in `files`.
pub fn archive_order(files: &[String], base: &[String], plugins: &[String]) -> Vec<String> {
    let by_lower: HashMap<String, &String> = files.iter().map(|f| (f.to_ascii_lowercase(), f)).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut add = |name: &str, require_present: bool| {
        let key = name.to_ascii_lowercase();
        let real = by_lower.get(&key).map(|r| r.to_string());
        if (real.is_some() || !require_present) && seen.insert(key) {
            out.push(real.unwrap_or_else(|| name.to_string()));
        }
    };
    for b in base {
        add(b, false);
    }
    for p in plugins {
        let stem = p.rsplit_once('.').map_or(p.as_str(), |(s, _)| s);
        add(&format!("{stem}.bsa"), true);
        add(&format!("{stem} - Textures.bsa"), true);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bsa::tests::{build, temp_dir};

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("Data\\Textures/Foo\\Bar.DDS"), "textures\\foo\\bar.dds");
        assert_eq!(normalize("\\textures\\a.dds"), "textures\\a.dds");
        assert_eq!(normalize("textures\\data\\a.dds"), "textures\\data\\a.dds");
    }

    #[test]
    fn reads_archive_lists_from_inis() {
        let default = "[Archive]\nsResourceArchiveList=A.bsa, B.bsa\nsResourceArchiveList2=C.bsa,D.bsa\n";
        assert_eq!(archive_names_from_ini(&[default]), ["A.bsa", "B.bsa", "C.bsa", "D.bsa"]);
        // A later file replaces one list; the other stays.
        let user = "[Archive]\n  sResourceArchiveList = X.bsa\n";
        assert_eq!(archive_names_from_ini(&[default, user]), ["X.bsa", "C.bsa", "D.bsa"]);
        assert!(archive_names_from_ini(&["[General]\nfoo=bar"]).is_empty());
    }

    #[test]
    fn orders_archives_base_then_plugins() {
        let files: Vec<String> = ["Skyrim - Meshes0.bsa", "Mod One.bsa", "mod one - textures.BSA", "Other.bsa", "Dawnguard.bsa"]
            .map(String::from)
            .to_vec();
        let base = vec!["Skyrim - Meshes0.bsa".to_string(), "Skyrim - Gone.bsa".to_string()];
        let plugins = ["Skyrim.esm", "Dawnguard.esm", "mod one.esp", "NoArchive.esp"].map(String::from).to_vec();
        // Base names are kept even if absent (the resolver skips missing files); plugin ones only if present.
        assert_eq!(
            archive_order(&files, &base, &plugins),
            ["Skyrim - Meshes0.bsa", "Skyrim - Gone.bsa", "Dawnguard.bsa", "Mod One.bsa", "mod one - textures.BSA"]
        );
    }

    fn setup(tag: &str) -> (PathBuf, AssetResolver) {
        let data = temp_dir(tag).join("Data");
        std::fs::create_dir_all(data.join("Textures/Mod")).unwrap();
        std::fs::write(data.join("Textures/Mod/Both.dds"), b"loose").unwrap();
        std::fs::write(data.join("Textures/Mod/OnlyLoose.dds"), b"loose2").unwrap();
        let first = build(&[("textures\\mod", "both.dds", b"first", false), ("textures\\mod", "OnlyFirst.dds", b"f", false)], false, false);
        let second = build(&[("textures\\mod", "BOTH.dds", b"second", true)], false, false);
        std::fs::write(data.join("a.bsa"), first).unwrap();
        std::fs::write(data.join("b.bsa"), second).unwrap();
        std::fs::write(data.join("bad.bsa"), b"definitely not a bsa, but long enough to have a header in it").unwrap();
        let r = AssetResolver::new(&data, ["a.bsa", "b.bsa", "bad.bsa", "missing.bsa"].map(|n| data.join(n)));
        (data, r)
    }

    #[test]
    fn loose_beats_archives_and_later_archive_beats_earlier() {
        let (data, r) = setup("assets_prec");
        assert_eq!(r.archives().len(), 2);
        assert_eq!(r.warnings().len(), 1, "{:?}", r.warnings());
        assert!(r.warnings()[0].contains("bad.bsa"));

        let both = r.resolve("Data/textures/MOD/both.dds").unwrap();
        assert_eq!(both.path, "textures\\mod\\both.dds");
        assert_eq!(both.winner, Source::Loose(data.join("Textures/Mod/Both.dds")));
        assert_eq!(both.shadowed.len(), 2);
        assert_eq!(r.describe(&both.shadowed[0]), "b.bsa"); // later archive first
        assert_eq!(r.describe(&both.shadowed[1]), "a.bsa");
        assert_eq!(r.read(&both.winner).unwrap(), b"loose");
        assert_eq!(r.read(&both.shadowed[0]).unwrap(), b"second");
        assert_eq!(r.describe(&both.winner), "loose: Textures/Mod/Both.dds");

        let only_first = r.resolve("textures/mod/onlyfirst.dds").unwrap();
        assert_eq!(r.describe(&only_first.winner), "a.bsa");
        assert!(only_first.shadowed.is_empty());
        assert_eq!(r.read_asset("textures/mod/onlyloose.dds").unwrap().unwrap(), b"loose2");
        assert!(r.resolve("textures/mod/nothing.dds").is_none());
        assert!(r.read_asset("textures/mod/nothing.dds").unwrap().is_none());
    }

    #[test]
    fn archive_wins_when_nothing_loose_and_paths_cannot_escape_data() {
        let (data, r) = setup("assets_escape");
        std::fs::write(data.parent().unwrap().join("secret.txt"), b"x").unwrap();
        assert!(r.resolve("../secret.txt").is_none());
        assert!(r.resolve("textures/../../secret.txt").is_none());
        assert!(r.resolve("").is_none());
        assert!(r.resolve("textures/mod").is_none()); // a folder isn't an asset
    }

    #[test]
    fn new_loose_files_are_seen_after_clearing_the_cache() {
        let (data, r) = setup("assets_cache");
        assert!(r.resolve("textures/mod/late.dds").is_none());
        std::fs::write(data.join("Textures/Mod/Late.dds"), b"new").unwrap();
        assert!(r.resolve("textures/mod/late.dds").is_none(), "stale listing is expected until cleared");
        r.clear_cache();
        assert!(r.resolve("textures/mod/late.dds").is_some());
    }

    #[test]
    fn for_game_reads_inis_plugins_and_archives() {
        use crate::steam::Edition;
        let root = temp_dir("assets_game");
        let game = root.join("game");
        let data = game.join("Data");
        let docs = root.join("docs/Saves");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(game.join("Skyrim_Default.ini"), "[Archive]\nsResourceArchiveList=Base.bsa\n").unwrap();
        std::fs::write(data.join("Base.bsa"), build(&[("f", "x.txt", b"base", false)], false, false)).unwrap();
        std::fs::write(data.join("Skyrim.esm"), b"").unwrap();
        std::fs::write(data.join("Mod.esp"), b"").unwrap();
        std::fs::write(data.join("Off.esp"), b"").unwrap();
        std::fs::write(data.join("Mod.bsa"), build(&[("f", "x.txt", b"mod", false)], false, false)).unwrap();
        std::fs::write(data.join("Off.bsa"), build(&[("f", "x.txt", b"off", false)], false, false)).unwrap();
        let plugins_txt = root.join("Plugins.txt");
        std::fs::write(&plugins_txt, "*Mod.esp\nOff.esp\n").unwrap();
        let paths = GamePaths {
            data_dir: data,
            plugins_txt,
            ccc_path: game.join("Skyrim.ccc"),
            saves_dir: docs,
            edition: Edition::Steam,
            prefix: None,
        };
        let r = AssetResolver::for_game(&paths).unwrap();
        let names: Vec<_> = r.archives().iter().map(|a| a.path().file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["Base.bsa", "Mod.bsa"], "the disabled plugin's archive must not load");
        let x = r.resolve("f/x.txt").unwrap();
        assert_eq!(r.read(&x.winner).unwrap(), b"mod");
        assert_eq!(x.shadowed.len(), 1);
    }

    /// Read-only sweep of a real install, for checking the readers against
    /// real data. Skipped unless `SKYPLUG_TEST_GAME_DIR` is set (and
    /// `SKYPLUG_TEST_PREFIX` for a Wine prefix):
    /// `SKYPLUG_TEST_GAME_DIR=".../Skyrim Special Edition" cargo test -p skyplug-core real_install -- --nocapture`
    #[test]
    fn real_install() {
        use crate::steam::{DEFAULT_APPID, GameInstall};
        let Some(dir) = std::env::var_os("SKYPLUG_TEST_GAME_DIR") else {
            eprintln!("SKYPLUG_TEST_GAME_DIR not set; skipping");
            return;
        };
        let prefix = std::env::var_os("SKYPLUG_TEST_PREFIX").map(PathBuf::from);
        let install = GameInstall::new(PathBuf::from(dir), prefix).unwrap();
        let paths = GamePaths::new(&install, DEFAULT_APPID);
        let r = paths.asset_resolver().unwrap();
        eprintln!("{} archives opened, {} warnings", r.archives().len(), r.warnings().len());
        for w in r.warnings() {
            eprintln!("  warning: {w}");
        }
        assert!(!r.archives().is_empty());

        let (mut files, mut read, mut dds_ok, mut dds_bad) = (0usize, 0usize, 0usize, 0usize);
        for (n, a) in r.archives().iter().enumerate() {
            files += a.len();
            let small = std::fs::metadata(a.path()).unwrap().len() < 64 << 20;
            for (i, e) in a.entries().iter().enumerate() {
                // Every entry must be found by its own path.
                assert_eq!(a.find(&e.path), Some(i), "{} in {}", e.path, a.path().display());
                if !(small || i % 97 == 0) {
                    continue;
                }
                let bytes = a.read(i).unwrap_or_else(|err| panic!("archive {n}: {err:#}"));
                read += 1;
                if e.path.to_ascii_lowercase().ends_with(".dds") {
                    match crate::dds::Dds::parse(&bytes).and_then(|d| d.decode(d.level_for(64))) {
                        Ok(img) => {
                            assert_eq!(img.pixels.len(), img.width * img.height * 4);
                            dds_ok += 1;
                        }
                        Err(err) => {
                            dds_bad += 1;
                            eprintln!("  dds {} in {}: {err:#}", e.path, a.path().file_name().unwrap().to_string_lossy());
                        }
                    }
                }
            }
        }
        eprintln!("{files} archive entries; read {read}; dds decoded {dds_ok}, failed {dds_bad}");
        assert_eq!(dds_bad, 0);
    }
}
