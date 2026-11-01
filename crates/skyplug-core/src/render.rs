//! A small software renderer for previewing models: it draws [`nif`]
//! meshes with their diffuse textures into an RGBA image, on the CPU, with no
//! window or GPU.
//!
//! It is for checking, not for matching the game. It shows shape, texture
//! mapping, colours, transparency and which file a texture came from. It
//! doesn't do the game's shading: no normal, specular, glow or environment
//! maps, no skin or hair tint, no vertex colours, no shadows.
//!
//! Orthographic camera, z-buffer, backface culling for single-sided
//! materials, alpha test and alpha blending as the mesh's `NiAlphaProperty`
//! says, and optional supersampling for smooth edges. The same input always
//! gives the same pixels, so output can be compared in tests.
//!
//! Skyrim's axes: Z is up and a character faces +Y. Azimuth 0 looks at the
//! front, 90 at the character's right side, 180 at the back.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use crate::assets::AssetResolver;
use crate::dds::{Dds, Rgba};
use crate::nif::{self, Mesh, Nif};

/// Whether triangles wind counter-clockwise when seen from the front, as
/// Skyrim's do.
const FRONT_IS_CCW: bool = true;

/// The female body, hands and feet the game wears under armour, as game
/// paths: draw them with [`Model::merge_underlay`] for scale.
pub const FEMALE_BODY: [&str; 3] = [
    "meshes/actors/character/character assets/femalebody_1.nif",
    "meshes/actors/character/character assets/femalehands_1.nif",
    "meshes/actors/character/character assets/femalefeet_1.nif",
];

/// `BSLightingShaderProperty`'s shader type for skin.
const SHADER_SKIN_TINT: u32 = 5;

/// A decoded texture, RGBA8.
#[derive(Debug, Clone)]
pub struct Texture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl From<Rgba> for Texture {
    fn from(i: Rgba) -> Self {
        Texture { width: i.width, height: i.height, pixels: i.pixels }
    }
}

impl Texture {
    /// A one-colour texture, for tests and fallbacks.
    pub fn solid(rgba: [u8; 4]) -> Self {
        Texture { width: 1, height: 1, pixels: rgba.to_vec() }
    }

    /// Bilinear sample at `(u, v)` with wrapping; `v` runs top to bottom.
    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        let (w, h) = (self.width as i64, self.height as i64);
        let x = u * w as f32 - 0.5;
        let y = v * h as f32 - 0.5;
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let px = |ix: i64, iy: i64| -> [f32; 4] {
            let i = (iy.rem_euclid(h) * w + ix.rem_euclid(w)) as usize * 4;
            [self.pixels[i] as f32, self.pixels[i + 1] as f32, self.pixels[i + 2] as f32, self.pixels[i + 3] as f32]
        };
        let (x0, y0) = (x0 as i64, y0 as i64);
        let (a, b, c, d) = (px(x0, y0), px(x0 + 1, y0), px(x0, y0 + 1), px(x0 + 1, y0 + 1));
        let mut o = [0f32; 4];
        for k in 0..4 {
            o[k] = (a[k] * (1.0 - fx) + b[k] * fx) * (1.0 - fy) + (c[k] * (1.0 - fx) + d[k] * fx) * fy;
        }
        o
    }
}

/// What happened to one texture a mesh names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextureResult {
    /// The game would use `source` (see [`AssetResolver::describe`]); `hides`
    /// lists the other copies that lose to it.
    Found { source: String, hides: Vec<String> },
    /// Neither loose nor in any archive.
    Missing,
    /// Found but not readable as a texture, with why.
    Unreadable(String),
}

/// One texture reference of one mesh, for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureStatus {
    pub mesh: String,
    pub slot: usize,
    pub path: String,
    pub result: TextureResult,
}

#[derive(Debug, Clone)]
enum Diffuse {
    /// The mesh names no diffuse texture.
    None,
    /// It names one that couldn't be loaded; drawn magenta so it can't be missed.
    Broken,
    Texture(Arc<Texture>),
}

#[derive(Debug, Clone)]
struct ModelMesh {
    mesh: Mesh,
    diffuse: Diffuse,
    /// Drawn behind anything else that's about as deep (see [`Options::garment_bias`]).
    underlay: bool,
}

