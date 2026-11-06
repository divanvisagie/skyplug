//! Try to load every pickable hair's model of an install, and say which fail and why.
//!
//!     cargo run --release --example hairscan -- <game dir> <wine prefix>
use std::collections::BTreeMap;
use std::path::PathBuf;

use skyplug_core::hair::HairCatalog;
use skyplug_core::render::{Model, TextureResult};
use skyplug_core::{DEFAULT_APPID, GameInstall, GamePaths};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [game, prefix] = &a[..] else { anyhow::bail!("usage: hairscan <game dir> <prefix>") };
    let paths = GamePaths::new(&GameInstall::new(PathBuf::from(game), Some(PathBuf::from(prefix)))?, DEFAULT_APPID);
    let catalog = HairCatalog::for_game(&paths)?;
    let resolver = paths.asset_resolver()?;
    let mut by_problem: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (mut ok, mut total) = (0, 0);
    for h in catalog.pickable().filter(|h| h.playable) {
        total += 1;
        let who = format!("{} ({})", h.label(), h.defined_in);
        let problem = if h.model.is_empty() {
            Some("no model".to_string())
        } else {
            match Model::from_asset(&resolver, &h.model, 64) {
                Err(e) => Some(format!("error: {e:#}")),
                Ok(None) => Some(format!("no such file: {}", h.model)),
                Ok(Some(m)) if m.mesh_count() == 0 => Some(format!("nothing drawn: {}", m.warnings.first().cloned().unwrap_or_default())),
                Ok(Some(m)) => m.textures.iter().find_map(|t| match &t.result {
                    TextureResult::Missing if t.slot == 0 => Some(format!("missing diffuse {}", t.path)),
                    TextureResult::Unreadable(why) if t.slot == 0 => Some(format!("unreadable diffuse: {why}")),
                    _ => None,
                }),
            }
        };
        match problem {
            None => ok += 1,
            Some(p) => by_problem.entry(p).or_default().push(who),
        }
    }
    println!("{ok} of {total} hairs load with a diffuse texture");
    let mut problems: Vec<_> = by_problem.into_iter().collect();
    problems.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for (p, who) in problems.iter().take(25) {
        println!("{:4}  {p}   e.g. {}", who.len(), who[0]);
    }
    println!("{} kinds of problem", problems.len());
    Ok(())
}
