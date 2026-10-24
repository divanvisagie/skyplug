use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Default Steam AppID for Skyrim Special Edition.
pub const DEFAULT_APPID: &str = "489830";
/// Folder name of Steam's install (`steamapps/common/<this>`), and of the
/// per-user folders Steam's build keeps its files in.
pub const DEFAULT_GAME_FOLDER: &str = "Skyrim Special Edition";

/// Which store's build of the game this is. Each names its per-user
/// folders (`AppData/Local/<folder>`, `My Games/<folder>`) differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Steam,
    Gog,
}

impl Edition {
    /// GOG's installers leave `goggame-<id>.info` files in the game
    /// directory; anything without one is taken to be Steam's build.
    pub fn detect(game_dir: &Path) -> Self {
        let is_gog = std::fs::read_dir(game_dir).into_iter().flatten().flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("goggame-") && name.ends_with(".info")
        });
        if is_gog { Edition::Gog } else { Edition::Steam }
    }

    /// The per-user folder name under `AppData/Local` and `My Games`.
    pub fn user_folder(self) -> &'static str {
        match self {
            Edition::Steam => DEFAULT_GAME_FOLDER,
            Edition::Gog => "Skyrim Special Edition GOG",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Edition::Steam => "Steam",
            Edition::Gog => "GOG",
        }
    }
}

/// A located Skyrim install: the game directory, and where to find the
/// per-user files that go with it (a Steam library's compatdata prefix, or
/// a Wine prefix given explicitly).
#[derive(Debug, Clone)]
pub struct GameInstall {
    pub game_dir: PathBuf,
    /// The Steam library the game is in, when it's under
    /// `<library>/steamapps/common/`; its compatdata holds the Proton prefix.
    pub library_root: Option<PathBuf>,
    /// The Wine prefix (the folder holding `drive_c`) the game runs in,
    /// used instead of Steam's compatdata prefix. For a copy run outside
    /// Steam, like GOG's under umu or Wine.
    pub prefix: Option<PathBuf>,
    pub edition: Edition,
}

impl GameInstall {
    /// [`Self::new`] without a prefix: the game must be in a Steam library
    /// (`<library>/steamapps/common/<game>`) so its compatdata prefix can
    /// be found, except on Windows, where the user files don't live in one.
    pub fn from_game_dir(game_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::new(game_dir, None)
    }

    /// The install in `game_dir`, with its user files in the Wine `prefix`
    /// when one is given. Without one, on Linux, the game must be in a
    /// Steam library so the Proton prefix can be found. The [`Edition`] is
    /// detected from the game directory.
    pub fn new(game_dir: impl Into<PathBuf>, prefix: Option<PathBuf>) -> Result<Self> {
        let game_dir = game_dir.into();
        let library_root = steam_library_of(&game_dir);
        if library_root.is_none() && prefix.is_none() && !cfg!(windows) {
            bail!(
                "{} isn't in a Steam library (steamapps/common), so there's no Proton prefix to find; give its Wine prefix",
                game_dir.display()
            );
        }
        let edition = Edition::detect(&game_dir);
        Ok(GameInstall { game_dir, library_root, prefix, edition })
    }

    pub fn data_dir(&self) -> PathBuf {
        self.game_dir.join("Data")
    }

    pub fn ccc_path(&self) -> PathBuf {
        self.game_dir.join("Skyrim.ccc")
    }

    pub fn compatdata_dir(&self, appid: &str) -> Option<PathBuf> {
        self.library_root.as_ref().map(|root| root.join("steamapps").join("compatdata").join(appid))
    }

    /// The Wine prefix holding the user files: the one given, else Steam's
    /// Proton prefix for `appid`. `None` on Windows without one given,
    /// where the user's own folders are used.
    pub fn wine_prefix(&self, appid: &str) -> Option<PathBuf> {
        match self.layout() {
            Layout::Proton => self.proton_prefix(appid),
            Layout::Native { .. } => None,
        }
    }

    fn proton_prefix(&self, appid: &str) -> Option<PathBuf> {
        self.prefix.clone().or_else(|| self.compatdata_dir(appid).map(|dir| dir.join("pfx")))
    }

    /// Path to Plugins.txt: in the Wine/Proton prefix on Linux, in the
    /// user's `AppData\Local` on Windows.
    pub fn plugins_txt_path(&self, appid: &str) -> PathBuf {
        self.user_folders(appid, &self.layout()).plugins_txt()
    }

    /// The `My Games/Skyrim Special Edition` documents folder (holds
    /// `Saves`, `SkyrimPrefs.ini`, ...; GOG's build adds ` GOG` to the
    /// name): in the Wine/Proton prefix on Linux, in the user's Documents
    /// on Windows.
    pub fn documents_dir(&self, appid: &str) -> PathBuf {
        self.user_folders(appid, &self.layout()).documents_dir()
    }

    /// Path to the `Saves` folder (`.ess` files); see [`Self::documents_dir`].
    pub fn saves_dir(&self, appid: &str) -> PathBuf {
        self.documents_dir(appid).join("Saves")
    }

    /// A prefix given explicitly is always used; otherwise the platform's.
    fn layout(&self) -> Layout {
        if self.prefix.is_some() { Layout::Proton } else { Layout::current() }
    }

    fn user_folders(&self, appid: &str, layout: &Layout) -> UserFolders {
        let game_folder = self.edition.user_folder();
        match layout {
            Layout::Proton => {
                let user = wine_user_dir(&self.proton_prefix(appid).unwrap_or_default());
                UserFolders {
                    local_app_data: user.join("AppData/Local"),
                    documents: user.join("Documents"),
                    game_folder,
                }
            }
            Layout::Native { local_app_data, documents } => UserFolders {
                local_app_data: local_app_data.clone(),
                documents: documents.clone(),
                game_folder,
            },
        }
    }
}