/// Meshes with their textures loaded, ready to render.
#[derive(Debug, Clone, Default)]
pub struct Model {
    meshes: Vec<ModelMesh>,
    /// Every texture the meshes name, and where each came from.
    pub textures: Vec<TextureStatus>,
    /// Problems from reading the model, as [`nif::Nif::warnings`], plus texture problems.
    pub warnings: Vec<String>,
}

impl Model {
    /// Load the diffuse texture of each mesh of `nif` through `resolver`, at
    /// most about `max_texture_side` pixels on a side (so big textures decode
    /// at a smaller mip level), and report where every texture came from.
    pub fn load(resolver: &AssetResolver, nif: &Nif, max_texture_side: usize) -> Model {
        let mut model = Model { warnings: nif.warnings.clone(), ..Model::default() };
        let mut cache: HashMap<String, Result<Arc<Texture>, String>> = HashMap::new();
        for mesh in nif.meshes.iter().filter(|m| !m.hidden) {
            let mut diffuse = Diffuse::None;
            for (slot, path) in mesh.textures.iter().enumerate().filter(|(_, p)| !p.is_empty()) {
                let result = match resolver.resolve(path) {
                    None => {
                        if slot == nif::slot::DIFFUSE {
                            diffuse = Diffuse::Broken;
                        }
                        TextureResult::Missing
                    }
                    Some(found) => {
                        let source = TextureResult::Found {
                            source: resolver.describe(&found.winner),
                            hides: found.shadowed.iter().map(|s| resolver.describe(s)).collect(),
                        };
                        if slot != nif::slot::DIFFUSE {
                            source
                        } else {
                            let loaded = cache.entry(found.path.clone()).or_insert_with(|| {
                                resolver
                                    .read(&found.winner)
                                    .and_then(|b| {
                                        let d = Dds::parse(&b)?;
                                        d.decode(d.level_for(max_texture_side))
                                    })
                                    .map(|i| Arc::new(Texture::from(i)))
                                    .map_err(|e| format!("{e:#}"))
                            });
                            match loaded {
                                Ok(t) => {
                                    diffuse = Diffuse::Texture(t.clone());
                                    source
                                }
                                Err(e) => {
                                    diffuse = Diffuse::Broken;
                                    model.warnings.push(format!("texture {path}: {e}"));
                                    TextureResult::Unreadable(e.clone())
                                }
                            }
                        }
                    }
                };
                model.textures.push(TextureStatus { mesh: mesh.name.clone(), slot, path: path.clone(), result });
            }
            // Shapes with the skin-tint shader are bare skin: a body, hands or feet bundled in an
            // armour file. Treat them as the body underlay so clothes drawn over them show.
            let underlay = mesh.shader_type == Some(SHADER_SKIN_TINT);
            model.meshes.push(ModelMesh { mesh: mesh.clone(), diffuse, underlay });
        }
        model
    }

    /// Read the model at `path` (a game asset path) and load it. `None` if
    /// the game has no such file.
    pub fn from_asset(resolver: &AssetResolver, path: &str, max_texture_side: usize) -> Result<Option<Model>> {
        let Some(bytes) = resolver.read_asset(path)? else { return Ok(None) };
        let nif = nif::parse(&bytes)?;
        Ok(Some(Model::load(resolver, &nif, max_texture_side)))
    }

    /// A model from meshes and their diffuse textures, without a resolver.
    pub fn from_meshes(meshes: Vec<(Mesh, Option<Texture>)>) -> Model {
        let meshes = meshes
            .into_iter()
            .map(|(mesh, t)| ModelMesh { mesh, diffuse: t.map_or(Diffuse::None, |t| Diffuse::Texture(Arc::new(t))), underlay: false })
            .collect();
        Model { meshes, ..Model::default() }
    }

    /// Put `other`'s meshes and reports into this model.
    pub fn merge(&mut self, other: Model) {
        self.meshes.extend(other.meshes);
        self.textures.extend(other.textures);
        self.warnings.extend(other.warnings);
    }

    /// Like [`Self::merge`], but `other` is drawn as an underlay: the body under
    /// clothes. In the game, worn clothes hide the body they cover; here the
    /// body is drawn, but anything else within [`Options::garment_bias`] of it
    /// is drawn in front, so a garment lying slightly inside the body still shows.
    pub fn merge_underlay(&mut self, mut other: Model) {
        for m in &mut other.meshes {
            m.underlay = true;
        }
        self.merge(other);
    }

