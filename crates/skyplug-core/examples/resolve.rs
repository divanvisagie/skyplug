//! Which file does the game use for an asset path, and what does it hide?
//!
//!     cargo run --example resolve -- <game dir> [--prefix <wine prefix>] <asset path> [--out <file>]
use std::path::PathBuf;

use skyplug_core::{DEFAULT_APPID, GameInstall, GamePaths};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let game = PathBuf::from(args.next().expect("game folder"));
    let (mut prefix, mut asset, mut out) = (None, None, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--prefix" => prefix = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            _ => asset = Some(a),
        }
    }
    let paths = GamePaths::new(&GameInstall::new(game, prefix)?, DEFAULT_APPID);
    let r = paths.asset_resolver()?;
    println!("{} archives", r.archives().len());
    for w in r.warnings() {
        println!("warning: {w}");
    }
    let asset = asset.expect("asset path");
    match r.resolve(&asset) {
        None => println!("{asset}: not found"),
        Some(found) => {
            println!("{}: {}", found.path, r.describe(&found.winner));
            for s in &found.shadowed {
                println!("  hides: {}", r.describe(s));
            }
            if let Some(out) = out {
                std::fs::write(&out, r.read(&found.winner)?)?;
                println!("wrote {}", out.display());
            }
        }
    }
    Ok(())
}
