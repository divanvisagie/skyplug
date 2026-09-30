#![doc = include_str!("../README.md")]

use std::path::PathBuf;

use anyhow::Result;

pub mod plugins;
pub mod saves;
pub mod steam;

pub use steam::{DEFAULT_APPID, GameInstall};

/// Every on-disk location the rest of the crate needs, resolved once from
/// a [`GameInstall`].
#[derive(Debug, Clone)]
pub struct GamePaths {
    pub data_dir: PathBuf,
    pub plugins_txt: PathBuf,
    pub ccc_path: PathBuf,
    pub saves_dir: PathBuf,
}

impl GamePaths {
    pub fn new(install: &GameInstall, appid: &str) -> Self {
        GamePaths {
            data_dir: install.data_dir(),
            plugins_txt: install.plugins_txt_path(appid),
            ccc_path: install.ccc_path(),
            saves_dir: install.saves_dir(appid),
        }
    }

    /// Auto-detect the install owning `appid` and resolve its paths.
    pub fn detect(appid: &str) -> Result<Self> {
        Ok(Self::new(&steam::find_game_install(appid)?, appid))
    }

    /// Scan Data, read Plugins.txt and Skyrim.ccc, and merge them into the
    /// per-plugin status view (see [`plugins::build_status`]).
    pub fn plugin_status(&self) -> Result<Vec<plugins::PluginStatus>> {
        let data_plugins = plugins::scan_data_plugins(&self.data_dir)?;
        let plugins_txt = plugins::parse_plugins_txt(&self.plugins_txt)?;
        let ccc = plugins::parse_ccc(&self.ccc_path)?;
        Ok(plugins::build_status(&data_plugins, &plugins_txt, &ccc))
    }
}