/// The library root of a game in `<library>/steamapps/common/<game>`.
fn steam_library_of(game_dir: &Path) -> Option<PathBuf> {
    let common = game_dir.parent()?;
    let steamapps = common.parent()?;
    let is_steam = common.file_name()? == "common" && steamapps.file_name()?.eq_ignore_ascii_case("steamapps");
    is_steam.then(|| steamapps.parent().map(Path::to_path_buf)).flatten()
}

/// The Windows user folder inside a Wine prefix. Proton and umu always
/// run as `steamuser`; plain Wine uses the Unix user's name.
fn wine_user_dir(prefix: &Path) -> PathBuf {
    let users = prefix.join("drive_c/users");
    let steamuser = users.join("steamuser");
    if steamuser.is_dir() {
        return steamuser;
    }
    match std::env::var_os("USER") {
        Some(user) if users.join(&user).is_dir() => users.join(user),
        _ => steamuser,
    }
}

/// Where the game keeps per-user files: Plugins.txt under `AppData\Local`,
/// saves and settings under `Documents\My Games`.
#[derive(Debug, Clone)]
pub enum Layout {
    /// Linux: the Windows user folders inside a Wine prefix, Steam's
    /// Proton one for the install unless another is given.
    Proton,
    /// Windows itself: the signed-in user's own folders.
    Native { local_app_data: PathBuf, documents: PathBuf },
}

impl Layout {
    /// Native on Windows, Proton everywhere else. On Windows the folders
    /// come from the shell's known folders, so a Documents folder moved to
    /// OneDrive is still found.
    pub fn current() -> Self {
        if cfg!(windows) {
            let home = home_dir().unwrap_or_default();
            Layout::Native {
                local_app_data: dirs::data_local_dir().unwrap_or_else(|| home.join("AppData/Local")),
                documents: dirs::document_dir().unwrap_or_else(|| home.join("Documents")),
            }
        } else {
            Layout::Proton
        }
    }
}

struct UserFolders {
    local_app_data: PathBuf,
    documents: PathBuf,
    game_folder: &'static str,
}

impl UserFolders {
    fn plugins_txt(&self) -> PathBuf {
        self.local_app_data.join(self.game_folder).join("Plugins.txt")
    }

    fn documents_dir(&self) -> PathBuf {
        self.documents.join("My Games").join(self.game_folder)
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
    Ok(GameInstall { game_dir, library_root: Some(library_root), prefix: None, edition: Edition::Steam })
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
    fn user_files_live_in_the_proton_prefix_or_the_windows_user_folders() {
        let install = GameInstall::from_game_dir("/mnt/lib/steamapps/common/Skyrim Special Edition").unwrap();

        let proton = install.user_folders("489830", &Layout::Proton);
        let prefix = PathBuf::from("/mnt/lib/steamapps/compatdata/489830/pfx/drive_c/users/steamuser");
        assert_eq!(proton.plugins_txt(), prefix.join("AppData/Local/Skyrim Special Edition/Plugins.txt"));
        assert_eq!(proton.documents_dir(), prefix.join("Documents/My Games/Skyrim Special Edition"));

        let native = Layout::Native {
            local_app_data: PathBuf::from("C:/Users/me/AppData/Local"),
            documents: PathBuf::from("C:/Users/me/OneDrive/Documents"),
        };
        let native = install.user_folders("489830", &native);
        assert_eq!(native.plugins_txt(), PathBuf::from("C:/Users/me/AppData/Local/Skyrim Special Edition/Plugins.txt"));
        assert_eq!(native.documents_dir(), PathBuf::from("C:/Users/me/OneDrive/Documents/My Games/Skyrim Special Edition"));
    }

    #[test]
    fn from_game_dir_derives_library_root() {
        let install = GameInstall::from_game_dir("/mnt/lib/steamapps/common/Skyrim Special Edition").unwrap();
        assert_eq!(install.library_root, Some(PathBuf::from("/mnt/lib")));
        assert!(install.plugins_txt_path("489830").starts_with("/mnt/lib/steamapps/compatdata/489830/pfx"));
    }

    #[cfg(not(windows))]
    #[test]
    fn a_game_outside_steam_needs_a_prefix() {
        assert!(GameInstall::from_game_dir("/games/gog/Skyrim Special Edition").is_err());
    }

    /// GOG's build, run under umu: its user files are in the given prefix,
    /// in folders named `Skyrim Special Edition GOG`.
    #[test]
    fn a_gog_install_uses_the_given_prefix_and_its_own_folders() {
        let root = std::env::temp_dir().join(format!("skyplug_gog_test_{}", std::process::id()));
        let game = root.join("Skyrim Special Edition");
        let prefix = root.join("prefix");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::create_dir_all(prefix.join("drive_c/users/steamuser")).unwrap();
        std::fs::write(game.join("goggame-1711230643.info"), "{}").unwrap();

        let install = GameInstall::new(&game, Some(prefix.clone())).unwrap();
        let user = prefix.join("drive_c/users/steamuser");
        assert_eq!(install.edition, Edition::Gog);
        assert_eq!(install.library_root, None);
        assert_eq!(install.wine_prefix(DEFAULT_APPID), Some(prefix.clone()));
        assert_eq!(install.plugins_txt_path(DEFAULT_APPID), user.join("AppData/Local/Skyrim Special Edition GOG/Plugins.txt"));
        assert_eq!(install.saves_dir(DEFAULT_APPID), user.join("Documents/My Games/Skyrim Special Edition GOG/Saves"));

        std::fs::remove_dir_all(&root).unwrap();
    }
}
