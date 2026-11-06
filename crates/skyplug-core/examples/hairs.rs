//! List the hairs of a load order, and optionally write a patch hiding all but some.
//!
//!   cargo run --example hairs -- <Data dir> <Plugins.txt> [<out patch> <keep substring>...]
//!
//! With an output path, every pickable hair whose plugin or name contains none of the
//! substrings is hidden. For trying the patch on a real install without touching it.
use std::path::Path;

use skyplug_core::hair::HairCatalog;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [data, plugins_txt, rest @ ..] = &args[..] else { anyhow::bail!("usage: hairs <Data dir> <Plugins.txt> [<out patch> <keep>...]") };
    let order: Vec<String> = std::fs::read_to_string(plugins_txt)?
        .lines()
        .filter_map(|l| l.strip_prefix('*'))
        .map(|l| l.trim().to_string())
        .collect();
    let mut with_base = vec!["Skyrim.esm".to_string(), "Update.esm".into(), "Dawnguard.esm".into(), "HearthFires.esm".into(), "Dragonborn.esm".into()];
    with_base.extend(order);
    let c = HairCatalog::open(Path::new(data), &with_base);
    for p in c.problems() {
        println!("problem: {p}");
    }
    for (plugin, n) in c.plugins_with_hair() {
        println!("{n:5}  {plugin}");
    }
    let total = c.pickable().count();
    let playable = c.pickable().filter(|h| h.playable).count();
    println!("{total} pickable, {playable} playable, {} head parts in all", c.all().len());
    if let [out, keep @ ..] = rest {
        let hidden: Vec<_> = c
            .pickable()
            .filter(|h| h.playable && !keep.iter().any(|k| h.label().to_lowercase().contains(&k.to_lowercase()) || h.key.plugin.contains(&k.to_lowercase())))
            .map(|h| h.key.clone())
            .collect();
        c.write_patch(&hidden, Path::new(out))?;
        println!("hiding {} in {out}", hidden.len());
    }
    Ok(())
}
