# skyplug

Command-line tool for inspecting and toggling Skyrim Special Edition's
`Plugins.txt` load order on Linux/Steam, without a mod manager.

It scans the game's `Data` directory for `.esp`/`.esm`/`.esl` files,
cross-references `Plugins.txt` and `Skyrim.ccc`, and reports what's
actually active. Only the base game/DLC masters (`Skyrim.esm`,
`Update.esm`, `Dawnguard.esm`, `HearthFires.esm`, `Dragonborn.esm`) and
Creation Club content listed in `Skyrim.ccc` load regardless of
`Plugins.txt`; every other plugin — including mod `.esm`s and
master-flagged `.esp`s like USSEP — needs `*` there to load. The header's
master flag is still read, but only to place masters ahead of regular
plugins in the load order.

It can also read `.ess` save files directly, to answer "which character is
this," "what saves does this character have," and "which mods were
actually active when this save was made" — handy for figuring out what a
save depends on before pruning mods.

## Usage

```sh
skyplug list              # show every plugin and its state
skyplug enable <plugin>   # add/set the `*` prefix in Plugins.txt
skyplug disable <plugin>  # remove the `*` prefix in Plugins.txt
skyplug edit              # interactive TUI: toggle plugins, browse saves
skyplug paths             # print the resolved Data/Plugins.txt/Skyrim.ccc/Saves paths

skyplug characters                # list every character across all saves
skyplug saves <character>         # list a character's saves (name or substring)
skyplug save-plugins <save>       # list the plugins active in a specific save

skyplug man                       # print the skyplug(1) man page
skyplug man --install             # install it next to the binary (see below)
```

### `edit` (TUI)

Two tabs, switched with `tab`: **Plugins** and **Saves**.

Everywhere:

- `↑`/`↓` or `j`/`k` — move selection; `gg`/`G` — jump to top/bottom
- `tab` — switch between Plugins and Saves
- `s` — save plugin changes and quit
- `q`/`esc` — quit; if there are unsaved changes, prompts once more before discarding

Plugins tab:

- `space`/`enter` — toggle the selected plugin
- `/` — filter by substring (live, case-insensitive); `↑`/`↓` still move while typing, `enter` keeps the filter, `esc` cancels back to what it was
- `o` — cycle sort: load order, name, type, enabled

Changes are only written to `Plugins.txt` on `s`; toggles made while browsing
are kept in memory until then, shown with a trailing `*`. Base game/DLC masters,
Creation Club content, and missing plugins can't be toggled (toggling them would have no effect, or
nothing to toggle) — selecting them shows why in the status bar instead.

Saves tab — drill down from characters, to a character's saves, to the
plugins a save was made with:

- `enter`/`l`/`→` — open the selected character or save
- `esc`/`h`/`←`/`backspace` — go back up a level (`esc` at the top quits)

Each of a save's plugins is marked against your current setup, including
toggles you haven't saved yet: `[x]` active, `[ ]` disabled, `[!]` missing
from Data, `[M]`/`[CC]` always loaded. A summary line counts how many are
missing or disabled, so you can flip to the Plugins tab, enable what the
save needs, and see it resolve before saving.

`<plugin>` can be the exact filename, a different case, or just the name
without its extension (e.g. `skyplug enable "paarthurnax dilemma"` matches
`Paarthurnax Dilemma.esp`).

`list` output:

```
[x] SomeMod.esp     enabled via Plugins.txt
[ ] OtherMod.esp     present in Data but not active
[M] Skyrim.esm       base game/DLC master — always loaded, Plugins.txt is irrelevant
[CC] ccBGSSSE001-Fish.esm   Creation Club content — always loaded
[!] Missing.esp      listed in Plugins.txt but no longer in Data
```

Game install and Proton prefix are auto-detected across your Steam
libraries, by matching which library actually *owns* the AppID (per
`libraryfolders.vdf`) rather than just probing for a folder that happens to
exist — a stale install left behind in another library won't be picked by
mistake. Override with `--game-dir` and `--appid` if needed (e.g. for
Skyrim VR: `--appid 611670`).

A `Plugins.txt.bak` is written next to `Plugins.txt` before every change.

### `characters` / `saves` / `save-plugins`

```sh
$ skyplug characters
Tessta (BretonRace) — 4 saves, latest: level 1 at Whiterun on 000.09.08, 2026-09-13 14:05:31

$ skyplug saves tessta
#3    level 1   Whiterun                       in-game day 000.09.08  2026-09-13 14:05:31  Quicksave0_..._1_1.ess

$ skyplug save-plugins Quicksave0_..._1_1
Quicksave0_..._1_1.ess — 17 plugin(s):
  Skyrim.esm                                   [M]  native (base game/DLC)
  ccasvsse001-almsivi.esm                      [CC] creation club
  unofficial skyrim special edition patch.esp  [OK] installed
  SomeRemovedMod.esp                           [!]  missing from Data
```

`saves <character>` and `save-plugins <save>` both accept an exact match,
a case-insensitive match, or (for saves) an unambiguous substring —
resolution fails with the list of candidates if a query is ambiguous.

`save-plugins` cross-references each plugin the save recorded as active
against what's actually in `Data/` right now — useful for seeing which of
an old save's mods you'd need to reinstall before loading it again.

Save parsing reads the `.ess` header directly (magic string, header
block, and — for `save-plugins` — the LZ4-compressed body) rather than
shelling out to anything; unsupported/corrupt save compression types are
reported as an error rather than silently skipped.

## Build

```sh
cargo build --release   # binary at target/release/skyplug
```

`cargo install` only installs the binary, so the man page is built into it
instead. `skyplug man --install` writes it to `../share/man/man1/` relative
to the binary, i.e. `~/.cargo/share/man/man1/skyplug.1`; man-db searches
that automatically for anything in `~/.cargo/bin` on your `PATH`, so
`man skyplug` works with no `MANPATH` changes. The page's source is
[`crates/skyplug/man/skyplug.1`](crates/skyplug/man/skyplug.1), written by
hand in mdoc; a test checks it mentions every subcommand and flag.

## Library (`skyplug-core`)

The install discovery, `Plugins.txt` handling and save parsing live in a
separate library crate, [`crates/skyplug-core`](crates/skyplug-core), so
other tools can reuse them; see its README for the API. The CLI and TUI in
`crates/skyplug` are built on top of it.
