use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Default Steam AppID for Skyrim Special Edition.
pub const DEFAULT_APPID: &str = "489830";
/// Folder name used for both the install (`steamapps/common/<this>`) and
/// the per-user data under the Proton prefix.
pub const DEFAULT_GAME_FOLDER: &str = "Skyrim Special Edition";

/// A located Skyrim install: the game directory plus the Steam library root
/// it lives under (needed to find the matching compatdata/Proton prefix).
#[derive(Debug, Clone)]
pub struct GameInstall {
    pub game_dir: PathBuf,
    pub library_root: PathBuf,
}

impl GameInstall {
    /// Build an install from an explicit game directory, deriving the
    /// library root from the standard `<library>/steamapps/common/<game>`
    /// layout so the matching compatdata prefix can still be found.
    pub fn from_game_dir(game_dir: impl Into<PathBuf>) -> Result<Self> {
        let game_dir = game_dir.into();
        let library_root = game_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .context("game directory doesn't look like a Steam steamapps/common install")?
            .to_path_buf();
        Ok(GameInstall { game_dir, library_root })
    }

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

    /// The `My Games/Skyrim Special Edition` documents folder inside the
    /// Proton prefix (holds `Saves`, `SkyrimPrefs.ini`, ...).
    pub fn documents_dir(&self, appid: &str) -> PathBuf {
        self.compatdata_dir(appid)
            .join("pfx/drive_c/users/steamuser/Documents/My Games")
            .join(DEFAULT_GAME_FOLDER)
    }

    /// Path to the `Saves` folder (`.ess` files) inside the Proton prefix
    /// for this install.
    pub fn saves_dir(&self, appid: &str) -> PathBuf {
        self.documents_dir(appid).join("Saves")
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")
}

/// Steam install roots to try, in order, covering the common native and
/// Flatpak layouts on Linux, plus the default Windows location.
pub fn candidate_steam_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".steam/root"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        PathBuf::from("C:/Program Files (x86)/Steam"),
    ]
}

fn extract_quoted(s: &str) -> Option<&str> {
    let s = s.trim();
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some(&s[..end])
}

/// A Steam library folder, with the set of appids Steam considers installed
/// under it (per `libraryfolders.vdf`'s `"apps"` block for that library).
#[derive(Debug, Clone)]
pub struct SteamLibrary {
    pub path: PathBuf,
    pub apps: Vec<String>,
}

/// Parse a `libraryfolders.vdf` file into each library's path and owned
/// appids. This is a depth-tracking scan rather than a full VDF parser —
/// enough for the file's fixed shape:
///
/// ```text
/// "libraryfolders"
/// {
///     "0"
///     {
///         "path"  "..."
///         "apps"
///         {
///             "<appid>"  "<size>"
///         }
///     }
/// }
/// ```
///
/// Appid ownership matters because a stale/leftover install directory can
/// exist under a library Steam no longer considers this app installed in
/// (e.g. after moving the install to a different library without deleting
/// files Steam didn't track, like loose mod files); relying on directory
/// existence alone can then pick the wrong install.
pub fn parse_libraries(vdf: &str) -> Vec<SteamLibrary> {
    let mut libraries = Vec::new();
    let mut depth: i32 = 0;
    let mut library_depth: Option<i32> = None;
    let mut apps_depth: Option<i32> = None;
    let mut current_path: Option<PathBuf> = None;
    let mut current_apps: Vec<String> = Vec::new();
    let mut pending_key: Option<String> = None;

    for raw_line in vdf.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if line == "{" {
            depth += 1;
            if let Some(key) = pending_key.take() {
                if library_depth.is_none() && !key.is_empty() && key.chars().all(|c| c.is_ascii_digit()) {
                    library_depth = Some(depth);
                    current_path = None;
                    current_apps = Vec::new();
                } else if library_depth == Some(depth - 1) && key == "apps" {
                    apps_depth = Some(depth);
                }
            }
            continue;
        }

        if line == "}" {
            if apps_depth == Some(depth) {
                apps_depth = None;
            } else if library_depth == Some(depth) {
                if let Some(path) = current_path.take() {
                    libraries.push(SteamLibrary { path, apps: std::mem::take(&mut current_apps) });
                }
                library_depth = None;
            }
            depth -= 1;
            continue;
        }

        let Some(rest) = line.strip_prefix('"') else { continue };
        let Some(key_end) = rest.find('"') else { continue };
        let key = &rest[..key_end];
        let after = rest[key_end + 1..].trim();

        if after.is_empty() {
            // Bare `"key"` line — its block opens on the next line.
            pending_key = Some(key.to_string());
            continue;
        }

        if apps_depth == Some(depth) {
            current_apps.push(key.to_string());
        } else if library_depth == Some(depth) && key == "path" {
            if let Some(value) = extract_quoted(after) {
                current_path = Some(PathBuf::from(value.replace("\\\\", "/")));
            }
        }
    }

    libraries
}

