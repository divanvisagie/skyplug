//! Draw a model from a game's files to a PPM picture.
//!
//!     cargo run --example shot -- <game dir> <wine prefix> <asset path> <out.ppm> [azimuth]
use std::path::PathBuf;

use skyplug_core::render::{self, Camera, Model, Options};
use skyplug_core::{DEFAULT_APPID, GameInstall, GamePaths};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [game, prefix, asset, out, rest @ ..] = &a[..] else { anyhow::bail!("usage: shot <game dir> <prefix> <asset> <out.ppm> [azimuth]") };
    let paths = GamePaths::new(&GameInstall::new(PathBuf::from(game), Some(PathBuf::from(prefix)))?, DEFAULT_APPID);
    let resolver = paths.asset_resolver()?;
    let model = Model::from_asset(&resolver, asset, 1024)?.ok_or_else(|| anyhow::anyhow!("{asset}: not found"))?;
    for w in &model.warnings {
        println!("warning: {w}");
    }
    for t in &model.textures {
        println!("{} slot {}: {} {:?}", t.mesh, t.slot, t.path, t.result);
    }
    println!("{} meshes, bounds {:?}", model.mesh_count(), model.bounds());
    let azimuth = rest.first().and_then(|s| s.parse().ok()).unwrap_or(35.0);
    let img = render::render(&model, &Camera { azimuth, elevation: 5.0, zoom: 1.0 }, &Options { width: 256, height: 256, supersample: 2, ..Options::default() });
    let mut ppm = format!("P6\n{} {}\n255\n", img.width, img.height).into_bytes();
    for p in img.pixels.chunks_exact(4) {
        ppm.extend_from_slice(&p[..3]);
    }
    std::fs::write(out, ppm)?;
    Ok(())
}
