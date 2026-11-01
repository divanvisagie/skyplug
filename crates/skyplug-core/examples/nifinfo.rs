//! Parse a NIF file and summarise what was read.
//!
//!     cargo run --example nifinfo -- <file.nif> [--obj <out.obj>]
use skyplug_core::nif;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("a .nif file");
    let obj = (args.next().as_deref() == Some("--obj")).then(|| args.next().expect("an output path"));
    let nif = match nif::parse(&std::fs::read(&path)?) {
        Ok(n) => n,
        Err(e) => {
            println!("{path}: {e:#}");
            return Ok(());
        }
    };
    println!("{path}: {} meshes", nif.meshes.len());
    for m in &nif.meshes {
        println!(
            "  '{}': {} verts, {} tris{}{}{}{}{}",
            m.name,
            m.positions.len(),
            m.triangles.len(),
            if m.skinned { ", skinned" } else { "" },
            if m.hidden { ", hidden" } else { "" },
            if m.double_sided { ", double-sided" } else { "" },
            m.alpha.map(|a| format!(", alpha(blend {}, test {}@{})", a.blend, a.test, a.threshold)).unwrap_or_default(),
            m.shader_type.map(|t| format!(", shader {t}")).unwrap_or_default(),
        );
        for (i, t) in m.textures.iter().enumerate().filter(|(_, t)| !t.is_empty()) {
            println!("      slot {i}: {t}");
        }
    }
    if let Some((lo, hi)) = nif.bounds() {
        println!("  bounds {lo:.1?} .. {hi:.1?}");
    }
    if let Some(out) = obj {
        // A Wavefront OBJ of every visible mesh, for looking at in any viewer.
        let (mut text, mut base) = (String::new(), 1);
        for m in nif.meshes.iter().filter(|m| !m.hidden) {
            text += &format!("o {}\n", m.name.replace(' ', "_"));
            for p in &m.positions {
                text += &format!("v {} {} {}\n", p[0], p[1], p[2]);
            }
            for t in &m.uvs {
                text += &format!("vt {} {}\n", t[0], 1.0 - t[1]);
            }
            for n in &m.normals {
                text += &format!("vn {} {} {}\n", n[0], n[1], n[2]);
            }
            for t in &m.triangles {
                let f = |i: u16| {
                    let k = i as usize + base;
                    match (m.uvs.is_empty(), m.normals.is_empty()) {
                        (false, false) => format!("{k}/{k}/{k}"),
                        (false, true) => format!("{k}/{k}"),
                        (true, false) => format!("{k}//{k}"),
                        _ => k.to_string(),
                    }
                };
                text += &format!("f {} {} {}\n", f(t[0]), f(t[1]), f(t[2]));
            }
            base += m.positions.len();
        }
        std::fs::write(&out, text)?;
        println!("  wrote {out}");
    }
    for w in &nif.warnings {
        println!("  warning: {w}");
    }
    if !nif.unsupported.is_empty() {
        println!("  unsupported: {}", nif.unsupported.join(", "));
    }
    Ok(())
}