/// Read and parse `<steam_root>/steamapps/libraryfolders.vdf`.
pub fn read_libraries(steam_root: &Path) -> Result<Vec<SteamLibrary>> {
    let vdf_path = steam_root.join("steamapps/libraryfolders.vdf");
    let contents =
        std::fs::read_to_string(&vdf_path).with_context(|| format!("reading {}", vdf_path.display()))?;
    Ok(parse_libraries(&contents))
}

/// Find the Steam library root that owns `appid`, per `libraryfolders.vdf`
/// under any of the [`candidate_steam_roots`].
pub fn find_owning_library(appid: &str) -> Result<PathBuf> {
    let home = home_dir()?;
    let mut any_steam_root = false;

    for steam_root in candidate_steam_roots(&home) {
        if !steam_root.is_dir() {
            continue;
        }
        any_steam_root = true;

        let Ok(libraries) = read_libraries(&steam_root) else { continue };
        if let Some(library) = libraries.into_iter().find(|l| l.apps.iter().any(|a| a == appid)) {
            return Ok(library.path);
        }
    }

    if !any_steam_root {
        bail!("could not find a Steam installation under {}", home.display());
    }
    bail!(
        "no Steam library reports owning appid {appid} (looked for a \"{}\" install)",
        DEFAULT_GAME_FOLDER
    )
}

/// Locate the Skyrim Special Edition install by finding the Steam library
/// that actually owns `appid`, per `libraryfolders.vdf` — not just any
/// library with a `steamapps/common/Skyrim Special Edition` folder lying
/// around, since a stale one from a prior install location can persist.
pub fn find_game_install(appid: &str) -> Result<GameInstall> {
    let library_root = find_owning_library(appid)?;
    let game_dir = library_root.join("steamapps/common").join(DEFAULT_GAME_FOLDER);
    Ok(GameInstall { game_dir, library_root })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_library_paths_and_apps() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/me/.local/share/Steam"
		"apps"
		{
			"228980"		"123"
		}
	}
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
		"label"		""
		"apps"
		{
			"489830"		"456"
			"611670"		"789"
		}
	}
}
"#;
        let libs = parse_libraries(vdf);
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[0].path, PathBuf::from("/home/me/.local/share/Steam"));
        assert_eq!(libs[0].apps, vec!["228980"]);
        assert_eq!(libs[1].path, PathBuf::from("/mnt/games/SteamLibrary"));
        assert_eq!(libs[1].apps, vec!["489830", "611670"]);
    }

    #[test]
    fn from_game_dir_derives_library_root() {
        let install = GameInstall::from_game_dir("/mnt/lib/steamapps/common/Skyrim Special Edition").unwrap();
        assert_eq!(install.library_root, PathBuf::from("/mnt/lib"));
        assert!(install.plugins_txt_path("489830").starts_with("/mnt/lib/steamapps/compatdata/489830/pfx"));
    }
}