    /// Number of meshes that will be drawn.
    pub fn mesh_count(&self) -> usize {
        self.meshes.len()
    }

    /// Smallest box holding every vertex, or `None` if there are none.
    pub fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        let mut it = self.meshes.iter().flat_map(|m| m.mesh.positions.iter());
        let first = *it.next()?;
        Some(it.fold((first, first), |(mut lo, mut hi), p| {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
            (lo, hi)
        }))
    }
}

/// Where the camera looks from, in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// 0 is the front (the camera at +Y), 90 the character's right (+X), 180 the back.
    pub azimuth: f32,
    /// Degrees above the horizon.
    pub elevation: f32,
    /// 1 fits the whole model; larger zooms in.
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera { azimuth: 0.0, elevation: 6.0, zoom: 1.0 }
    }
}

/// How to render.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub width: usize,
    pub height: usize,
    /// Render at this many times the size in each direction and average down (1 turns it off).
    pub supersample: usize,
    /// Background colour, straight alpha (use alpha 0 for a transparent one).
    pub background: [u8; 4],
    /// How far, in game units, an underlay (the body) is pushed back, so a
    /// garment lying slightly inside it is still drawn in front.
    pub garment_bias: f32,
}

impl Default for Options {
    fn default() -> Self {
        Options { width: 512, height: 512, supersample: 2, background: [38, 41, 50, 255], garment_bias: 4.0 }
    }
}

