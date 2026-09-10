# skyplug

Command-line tool for inspecting and toggling Skyrim Special Edition's
`Plugins.txt` load order on Linux/Steam, without a mod manager.

It scans the game's `Data` directory for `.esp`/`.esm`/`.esl` files, reads
each plugin's header to see if it's a master or light-master (which the
engine always loads, regardless of `Plugins.txt`), cross-references
`Plugins.txt` and `Skyrim.ccc`, and reports what's actually active.

## Usage

```sh
skyplug list              # show every plugin and its state
skyplug enable <plugin>   # add/set the `*` prefix in Plugins.txt
skyplug disable <plugin>  # remove the `*` prefix in Plugins.txt
skyplug paths             # print the resolved Data/Plugins.txt/Skyrim.ccc paths
```

`<plugin>` can be the exact filename, a different case, or just the name
without its extension (e.g. `skyplug enable "paarthurnax dilemma"` matches
`Paarthurnax Dilemma.esp`).

`list` output:

```
[x] SomeMod.esp     enabled via Plugins.txt
[ ] OtherMod.esp     present in Data but not active
[M] Skyrim.esm       master/light-master — always loaded, Plugins.txt is irrelevant
[CC] ccBGSSSE001-Fish.esm   Creation Club content — always loaded
[!] Missing.esp      listed in Plugins.txt but no longer in Data
```

Game install and Proton prefix are auto-detected across your Steam
libraries (parses `libraryfolders.vdf`); override with `--game-dir` and
`--appid` if needed (e.g. for Skyrim VR: `--appid 611670`).

A `Plugins.txt.bak` is written next to `Plugins.txt` before every change.

## Build

```sh
cargo build --release
```
