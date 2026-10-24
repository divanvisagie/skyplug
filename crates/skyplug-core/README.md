# skyplug-core

Library for working with a Skyrim Special Edition install, Steam's or
GOG's, under Proton or Wine on Linux or on Windows itself: finding the game, reading and
changing which plugins are active, and parsing `.ess` save files. It's the
shared core of the
[`skyplug`](https://github.com/divanvisagie/skyplug) command-line tool,
split out so other tools don't have to re-implement the same file formats.

Nothing in here prints. Functions return data or errors, and the caller
decides how to present them.

## Modules

- **`steam`** — locates the install by finding the Steam library that
  actually *owns* the AppID in `libraryfolders.vdf`, rather than probing
  for a folder that might be a stale leftover. Exposes the VDF parser,
  the candidate Steam roots (native, Flatpak, Windows default), and the
  paths for `Plugins.txt`, `Saves` and `My Games`: inside the Proton prefix
  on Linux, in the user's own `AppData\Local` and Documents on Windows
  (see `Layout`). A copy outside Steam can bring its own Wine prefix, and
  `Edition` tells Steam's build from GOG's, whose per-user folders are
  named `Skyrim Special Edition GOG`.
- **`plugins`** — scans `Data` for `.esp`/`.esm`/`.esl` files and reads
  each header's master flag, parses `Plugins.txt` and `Skyrim.ccc`, merges
  them into a per-plugin status with an approximate load order, and
  enables/disables plugins in `Plugins.txt` (writing a `.bak` first).
- **`saves`** — parses `.ess` saves: the full header (character, level,
  location, race, sex, XP, timestamp), the screenshot pixels, and the list
  of plugins active when the save was written. Handles uncompressed and
  LZ4-compressed saves, and can read just the header cheaply.

`GamePaths` ties these together: resolve every path once, then ask it
for the merged plugin status.

## Usage

```toml
[dependencies]
skyplug-core = "0.1"
```

```rust,no_run
use skyplug_core::{DEFAULT_APPID, GamePaths, saves};

fn main() -> anyhow::Result<()> {
    // Find the install that owns Skyrim SE's AppID, and the paths under it.
    let paths = GamePaths::detect(DEFAULT_APPID)?;

    for plugin in paths.plugin_status()? {
        println!("{:?} {} (load order {:?})", plugin.state, plugin.name, plugin.load_order);
    }

    // Group saves by character, newest first.
    let list = saves::list_saves(&paths.saves_dir)?;
    for character in saves::characters(&list.entries) {
        println!("{}: {} saves", character.name, character.save_count);
    }

    // Parse a save you already have in memory (e.g. from a file picker)...
    let bytes = std::fs::read("Save1.ess")?;
    let save = saves::parse_save(&bytes)?;
    println!("{} used {} plugins", save.header.player_name, save.plugins.len());

    // ...or read only the header from disk, skipping the screenshot and body.
    let header = saves::read_header_from_file("Save1.ess".as_ref())?;
    println!("level {} in {}", header.player_level, header.player_location);

    Ok(())
}
```

For a game directory you already know, build the paths with
`GameInstall::from_game_dir(dir)` and `GamePaths::new(&install, appid)`
instead of `detect`. For one outside Steam on Linux, like GOG's under umu,
pass the Wine prefix it runs in (the folder holding `drive_c`):
`GameInstall::new(dir, Some(prefix))`. The edition is detected from the
game directory either way.

## Caveats

- **Errors are `anyhow::Error`**, carrying context about which file
  failed. There's no typed error enum to match on yet.
- **Windows finds Steam at its default location.** Steam roots include
  `C:\Program Files (x86)\Steam`; a Steam installed elsewhere isn't read
  from the registry yet. `GameInstall::from_game_dir` covers it.
- **Load order is approximate.** Creation Club, then masters, then regular
  plugins, each group in `Plugins.txt`/`Skyrim.ccc` order. Plugins' own
  master lists aren't read, so dependency reordering among masters isn't
  modelled.
- **Saves' light-plugin list isn't parsed yet.** SE saves store ESL-flagged
  plugins in a second list after the regular one; `SaveFile::plugins`
  currently only contains the regular list.

## License

BSD-3-Clause