type V3 = [f32; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn norm(a: V3) -> V3 {
    let l = dot(a, a).sqrt();
    if l > 1e-12 { [a[0] / l, a[1] / l, a[2] / l] } else { [0.0, 0.0, 1.0] }
}
fn add_scaled(a: V3, b: V3, s: f32) -> V3 {
    [a[0] + b[0] * s, a[1] + b[1] * s, a[2] + b[2] * s]
}

/// The camera's frame and the model-to-pixel mapping.
struct View {
    center: V3,
    right: V3,
    up: V3,
    /// Toward the camera.
    toward: V3,
    scale: f32,
    w: f32,
    h: f32,
}

impl View {
    fn new(model: &Model, cam: &Camera, w: usize, h: usize) -> View {
        let (lo, hi) = model.bounds().unwrap_or(([-1.0; 3], [1.0; 3]));
        let center = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
        let radius = (dot(sub(hi, lo), sub(hi, lo)).sqrt() / 2.0).max(1e-3);
        let (az, el) = (cam.azimuth.to_radians(), cam.elevation.to_radians());
        let toward = [az.sin() * el.cos(), az.cos() * el.cos(), el.sin()];
        let right = norm(cross([-toward[0], -toward[1], -toward[2]], [0.0, 0.0, 1.0]));
        let up = cross(right, [-toward[0], -toward[1], -toward[2]]);
        let scale = (w.min(h) as f32) / (2.0 * radius) * 0.96 * cam.zoom.max(0.01);
        View { center, right, up, toward, scale, w: w as f32, h: h as f32 }
    }

    /// Pixel x, pixel y, and depth (larger is nearer the camera).
    fn project(&self, p: V3) -> (f32, f32, f32) {
        let q = sub(p, self.center);
        (self.w / 2.0 + dot(q, self.right) * self.scale, self.h / 2.0 - dot(q, self.up) * self.scale, dot(q, self.toward))
    }
}

struct Frame {
    w: usize,
    h: usize,
    /// Straight-alpha RGBA, 0 to 255 as floats.
    color: Vec<[f32; 4]>,
    depth: Vec<f32>,
}

/// Draw `model` from `camera`.
pub fn render(model: &Model, camera: &Camera, opts: &Options) -> Rgba {
    let s = opts.supersample.max(1);
    let (w, h) = (opts.width.max(1) * s, opts.height.max(1) * s);
    let bg = opts.background.map(|c| c as f32);
    let mut f = Frame { w, h, color: vec![bg; w * h], depth: vec![f32::NEG_INFINITY; w * h] };
    let view = View::new(model, camera, w, h);

    // Opaque and alpha-tested meshes first, writing depth; then blended ones, far to near.
    let blended = |m: &ModelMesh| m.mesh.alpha.is_some_and(|a| a.blend);
    for m in model.meshes.iter().filter(|m| !blended(m)) {
        draw(&mut f, &view, m, true, opts.garment_bias);
    }
    let mut late: Vec<&ModelMesh> = model.meshes.iter().filter(|m| blended(m)).collect();
    let centroid_depth = |m: &ModelMesh| {
        let n = m.mesh.positions.len().max(1) as f32;
        m.mesh.positions.iter().map(|&p| view.project(p).2).sum::<f32>() / n
    };
    late.sort_by(|a, b| centroid_depth(a).total_cmp(&centroid_depth(b)));
    for m in late {
        draw(&mut f, &view, m, false, opts.garment_bias);
    }

    // Average s x s blocks down, weighting by alpha so transparent pixels don't darken edges.
    let (ow, oh) = (opts.width.max(1), opts.height.max(1));
    let mut pixels = vec![0u8; ow * oh * 4];
    let n = (s * s) as f32;
    for y in 0..oh {
        for x in 0..ow {
            let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..s {
                for sx in 0..s {
                    let p = f.color[(y * s + sy) * w + x * s + sx];
                    let pa = p[3] / 255.0;
                    r += p[0] * pa;
                    g += p[1] * pa;
                    b += p[2] * pa;
                    a += pa;
                }
            }
            let o = (y * ow + x) * 4;
            if a > 0.0 {
                pixels[o] = (r / a).round().clamp(0.0, 255.0) as u8;
                pixels[o + 1] = (g / a).round().clamp(0.0, 255.0) as u8;
                pixels[o + 2] = (b / a).round().clamp(0.0, 255.0) as u8;
            }
            pixels[o + 3] = (a / n * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    Rgba { width: ow, height: oh, pixels }
}

fn draw(f: &mut Frame, view: &View, m: &ModelMesh, write_depth: bool, garment_bias: f32) {
    let bias = if m.underlay { -garment_bias } else { 0.0 };
    let mesh = &m.mesh;
    let pos = &mesh.positions;
    let has_uv = mesh.uvs.len() == pos.len();
    let has_normals = mesh.normals.len() == pos.len();
    let (test, blend, threshold) = mesh.alpha.map_or((false, false, 0.0), |a| (a.test, a.blend, a.threshold as f32));
    // Light from above-left of the camera, plus an ambient floor.
    let light = norm(add_scaled(add_scaled(add_scaled([0.0; 3], view.right, -0.35), view.up, 0.55), view.toward, 0.8));

    for t in &mesh.triangles {
        let [ia, ib, ic] = [t[0] as usize, t[1] as usize, t[2] as usize];
        let (pa, pb, pc) = (pos[ia], pos[ib], pos[ic]);
        let face = cross(sub(pb, pa), sub(pc, pa));
        let facing = dot(face, view.toward);
        if facing == 0.0 {
            continue;
        }
        let front = (facing > 0.0) == FRONT_IS_CCW;
        if !front && !mesh.double_sided {
            continue;
        }
        let (a, b, c) = (view.project(pa), view.project(pb), view.project(pc));
        let area = (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
        let finite = [a.0, a.1, a.2, b.0, b.1, b.2, c.0, c.1, c.2].iter().all(|v| v.is_finite());
        if !finite || area.abs() < 1e-9 {
            continue;
        }
        let x0 = a.0.min(b.0).min(c.0).floor().max(0.0) as usize;
        let y0 = a.1.min(b.1).min(c.1).floor().max(0.0) as usize;
        let x1 = (a.0.max(b.0).max(c.0).ceil().max(0.0) as usize).min(f.w.saturating_sub(1));
        let y1 = (a.1.max(b.1).max(c.1).ceil().max(0.0) as usize).min(f.h.saturating_sub(1));
        // The side facing the camera, for lighting a back face of a double-sided mesh.
        let flip = if facing > 0.0 { 1.0 } else { -1.0 };
        let face_n = norm([face[0] * flip, face[1] * flip, face[2] * flip]);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let w0 = ((c.0 - b.0) * (py - b.1) - (c.1 - b.1) * (px - b.0)) / area;
                let w1 = ((a.0 - c.0) * (py - c.1) - (a.1 - c.1) * (px - c.0)) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < -1e-5 || w1 < -1e-5 || w2 < -1e-5 {
                    continue;
                }
                let z = w0 * a.2 + w1 * b.2 + w2 * c.2 + bias;
                let i = y * f.w + x;
                if z <= f.depth[i] {
                    continue;
                }
                let mut rgba = match &m.diffuse {
                    Diffuse::Texture(tex) if has_uv => {
                        let u = w0 * mesh.uvs[ia][0] + w1 * mesh.uvs[ib][0] + w2 * mesh.uvs[ic][0];
                        let v = w0 * mesh.uvs[ia][1] + w1 * mesh.uvs[ib][1] + w2 * mesh.uvs[ic][1];
                        tex.sample(u, v)
                    }
                    Diffuse::Texture(tex) => tex.sample(0.5, 0.5),
                    Diffuse::Broken => [255.0, 0.0, 255.0, 255.0],
                    Diffuse::None => [178.0, 178.0, 178.0, 255.0],
                };
                // Texture alpha matters only where the material asks for it.
                if test && rgba[3] <= threshold {
                    continue;
                }
                let alpha = if blend { rgba[3] / 255.0 } else { 1.0 };
                if alpha <= 0.0 {
                    continue;
                }
                let n = if has_normals {
                    let (na, nb, nc) = (mesh.normals[ia], mesh.normals[ib], mesh.normals[ic]);
                    let n = norm(std::array::from_fn(|k| w0 * na[k] + w1 * nb[k] + w2 * nc[k]));
                    [n[0] * flip, n[1] * flip, n[2] * flip]
                } else {
                    face_n
                };
                let lit = (0.42 + 0.68 * dot(n, light).max(0.0)).min(1.0);
                for ch in &mut rgba[..3] {
                    *ch *= lit;
                }
                let dst = f.color[i];
                if alpha >= 1.0 {
                    f.color[i] = [rgba[0], rgba[1], rgba[2], 255.0];
                } else {
                    // Straight-alpha "over".
                    let da = dst[3] / 255.0;
                    let oa = alpha + da * (1.0 - alpha);
                    let mut o = [0f32; 4];
                    for k in 0..3 {
                        o[k] = (rgba[k] * alpha + dst[k] * da * (1.0 - alpha)) / oa;
                    }
                    o[3] = oa * 255.0;
                    f.color[i] = o;
                }
                if write_depth && alpha >= 1.0 {
                    f.depth[i] = z;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nif::Alpha;

    /// A triangle in the XZ plane facing +Y (front), wound counter-clockwise from there.
    fn tri(y: f32) -> Mesh {
        Mesh {
            name: "t".into(),
            positions: vec![[-1.0, y, -1.0], [1.0, y, -1.0], [0.0, y, 1.0]],
            // Seen from +Y looking toward -Y, screen right is -X, so this order is counter-clockwise.
            triangles: vec![[0, 2, 1]],
            ..Mesh::default()
        }
    }

    fn opts() -> Options {
        Options { width: 33, height: 33, supersample: 1, background: [0, 0, 0, 255], garment_bias: 1.5 }
    }

    fn px(img: &Rgba, x: usize, y: usize) -> [u8; 4] {
        let i = (y * img.width + x) * 4;
        img.pixels[i..i + 4].try_into().unwrap()
    }

    fn front() -> Camera {
        Camera { azimuth: 0.0, elevation: 0.0, zoom: 1.0 }
    }

    /// Colour with the lighting factor divided out is awkward; compare the dominant channel instead.
    fn is_reddish(p: [u8; 4]) -> bool {
        p[0] > 100 && p[1] < 40 && p[2] < 40
    }

    #[test]
    fn draws_a_textured_triangle_and_leaves_the_background() {
        let m = Model::from_meshes(vec![(tri(0.0), Some(Texture::solid([255, 0, 0, 255])))]);
        let img = render(&m, &front(), &opts());
        assert!(is_reddish(px(&img, 16, 20)), "{:?}", px(&img, 16, 20));
        assert_eq!(px(&img, 1, 1), [0, 0, 0, 255]);
        assert_eq!((img.width, img.height), (33, 33));
    }

    #[test]
    fn back_faces_are_culled_unless_double_sided() {
        let mut m = tri(0.0);
        let model = |m: &Mesh| Model::from_meshes(vec![(m.clone(), Some(Texture::solid([255, 0, 0, 255])))]);
        let behind = Camera { azimuth: 180.0, ..front() };
        assert_eq!(px(&render(&model(&m), &behind, &opts()), 16, 20), [0, 0, 0, 255], "seen from behind: culled");
        m.double_sided = true;
        assert!(is_reddish(px(&render(&model(&m), &behind, &opts()), 16, 20)));
    }

    #[test]
    fn nearer_triangle_wins_from_each_side() {
        let (mut near, mut far) = (tri(1.0), tri(-1.0));
        near.double_sided = true;
        far.double_sided = true;
        let m = Model::from_meshes(vec![(far, Some(Texture::solid([0, 0, 255, 255]))), (near, Some(Texture::solid([255, 0, 0, 255])))]);
        // From the front (camera at +Y) the y = +1 triangle is nearer.
        assert!(is_reddish(px(&render(&m, &front(), &opts()), 16, 20)));
        let back = render(&m, &Camera { azimuth: 180.0, ..front() }, &opts());
        let p = px(&back, 16, 20);
        assert!(p[2] > 100 && p[0] < 40, "{p:?}");
    }

    #[test]
    fn an_underlay_loses_to_a_garment_just_behind_it_but_not_to_one_far_behind() {
        let red = || Some(Texture::solid([255, 0, 0, 255]));
        let blue = || Some(Texture::solid([0, 0, 255, 255]));
        // Body at y = 0.0 (nearer the camera), garment at y = -1.0 (behind it by 1 unit, under the 1.5 bias).
        let mut body = Model::from_meshes(vec![(tri(0.0), blue())]);
        let mut garment = tri(-1.0);
        garment.double_sided = true;
        let mut m = Model::from_meshes(vec![(garment, red())]);
        m.merge_underlay(std::mem::take(&mut body));
        assert!(is_reddish(px(&render(&m, &front(), &opts()), 16, 20)), "garment shows through the body");
        // Garment 3 units behind: now the body hides it.
        let mut far = tri(-3.0);
        far.double_sided = true;
        let mut m = Model::from_meshes(vec![(far, red())]);
        m.merge_underlay(Model::from_meshes(vec![(tri(0.0), blue())]));
        let p = px(&render(&m, &front(), &opts()), 16, 20);
        assert!(p[2] > 100 && p[0] < 40, "{p:?}");
    }

    #[test]
    fn skin_shaded_shapes_are_treated_as_the_body_underlay() {
        let resolver = AssetResolver::new(std::env::temp_dir().join("skyplug_no_such_data"), []);
        let (mut skin, mut cloth) = (tri(0.0), tri(0.0));
        skin.shader_type = Some(SHADER_SKIN_TINT);
        cloth.shader_type = Some(0);
        let model = Model::load(&resolver, &Nif { meshes: vec![skin, cloth], ..Nif::default() }, 256);
        assert!(model.meshes[0].underlay && !model.meshes[1].underlay);
    }

    #[test]
    fn alpha_test_cuts_out_and_alpha_blend_mixes() {
        let mut m = tri(0.0);
        m.alpha = Some(Alpha { blend: false, test: true, threshold: 128 });
        let clear = Texture::solid([255, 0, 0, 100]);
        assert_eq!(px(&render(&Model::from_meshes(vec![(m.clone(), Some(clear))]), &front(), &opts()), 16, 20), [0, 0, 0, 255]);
        let solid = Texture::solid([255, 0, 0, 200]);
        assert!(is_reddish(px(&render(&Model::from_meshes(vec![(m.clone(), Some(solid))]), &front(), &opts()), 16, 20)));

        m.alpha = Some(Alpha { blend: true, test: false, threshold: 0 });
        let half = Texture::solid([255, 0, 0, 128]);
        let p = px(&render(&Model::from_meshes(vec![(m, Some(half))]), &front(), &opts()), 16, 20);
        // About half red over black.
        assert!(p[0] > 40 && p[0] < 140 && p[1] < 10, "{p:?}");
    }

    #[test]
    fn texture_coordinates_pick_the_texel() {
        // A 2x1 texture, red then blue; the triangle's UVs span u from 0 to 1.
        let tex = Texture { width: 2, height: 1, pixels: vec![255, 0, 0, 255, 0, 0, 255, 255] };
        let mut m = Mesh {
            positions: vec![[-1.0, 0.0, -1.0], [1.0, 0.0, -1.0], [-1.0, 0.0, 1.0], [1.0, 0.0, 1.0]],
            triangles: vec![[0, 2, 1], [1, 2, 3]],
            ..Mesh::default()
        };
        // Screen right is -X from the front, so x = +1 (u = 0, red) is on the left of the image.
        m.uvs = vec![[1.0, 1.0], [0.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
        let img = render(&Model::from_meshes(vec![(m, Some(tex))]), &front(), &Options { supersample: 1, ..opts() });
        let (left, right) = (px(&img, 10, 16), px(&img, 22, 16)); // the texel centres
        assert!(is_reddish(left), "left should be red: {left:?}");
        assert!(right[2] > 100 && right[0] < 40, "right should be blue: {right:?}");
    }

    #[test]
    fn missing_textures_show_as_magenta_and_no_texture_as_grey() {
        let mut m = Model::from_meshes(vec![(tri(0.0), None)]);
        let grey = px(&render(&m, &front(), &opts()), 16, 20);
        assert!(grey[0] > 60 && grey[0].abs_diff(grey[1]) < 3 && grey[1].abs_diff(grey[2]) < 3, "{grey:?}");
        m.meshes[0].diffuse = Diffuse::Broken;
        let p = px(&render(&m, &front(), &opts()), 16, 20);
        assert!(p[0] > 100 && p[1] < 10 && p[2] > 100, "{p:?}");
    }

    #[test]
    fn supersampling_smooths_edges_and_transparent_background_stays_transparent() {
        let m = Model::from_meshes(vec![(tri(0.0), Some(Texture::solid([255, 255, 255, 255])))]);
        let clear = Options { background: [0, 0, 0, 0], ..opts() };
        let img = render(&m, &front(), &Options { supersample: 3, ..clear });
        assert_eq!(px(&img, 1, 1)[3], 0, "background is transparent");
        assert_eq!(px(&img, 16, 20)[3], 255);
        // Somewhere along the slanted edge a pixel is partly covered.
        assert!(img.pixels.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255));
    }

    #[test]
    fn empty_models_and_degenerate_input_do_not_panic() {
        let empty = Model::default();
        let img = render(&empty, &Camera::default(), &opts());
        assert_eq!(px(&img, 5, 5), [0, 0, 0, 255]);
        // Collapsed triangle, NaN positions, a 0-size request.
        let mut m = tri(0.0);
        m.positions[1] = m.positions[0];
        let mut n = tri(0.0);
        n.positions[0] = [f32::NAN, 0.0, 0.0];
        let _ = render(&Model::from_meshes(vec![(m, None), (n, None)]), &Camera::default(), &opts());
        let _ = render(&Model::from_meshes(vec![(tri(0.0), None)]), &Camera::default(), &Options { width: 0, height: 0, ..opts() });
    }

    #[test]
    fn rendering_is_deterministic() {
        let m = Model::from_meshes(vec![(tri(0.0), Some(Texture::solid([10, 200, 90, 255])))]);
        let cam = Camera { azimuth: 33.0, elevation: 12.0, zoom: 1.2 };
        let o = Options { supersample: 2, ..opts() };
        assert_eq!(render(&m, &cam, &o), render(&m, &cam, &o));
    }

    #[test]
    fn load_reports_where_textures_come_from() {
        use crate::bsa::tests::{build, temp_dir};
        let data = temp_dir("render_load").join("Data");
        std::fs::create_dir_all(data.join("textures")).unwrap();
        let dds = crate::dds::tests::make_bgra(2, 2, [0, 0, 255, 255]); // B, G, R, A: red
        std::fs::write(data.join("textures/Loose.dds"), &dds).unwrap();
        std::fs::write(data.join("a.bsa"), build(&[("textures", "loose.dds", &dds, false), ("textures", "packed.dds", &dds, false)], false, false)).unwrap();
        let resolver = AssetResolver::new(&data, [data.join("a.bsa")]);

        let mut mesh = tri(0.0);
        mesh.textures = vec!["textures\\Loose.dds".into(), "textures\\nothere_n.dds".into()];
        let mut second = tri(0.0);
        second.name = "second".into();
        second.textures = vec!["textures\\packed.dds".into()];
        let mut third = tri(0.0);
        third.name = "third".into();
        third.textures = vec!["textures\\gone.dds".into()];
        let nif = Nif { meshes: vec![mesh, second, third], ..Nif::default() };
        let model = Model::load(&resolver, &nif, 1024);

        let by = |name: &str, slot: usize| model.textures.iter().find(|t| t.mesh == name && t.slot == slot).unwrap().result.clone();
        assert_eq!(by("t", 0), TextureResult::Found { source: "loose: textures/Loose.dds".into(), hides: vec!["a.bsa".into()] });
        assert_eq!(by("t", 1), TextureResult::Missing);
        assert_eq!(by("second", 0), TextureResult::Found { source: "a.bsa".into(), hides: vec![] });
        assert_eq!(by("third", 0), TextureResult::Missing);
        // The three triangles overlap; the first drawn wins, and it has the red loose texture.
        let img = render(&model, &front(), &opts());
        assert!(is_reddish(px(&img, 16, 20)), "{:?}", px(&img, 16, 20));
        assert_eq!(model.mesh_count(), 3);
    }

    /// Read-only: render a sample of every model in a real install (loose and
    /// in archives) with its textures. Skipped unless `SKYPLUG_TEST_GAME_DIR`
    /// is set (see the crate README). Checks nothing panics and counts what
    /// came out.
    #[test]
    fn real_install_renders() {
        use crate::GamePaths;
        use crate::steam::{DEFAULT_APPID, GameInstall};
        let Some(dir) = std::env::var_os("SKYPLUG_TEST_GAME_DIR") else {
            eprintln!("SKYPLUG_TEST_GAME_DIR not set; skipping");
            return;
        };
        let prefix = std::env::var_os("SKYPLUG_TEST_PREFIX").map(std::path::PathBuf::from);
        let paths = GamePaths::new(&GameInstall::new(std::path::PathBuf::from(dir), prefix).unwrap(), DEFAULT_APPID);
        let resolver = paths.asset_resolver().unwrap();

        // Every 20th model, from archives and from loose files.
        let mut names: Vec<String> = vec![];
        for a in resolver.archives() {
            names.extend(a.entries().iter().filter(|e| e.path.to_ascii_lowercase().ends_with(".nif")).map(|e| e.path.clone()));
        }
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, root, out);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("nif")) {
                    out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        walk(&paths.data_dir, &paths.data_dir, &mut names);
        names.sort();
        names.dedup();
        let opts = Options { width: 96, height: 96, supersample: 1, ..Options::default() };
        let (mut rendered, mut blank, mut skipped, mut tex_ok, mut tex_missing, mut tex_bad) = (0, 0, 0, 0, 0, 0);
        for name in names.iter().step_by(20) {
            let Ok(Some(model)) = Model::from_asset(&resolver, name, 128) else {
                skipped += 1; // LE, or a file that isn't a model
                continue;
            };
            for t in &model.textures {
                match t.result {
                    TextureResult::Found { .. } => tex_ok += 1,
                    TextureResult::Missing => tex_missing += 1,
                    TextureResult::Unreadable(_) => tex_bad += 1,
                }
            }
            if model.mesh_count() == 0 {
                skipped += 1;
                continue;
            }
            let img = render(&model, &Camera { azimuth: 35.0, ..Camera::default() }, &opts);
            rendered += 1;
            if img.pixels.chunks_exact(4).all(|p| p == opts.background) {
                blank += 1;
                eprintln!("  blank: {name} ({} meshes)", model.mesh_count());
            }
        }
        eprintln!("{} sampled: {rendered} rendered ({blank} blank), {skipped} not drawable; textures: {tex_ok} found, {tex_missing} missing, {tex_bad} unreadable", names.len().div_ceil(20));
        assert!(rendered > 100);
        assert!(blank * 20 < rendered, "too many blank renders: {blank} of {rendered}");
        assert_eq!(tex_bad, 0);
    }
}
