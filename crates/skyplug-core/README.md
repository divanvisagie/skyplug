# skyplug-core

Library for working with a Skyrim Special Edition install, Steam's or
GOG's, under Proton or Wine on Linux or on Windows itself: finding the game, reading and
changing which plugins are active, parsing `.ess` save files, and reading
the game's `.bsa` archives, `.dds` textures and `.nif` models. It's the
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

- **`bsa`** — reads Skyrim SE archives (version 105): the index on open, a
  file's bytes on request (LZ4 handled), lookups by path in any case.
  Read-only, and it refuses other versions and corrupt indexes instead of
  guessing.
- **`dds`** — decodes `.dds` textures to RGBA8, one mip level at a time:
  BC1 to BC5, BC7 and uncompressed 8/24/32-bit, with a legacy or DX10 header.
- **`nif`** — reads Skyrim SE models into meshes: positions, normals, UVs,
  vertex colours, triangles, texture paths, alpha and double-sided flags,
  with node transforms applied. Handles skinned shapes (geometry in
  `NiSkinPartition`), the older `NiTriShape`, and switch nodes. Skyrim LE
  models come back as a `LegacyFormat` error; unreadable blocks become
  warnings and the rest still loads. A skinned shape keeps its bones, bind
  transforms and per-vertex weights, and `Mesh::pose(&Skeleton)` moves it the
  way the game does, by the weighted bones of the game's skeleton
  (`Skeleton::from_nif` of `skeleton_female.nif`, or `skeleton.nif` for men): a model made on a skeleton
  that differs from the game's (a few mods' are shifted by a couple of units)
  comes out where it does in game. No animation, collision, particles or face
  meshes yet.
- **`records`** — reads plugin files: the header (flags, masters) and the
  records of one top-level group at a time, with zlib-compressed bodies,
  subrecords and FormIDs resolved to the plugin that defines them. Read-only.
- **`armour`** — `ArmourCatalog`: the armours of a load order and the models
  they're worn as. An armour only lists "armatures", and the last plugin in the
  load order to change an armature decides its model, so a refit like UNP
  Vanilla Outfits shows through. Names come from the plugin or, for the
  localized base plugins, from their string tables. Each armour has a model set
  for women and one for men (`Armour::female`, `Armour::male`, or
  `Armour::models_for(BodySex)`), for the human races; an armature with a model
  for only one sex gives it to both. `ArmourCatalog::armours_for` finds the
  armours for a list of record keys. `Armour::visibility_problems(sex, …)` reads each model and says
  which shapes the game won't draw: it only draws a skin partition whose slot
  is among the slots of the armature wearing it (`partition_problems`), so an
  armour moved to another slot in its plugin without its model is invisible.
  Armatures' texture swaps (`MO3S`/`MO2S` with the `TXST` texture sets they
  name) come with each model as `ModelSet::alt_textures`, and
  `apply_alternate_textures` puts them on a parsed model: the Tartan and Dark
  variants of an outfit are the same mesh with another set.
- **`render`** — a small software renderer: a `Model` is meshes plus their
  diffuse textures resolved through an `AssetResolver` (with a report of
  where every texture came from, or that it's missing), and `render` draws it
  from a `Camera` into RGBA with a z-buffer, backface culling, alpha test and
  blend, and supersampling. `Model::pose` poses its skinned meshes on a
  skeleton (`render::load_skeleton_for` reads the game's, per `BodySex`) and
  reports what moved. `BodySex::body` names the installed body, hands and feet
  to draw under armour.
  For checking shape, mapping and colour, not the game's shading.
- **`outfit`** — what a character wears, read from a Special Edition save:
  the worn items in the player's inventory (the `Worn` and `WornLeft` extra
  data) as plugin and FormID, with items made in game counted rather than
  listed. `ArmourCatalog::armours_for` turns them into armours. The inventory
  is found by scanning the player's change form for the block that parses end
  to end, so a save with an extra data type this doesn't know fails instead of
  showing a wrong outfit.
- **`assets`** — `AssetResolver` finds an asset the way the game does, and
  says what it hides: loose files in `Data` first, then archives in load
  order (the base game's from `Skyrim_Default.ini`, `Skyrim.ini` and
  `SkyrimCustom.ini`, then each enabled plugin's `<name>.bsa` and
  `<name> - Textures.bsa`), the later one winning.

`GamePaths` ties these together: resolve every path once, then ask it
for the merged plugin status.

## Usage

```toml
[dependencies]
skyplug-core = "0.3"
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

    // Which file does the game use for a texture, and what does it hide?
    let assets = paths.asset_resolver()?;
    if let Some(found) = assets.resolve("textures/armor/iron/ironarmor.dds") {
        println!("uses {}", assets.describe(&found.winner));
        for hidden in &found.shadowed {
            println!("  hides {}", assets.describe(hidden));
        }
        let texture = assets.read(&found.winner)?;
        let dds = skyplug_core::dds::Dds::parse(&texture)?;
        let small = dds.decode(dds.level_for(256))?; // RGBA8, about 256 px
        println!("{}x{}", small.width, small.height);
    }

    // Read a model and the textures it names.
    if let Some(model) = assets.read_asset("meshes/armor/iron/f/cuirass_1.nif")? {
        for mesh in skyplug_core::nif::parse(&model)?.meshes {
            println!("{}: {} triangles, diffuse {:?}", mesh.name, mesh.triangles.len(), mesh.texture(0));
        }
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

## Testing against a real install

The unit tests build their own archives and textures. To check the readers
against a real install (read only), point a test at it:

```sh
SKYPLUG_TEST_GAME_DIR="/path/to/Skyrim Special Edition" \
SKYPLUG_TEST_PREFIX=/path/to/wine/prefix \
cargo test -p skyplug-core --release real_install -- --nocapture
```

It opens every archive, looks every entry up by its own path, reads and
decompresses a sample, and decodes the textures in it. `real_install_meshes`
does the same for models: it parses every `.nif`, loose and in archives, and
prints how many parsed, were LE, warned, and which block types are unhandled.

`real_install_renders` renders a sample of every model with its textures, and
`real_install_armours` reads every enabled plugin's armours and checks that each
model they name exists.
`skyplug preview` (in the `skyplug` crate) is the command-line front end to
`render`. Two examples help when looking at one file: `cargo run --example resolve --
<game dir> [--prefix <prefix>] <asset path> [--out <file>]` shows where one
asset comes from, and `cargo run --example nifinfo -- <file.nif> [--obj
<out.obj>]` summarises a model and can write it as an OBJ.
