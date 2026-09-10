use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Default Steam AppID for Skyrim Special Edition.
pub const DEFAULT_APPID: &str = "489830";
const DEFAULT_GAME_FOLDER: &str = "Skyrim Special Edition";

/// A located Skyrim install: the game directory plus the Steam library root
/// it lives under (needed to find the matching compatdata/Proton prefix).
pub struct GameInstall {
    pub game_dir: PathBuf,
    pub library_root: PathBuf,
}

impl GameInstall {
    pub fn data_dir(&self) -> PathBuf {
        self.game_dir.join("Data")
    }

    pub fn ccc_path(&self) -> PathBuf {
        self.game_dir.join("Skyrim.ccc")
    }

    pub fn compatdata_dir(&self, appid: &str) -> PathBuf {
        self.library_root.join("steamapps").join("compatdata").join(appid)
    }

    /// Path to Plugins.txt inside the Proton prefix for this install.
    pub fn plugins_txt_path(&self, appid: &str) -> PathBuf {
        self.compatdata_dir(appid)
            .join("pfx/drive_c/users/steamuser/AppData/Local")
            .join(DEFAULT_GAME_FOLDER)
            .join("Plugins.txt")
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable is not set")
}

/// Steam install roots to try, in order, covering the common native and
/// Flatpak layouts on Linux.
fn candidate_steam_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".steam/root"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ]
}

/// Extract every `"path"  "..."` value from a `libraryfolders.vdf` file.
/// This is a small line-oriented scan rather than a full VDF parser, which
/// is all that's needed for this key.
fn parse_library_paths(vdf: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in vdf.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"path\"") {
            let rest = rest.trim();
            if let Some(value) = extract_quoted(rest) {
                paths.push(PathBuf::from(value.replace("\\\\", "/")));
            }
        }
    }
    paths
}

fn extract_quoted(s: &str) -> Option<&str> {
    let s = s.trim();
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some(&s[..end])
}

/// All Steam library roots (folders containing a `steamapps` dir) reachable
/// from any discovered Steam install.
pub fn library_roots() -> Result<Vec<PathBuf>> {
    let home = home_dir()?;
    let mut roots = Vec::new();

    for steam_root in candidate_steam_roots(&home) {
        if !steam_root.is_dir() {
            continue;
        }
        roots.push(steam_root.clone());

        let vdf_path = steam_root.join("steamapps/libraryfolders.vdf");
        if let Ok(contents) = std::fs::read_to_string(&vdf_path) {
            roots.extend(parse_library_paths(&contents));
        }
    }

    roots.sort();
    roots.dedup();

    if roots.is_empty() {
        bail!("could not find a Steam installation under {}", home.display());
    }

    Ok(roots)
}

/// Locate the Skyrim Special Edition install by scanning every known Steam
/// library for `steamapps/common/Skyrim Special Edition`.
pub fn find_game_install() -> Result<GameInstall> {
    for library_root in library_roots()? {
        let game_dir = library_root
            .join("steamapps/common")
            .join(DEFAULT_GAME_FOLDER);
        if game_dir.join("SkyrimSE.exe").exists() || game_dir.join("Data").is_dir() {
            return Ok(GameInstall { game_dir, library_root });
        }
    }
    bail!(
        "could not find a \"{}\" install under any Steam library",
        DEFAULT_GAME_FOLDER
    )
}
