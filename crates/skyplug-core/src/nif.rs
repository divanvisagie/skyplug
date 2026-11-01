//! Reading Skyrim Special Edition `.nif` models into meshes a renderer can
//! draw: positions, normals, UVs, vertex colours, triangles, the texture
//! paths and the few material flags that decide how it looks.
//!
//! Only Skyrim SE's format is read (NIF 20.2.0.7, Bethesda stream version
//! 100). Skyrim LE's (stream version 83) is recognised and refused with a
//! [`LegacyFormat`] error, so callers can say "this is an LE mesh" instead
//! of drawing it wrong. Nothing is written or converted.
//!
//! What it reads: the node tree (with transforms), `BSTriShape` and its
//! SE variants, the older `NiTriShape` with `NiTriShapeData`, skinned shapes
//! whose geometry lives in `NiSkinPartition`, `BSLightingShaderProperty`
//! with its texture set, `BSEffectShaderProperty`'s source texture, and
//! `NiAlphaProperty`.
//!
//! What it doesn't: skeletons and skinning weights (skinned meshes come out
//! in the pose they're stored in, with the shape's own transform applied, which
//! is the upright body pose), animation,
//! collision, particles, face/head meshes (`BSDynamicTriShape`) and strips.
//! A block it can't read is skipped by its declared size, noted in
//! [`Nif::warnings`], and the rest of the file still loads; block types that
//! look like geometry but aren't handled are listed in [`Nif::unsupported`].
//!
//! Malformed input gives an error or a warning, never a panic, and no
//! allocation is sized by a number in the file without checking it fits in
//! the file first.

use std::collections::HashSet;
use std::fmt;

use anyhow::{Context, Result, ensure};

const VERSION_SSE: u32 = 0x1402_0007;
const BS_SSE: u32 = 100;
const BS_LE: u32 = 83;

/// Texture slots in [`Mesh::textures`], as `BSShaderTextureSet` orders them.
pub mod slot {
    pub const DIFFUSE: usize = 0;
    pub const NORMAL: usize = 1;
    /// Glow map, or skin/face tint, depending on the shader type.
    pub const GLOW: usize = 2;
    pub const HEIGHT: usize = 3;
    pub const ENVIRONMENT: usize = 4;
    pub const ENVIRONMENT_MASK: usize = 5;
    pub const SUBSURFACE: usize = 6;
    pub const SPECULAR: usize = 7;
}

/// The file is a Skyrim LE (or other pre-SE) model: not drawn, since its
/// vertex data and skinning differ from SE's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyFormat {
    /// The Bethesda stream version: 83 for Skyrim LE.
    pub bs_version: u32,
}

impl fmt::Display for LegacyFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.bs_version == BS_LE {
            write!(f, "Skyrim LE model (NIF stream version {}), not Skyrim SE's {}", self.bs_version, BS_SSE)
        } else {
            write!(f, "NIF stream version {} isn't Skyrim SE's ({})", self.bs_version, BS_SSE)
        }
    }
}

impl std::error::Error for LegacyFormat {}

/// A parsed model.
#[derive(Debug, Default)]
pub struct Nif {
    /// Every shape reachable from the root, in file order.
    pub meshes: Vec<Mesh>,
    /// Blocks that couldn't be read and were skipped, with why.
    pub warnings: Vec<String>,
    /// Block types that probably carry visible geometry but aren't handled
    /// (so something may be missing from `meshes`). Deduplicated, sorted.
    pub unsupported: Vec<String>,
}

/// Alpha handling from `NiAlphaProperty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alpha {
    pub blend: bool,
    pub test: bool,
    /// Alpha test cut-off, 0 to 255.
    pub threshold: u8,
}

/// One drawable shape.
#[derive(Debug, Clone, Default)]
pub struct Mesh {
    pub name: String,
    pub positions: Vec<[f32; 3]>,
    /// Same length as `positions`, or empty.
    pub normals: Vec<[f32; 3]>,
    /// Same length as `positions`, or empty. `v` runs top to bottom, as in DDS files.
    pub uvs: Vec<[f32; 2]>,
    /// RGBA, same length as `positions`, or empty.
    pub colors: Vec<[u8; 4]>,
    pub triangles: Vec<[u16; 3]>,
    /// Texture paths by [`slot`], as stored (any case, backslashes). Empty
    /// string for an unused slot; empty list for a shape with no material.
    pub textures: Vec<String>,
    /// `BSLightingShaderProperty`'s shader type (0 default, 1 environment
    /// map, 5 skin tint, 6 hair tint, ...), or `None` for other materials.
    pub shader_type: Option<u32>,
    pub alpha: Option<Alpha>,
    /// Drawn from both sides.
    pub double_sided: bool,
    /// The file marks the shape hidden; consumers usually skip these.
    pub hidden: bool,
    /// The shape is skinned to a skeleton. Its vertices are in the pose they're
    /// stored in; no bone weights are applied. Node transforms are baked in either way.
    pub skinned: bool,
    /// The body-part number of each of the shape's skin partitions (`BSDismemberSkinInstance`):
    /// 30 to 61 is a biped slot, and 130 to 161 are the same slots as the game's head parts
    /// number them. Empty for a shape with no such partitions. The game only draws a partition
    /// whose slot is among the slots of the armour addon wearing the model.
    pub body_parts: Vec<u16>,
}

impl Mesh {
    /// The texture path in `slot` if there is one.
    pub fn texture(&self, slot: usize) -> Option<&str> {
        self.textures.get(slot).map(String::as_str).filter(|s| !s.is_empty())
    }
}

impl Nif {
    /// Smallest box holding every vertex of every mesh, or `None` if there are none.
    pub fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        let mut it = self.meshes.iter().flat_map(|m| m.positions.iter());
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

// ---------------------------------------------------------------- reading

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(d: &'a [u8]) -> Self {
        Reader { d, p: 0 }
    }
    fn left(&self) -> usize {
        self.d.len() - self.p
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(n <= self.left(), "data ends {} bytes early (at {})", n - self.left(), self.p);
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
    /// `n` items of `size` bytes must fit in what's left.
    fn fits(&self, n: usize, size: usize) -> Result<usize> {
        ensure!(n.checked_mul(size).is_some_and(|t| t <= self.left()), "count {n} doesn't fit in the data");
        Ok(n)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn f32s<const N: usize>(&mut self) -> Result<[f32; N]> {
        let mut o = [0f32; N];
        for v in &mut o {
            *v = self.f32()?;
        }
        Ok(o)
    }
    /// A string with a 32-bit length in front.
    fn sized(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    /// A string with an 8-bit length in front (counting a trailing NUL).
    fn short(&mut self) -> Result<String> {
        let n = self.u8()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).trim_end_matches('\0').to_string())
    }
    fn refs(&mut self) -> Result<Vec<i32>> {
        let n = self.u32()? as usize;
        self.fits(n, 4)?;
        (0..n).map(|_| self.i32()).collect()
    }
}

fn half(h: u16) -> f32 {
    let s = if h >> 15 == 1 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1f) as i32;
    let m = (h & 0x3ff) as f32;
    s * match e {
        0 => m * 2f32.powi(-24),
        31 => if m == 0.0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

// -------------------------------------------------------------- transforms

#[derive(Clone, Copy)]
struct Xf {
    t: [f32; 3],
    r: [[f32; 3]; 3],
    s: f32,
}

impl Xf {
    const IDENT: Xf = Xf { t: [0.0; 3], r: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], s: 1.0 };

    fn is_ident(&self) -> bool {
        self.t == [0.0; 3] && self.r == Self::IDENT.r && self.s == 1.0
    }
    fn rotate(&self, v: [f32; 3]) -> [f32; 3] {
        let mut o = [0f32; 3];
        for (i, row) in self.r.iter().enumerate() {
            o[i] = row[0] * v[0] + row[1] * v[1] + row[2] * v[2];
        }
        o
    }
    fn point(&self, v: [f32; 3]) -> [f32; 3] {
        let r = self.rotate(v);
        [self.t[0] + self.s * r[0], self.t[1] + self.s * r[1], self.t[2] + self.s * r[2]]
    }
    /// `self` is the parent's transform, `c` the child's: the child's, in the parent's parent's space.
    fn then(&self, c: &Xf) -> Xf {
        let mut r = [[0f32; 3]; 3];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..3).map(|k| self.r[i][k] * c.r[k][j]).sum();
            }
        }
        Xf { t: self.point(c.t), r, s: self.s * c.s }
    }
}

// ------------------------------------------------------------------ blocks

struct AvObject {
    name: String,
    flags: u32,
    xf: Xf,
}

fn string_at(strings: &[String], i: i32) -> String {
    usize::try_from(i).ok().and_then(|i| strings.get(i)).cloned().unwrap_or_default()
}

/// The `NiObjectNET` part: name, extra data and controller.
fn object_net(r: &mut Reader, strings: &[String]) -> Result<String> {
    let name = string_at(strings, r.i32()?);
    r.refs()?; // extra data
    r.i32()?; // controller
    Ok(name)
}

/// `NiAVObject`: `NiObjectNET`, flags, transform and collision.
fn av_object(r: &mut Reader, strings: &[String]) -> Result<AvObject> {
    let name = object_net(r, strings)?;
    let flags = r.u32()?;
    let t = r.f32s::<3>()?;
    let mut rot = [[0f32; 3]; 3];
    for row in &mut rot {
        *row = r.f32s::<3>()?;
    }
    let s = r.f32()?;
    r.i32()?; // collision object
    Ok(AvObject { name, flags, xf: Xf { t, r: rot, s } })
}

#[derive(Default, Clone)]
struct Geometry {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    colors: Vec<[u8; 4]>,
    triangles: Vec<[u16; 3]>,
}

/// Decode a packed SE vertex buffer described by `desc` (see `BSVertexDesc`).
fn decode_vertices(raw: &[u8], desc: u64, n: usize, g: &mut Geometry) -> Result<()> {
    const VF_VERTEX: u64 = 0x1;
    const VF_UV: u64 = 0x2;
    const VF_UV2: u64 = 0x4;
    const VF_NORMAL: u64 = 0x8;
    const VF_TANGENT: u64 = 0x10;
    const VF_COLORS: u64 = 0x20;
    const VF_SKINNED: u64 = 0x40;
    const VF_LANDDATA: u64 = 0x80;
    const VF_EYEDATA: u64 = 0x100;

    let size = ((desc & 0xF) * 4) as usize;
    let flags = (desc >> 44) & 0x3FF;
    let has = |f: u64| flags & f != 0;
    let off = |shift: u32| (((desc >> shift) & 0xF) * 4) as usize;
    // Sizes of what SE stores per vertex: float position plus a float, half UVs, byte normals...
    let expected: usize = [
        (VF_VERTEX, 16),
        (VF_UV, 4),
        (VF_UV2, 4),
        (VF_NORMAL, 4),
        (VF_TANGENT, 4),
        (VF_COLORS, 4),
        (VF_SKINNED, 12),
        (VF_LANDDATA, 4),
        (VF_EYEDATA, 4),
    ]
    .iter()
    .filter(|(f, _)| has(*f))
    .map(|(_, s)| s)
    .sum();
    ensure!(has(VF_VERTEX), "vertex format has no positions (flags {flags:#x})");
    ensure!(size == expected, "vertex size {size} doesn't match the format's {expected} (flags {flags:#x})");
    ensure!(n.checked_mul(size).is_some_and(|t| t <= raw.len()), "vertex data is cut off");
    let (o_pos, o_uv, o_normal, o_color) = (off(4), off(8), off(16), off(24));
    // Every attribute we read must lie inside the vertex, whatever the descriptor says.
    for (flag, offset, len) in [(VF_VERTEX, o_pos, 12), (VF_UV, o_uv, 4), (VF_NORMAL, o_normal, 3), (VF_COLORS, o_color, 4)] {
        ensure!(!has(flag) || offset + len <= size, "vertex attribute at {offset} doesn't fit a {size}-byte vertex");
    }
    for v in raw.chunks_exact(size).take(n) {
        let f = |o: usize| f32::from_le_bytes(v[o..o + 4].try_into().unwrap());
        g.positions.push([f(o_pos), f(o_pos + 4), f(o_pos + 8)]);
        if has(VF_UV) {
            let h = |o: usize| half(u16::from_le_bytes(v[o..o + 2].try_into().unwrap()));
            g.uvs.push([h(o_uv), h(o_uv + 2)]);
        }
        if has(VF_NORMAL) {
            let b = |o: usize| v[o] as f32 / 127.5 - 1.0;
            g.normals.push([b(o_normal), b(o_normal + 1), b(o_normal + 2)]);
        }
        if has(VF_COLORS) {
            g.colors.push(v[o_color..o_color + 4].try_into().unwrap());
        }
    }
    Ok(())
}

struct Shape {
    av: AvObject,
    skin: i32,
    shader: i32,
    alpha: i32,
    data: i32, // NiTriShapeData ref for old-style shapes
    own: Option<Geometry>,
}

fn bs_tri_shape(r: &mut Reader, strings: &[String]) -> Result<Shape> {
    let av = av_object(r, strings)?;
    r.skip(16)?; // bounding sphere
    let (skin, shader, alpha) = (r.i32()?, r.i32()?, r.i32()?);
    let desc = r.u64()?;
    let ntris = r.u16()? as usize;
    let nverts = r.u16()? as usize;
    let data_size = r.u32()? as usize;
    let mut own = None;
    if data_size > 0 {
        // The data size covers the vertices and then the triangles.
        let size = ((desc & 0xF) * 4) as usize;
        ensure!(
            nverts * size + ntris * 6 == data_size,
            "data is {data_size} bytes, expected {nverts} vertices x {size} + {ntris} triangles x 6"
        );
        let mut g = Geometry::default();
        decode_vertices(r.take(nverts * size)?, desc, nverts, &mut g)?;
        for _ in 0..ntris {
            g.triangles.push([r.u16()?, r.u16()?, r.u16()?]);
        }
        own = Some(g);
    }
    Ok(Shape { av, skin, shader, alpha, data: -1, own })
}

/// `NiTriShape` and friends: geometry is in a separate `NiTriShapeData`.
fn ni_tri_shape(r: &mut Reader, strings: &[String]) -> Result<Shape> {
    let av = av_object(r, strings)?;
    let data = r.i32()?;
    let skin = r.i32()?;
    let materials = r.u32()? as usize;
    r.fits(materials, 8)?;
    r.skip(materials * 8)?; // material names and extra data
    r.skip(4 + 1)?; // active material, needs-update flag
    let (shader, alpha) = (r.i32()?, r.i32()?);
    Ok(Shape { av, skin, shader, alpha, data, own: None })
}

fn ni_tri_shape_data(r: &mut Reader) -> Result<Geometry> {
    r.skip(4)?; // group id
    let n = r.u16()? as usize;
    r.skip(2)?; // keep and compress flags
    let mut g = Geometry::default();
    if r.u8()? != 0 {
        r.fits(n, 12)?;
        for _ in 0..n {
            g.positions.push(r.f32s::<3>()?);
        }
    }
    let bs_flags = r.u16()?;
    r.skip(4)?; // material CRC
    if r.u8()? != 0 {
        r.fits(n, 12)?;
        for _ in 0..n {
            g.normals.push(r.f32s::<3>()?);
        }
        if bs_flags & 0x1000 != 0 {
            r.skip(n.checked_mul(24).context("count overflow")?)?; // tangents and bitangents
        }
    }
    r.skip(16)?; // bounding sphere
    if r.u8()? != 0 {
        r.fits(n, 16)?;
        for _ in 0..n {
            let c = r.f32s::<4>()?;
            g.colors.push(c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8));
        }
    }
    if bs_flags & 1 != 0 {
        r.fits(n, 8)?;
        for _ in 0..n {
            g.uvs.push(r.f32s::<2>()?);
        }
    }
    r.skip(2 + 4)?; // consistency flags, additional data
    let ntris = r.u16()? as usize;
    r.skip(4)?; // number of triangle points
    if r.u8()? != 0 {
        r.fits(ntris, 6)?;
        for _ in 0..ntris {
            g.triangles.push([r.u16()?, r.u16()?, r.u16()?]);
        }
    }
    Ok(g)
}

/// Geometry of a skinned shape, from `NiSkinPartition`; must use the whole block.
fn skin_partition(r: &mut Reader) -> Result<Geometry> {
    let parts = r.u32()? as usize;
    let data_size = r.u32()? as usize;
    let vertex_size = r.u32()? as usize;
    let desc = r.u64()?;
    let mut g = Geometry::default();
    if data_size > 0 {
        ensure!(data_size.checked_rem(vertex_size) == Some(0), "vertex buffer size doesn't divide by the vertex size");
        let raw = r.take(data_size)?;
        // Face meshes (`BSDynamicTriShape`) keep positions in the shape itself, so their
        // partition's vertices have none; only the triangles are of use, and the shape is
        // reported as unsupported anyway.
        if desc >> 44 & 0x1 != 0 {
            decode_vertices(raw, desc, data_size / vertex_size, &mut g)?;
        }
    }
    r.fits(parts, 20)?;
    for _ in 0..parts {
        let nv = r.u16()? as usize;
        let nt = r.u16()? as usize;
        let nb = r.u16()? as usize;
        let ns = r.u16()? as usize;
        let nw = r.u16()? as usize;
        r.skip(nb * 2)?; // bones
        if r.u8()? != 0 {
            r.skip(nv * 2)?; // vertex map
        }
        if r.u8()? != 0 {
            r.skip(nv * nw * 4)?; // weights
        }
        r.fits(ns, 2)?;
        let strips: usize = (0..ns).map(|_| r.u16().map(|v| v as usize)).sum::<Result<_>>()?;
        if r.u8()? != 0 {
            r.skip(if ns > 0 { strips * 2 } else { nt * 6 })?; // local triangles or strips
        }
        if r.u8()? != 0 {
            r.skip(nv * nw)?; // bone indices
        }
        r.skip(1 + 1 + 8)?; // LOD level, global vertex buffer flag, vertex desc
        r.fits(nt, 6)?;
        for _ in 0..nt {
            g.triangles.push([r.u16()?, r.u16()?, r.u16()?]); // "triangles copy": indices into the shared buffer
        }
    }
    ensure!(r.left() == 0, "{} bytes left over", r.left());
    Ok(g)
}

struct Node {
    av: AvObject,
    children: Vec<i32>,
    /// For a switch node: the child that's shown.
    switch: Option<u32>,
}

fn node(r: &mut Reader, strings: &[String], switch: bool) -> Result<Node> {
    let av = av_object(r, strings)?;
    let children = r.refs()?;
    r.refs()?; // effects
    let switch = if switch {
        r.u16()?;
        Some(r.u32()?)
    } else {
        None
    };
    Ok(Node { av, children, switch })
}

enum Block {
    Node(Node),
    Shape(Shape),
    TriData(Geometry),
    SkinInstance { partition: i32, parts: Vec<u16> },
    SkinPartition(Geometry),
    Lighting { shader_type: u32, flags2: u32, texture_set: i32 },
    Effect { flags2: u32, texture: String },
    TextureSet(Vec<String>),
    Alpha(Alpha),
}

fn parse_block(ty: &str, data: &[u8], strings: &[String]) -> Result<Option<Block>> {
    let mut r = Reader::new(data);
    let r = &mut r;
    Ok(Some(match ty {
        "NiNode" | "BSFadeNode" | "BSLeafAnimNode" | "BSTreeNode" | "BSMultiBoundNode" | "BSOrderedNode" | "BSValueNode"
        | "BSBlastNode" | "BSDamageStage" | "BSDebrisNode" | "NiBillboardNode" => Block::Node(node(r, strings, false)?),
        "NiSwitchNode" => Block::Node(node(r, strings, true)?),
        "BSTriShape" | "BSSubIndexTriShape" | "BSMeshLODTriShape" => Block::Shape(bs_tri_shape(r, strings)?),
        "NiTriShape" => Block::Shape(ni_tri_shape(r, strings)?),
        "NiTriShapeData" => Block::TriData(ni_tri_shape_data(r)?),
        "NiSkinInstance" | "BSDismemberSkinInstance" => {
            r.i32()?; // NiSkinData
            let partition = r.i32()?;
            let mut parts = Vec::new();
            if ty == "BSDismemberSkinInstance" {
                r.i32()?; // skeleton root
                let bones = r.u32()? as usize;
                r.fits(bones, 4)?;
                r.skip(bones * 4)?;
                let n = r.u32()? as usize;
                r.fits(n, 4)?;
                for _ in 0..n {
                    r.u16()?; // partition flags
                    parts.push(r.u16()?);
                }
            }
            Block::SkinInstance { partition, parts }
        }
        "NiSkinPartition" => Block::SkinPartition(skin_partition(r)?),
        "BSLightingShaderProperty" => {
            let shader_type = r.u32()?;
            object_net(r, strings)?;
            r.u32()?; // shader flags 1
            let flags2 = r.u32()?;
            r.skip(16)?; // UV offset and scale
            Block::Lighting { shader_type, flags2, texture_set: r.i32()? }
        }
        "BSEffectShaderProperty" => {
            object_net(r, strings)?;
            r.u32()?;
            let flags2 = r.u32()?;
            r.skip(16)?;
            Block::Effect { flags2, texture: r.sized()? }
        }
        "BSShaderTextureSet" => {
            let n = r.u32()? as usize;
            r.fits(n, 4)?;
            Block::TextureSet((0..n).map(|_| r.sized()).collect::<Result<_>>()?)
        }
        "NiAlphaProperty" => {
            object_net(r, strings)?;
            let flags = r.u16()?;
            Block::Alpha(Alpha { blend: flags & 1 != 0, test: flags & 0x200 != 0, threshold: r.u8()? })
        }
        _ => return Ok(None),
    }))
}

/// Block types that name geometry or hierarchy but that this reader doesn't handle.
fn looks_visible(ty: &str) -> bool {
    !ty.starts_with("bhk")
        && (ty.ends_with("TriShape") || ty.ends_with("TriStrips") || ty.ends_with("Node") || ty.contains("Particle") || ty.ends_with("Geometry"))
}

// -------------------------------------------------------------------- file

/// Parse a Skyrim SE model. Fails for files that aren't one, and with
/// [`LegacyFormat`] (see [`anyhow::Error::downcast_ref`]) for Skyrim LE
/// models; problems inside single blocks become [`Nif::warnings`].
pub fn parse(bytes: &[u8]) -> Result<Nif> {
    let nl = bytes.iter().take(128).position(|&c| c == b'\n').context("no Gamebryo header line")?;
    ensure!(bytes.starts_with(b"Gamebryo File Format") || bytes.starts_with(b"NetImmerse File Format"), "not a NIF file");
    let mut r = Reader::new(bytes);
    r.p = nl + 1;
    let version = r.u32()?;
    ensure!(version == VERSION_SSE, "NIF version {version:#x} isn't 20.2.0.7");
    ensure!(r.u8()? == 1, "big-endian NIFs aren't supported");
    let user_version = r.u32()?;
    let block_count = r.u32()? as usize;
    if user_version < 3 {
        return Err(LegacyFormat { bs_version: 0 }.into());
    }
    let bs_version = r.u32()?;
    if bs_version != BS_SSE {
        return Err(LegacyFormat { bs_version }.into());
    }
    r.short()?; // author
    r.short()?; // process script
    r.short()?; // export script

    let type_count = r.u16()? as usize;
    r.fits(type_count, 4)?;
    let types: Vec<String> = (0..type_count).map(|_| r.sized()).collect::<Result<_>>()?;
    // Per block: a type index (2 bytes) and a size (4 bytes).
    r.fits(block_count, 6)?;
    let type_of: Vec<usize> = (0..block_count).map(|_| r.u16().map(|v| (v & 0x7fff) as usize)).collect::<Result<_>>()?;
    let sizes: Vec<usize> = (0..block_count).map(|_| r.u32().map(|v| v as usize)).collect::<Result<_>>()?;
    let string_count = r.u32()? as usize;
    r.u32()?; // longest string
    r.fits(string_count, 4)?;
    let strings: Vec<String> = (0..string_count).map(|_| r.sized()).collect::<Result<_>>()?;
    let group_count = r.u32()? as usize;
    r.fits(group_count, 4)?;
    r.skip(group_count * 4)?;
    ensure!(
        sizes.iter().try_fold(0usize, |a, &s| a.checked_add(s)).is_some_and(|t| t <= r.left()),
        "blocks are larger than the file"
    );

    let mut nif = Nif::default();
    let mut blocks: Vec<Option<Block>> = Vec::with_capacity(block_count);
    let mut unsupported = HashSet::new();
    for i in 0..block_count {
        let ty = types.get(type_of[i]).map(String::as_str).context("block has an unknown type index")?;
        let data = r.take(sizes[i])?;
        match parse_block(ty, data, &strings) {
            Ok(b) => {
                if b.is_none() && looks_visible(ty) {
                    unsupported.insert(ty.to_string());
                }
                blocks.push(b);
            }
            Err(e) => {
                nif.warnings.push(format!("block {i} ({ty}): {e:#}"));
                blocks.push(None);
            }
        }
    }
    nif.unsupported = unsupported.into_iter().collect();
    nif.unsupported.sort();
    assemble(&blocks, &mut nif);
    Ok(nif)
}

fn assemble(blocks: &[Option<Block>], nif: &mut Nif) {
    let get = |i: i32| usize::try_from(i).ok().and_then(|i| blocks.get(i)).and_then(Option::as_ref);

    // Roots: nodes and shapes nobody lists as a child.
    let mut is_child = HashSet::new();
    for b in blocks.iter().flatten() {
        if let Block::Node(n) = b {
            is_child.extend(n.children.iter().copied().filter(|&c| c >= 0).map(|c| c as usize));
        }
    }
    let mut stack: Vec<(usize, Xf)> = blocks
        .iter()
        .enumerate()
        .filter(|(i, b)| matches!(b, Some(Block::Node(_) | Block::Shape(_))) && !is_child.contains(i))
        .map(|(i, _)| (i, Xf::IDENT))
        .rev()
        .collect();

    let mut seen = HashSet::new();
    while let Some((i, parent)) = stack.pop() {
        if !seen.insert(i) {
            continue; // a cycle or a shared child: take it once
        }
        match blocks.get(i).and_then(Option::as_ref) {
            Some(Block::Node(n)) => {
                let world = parent.then(&n.av.xf);
                let kids: Vec<i32> = match n.switch {
                    Some(k) => n.children.get(k as usize).copied().into_iter().collect(),
                    None => n.children.clone(),
                };
                for &c in kids.iter().rev().filter(|&&c| c >= 0) {
                    stack.push((c as usize, world));
                }
            }
            Some(Block::Shape(s)) => {
                match build_mesh(s, &parent, &get) {
                    Ok(m) => nif.meshes.push(m),
                    Err(e) => nif.warnings.push(format!("shape '{}' (block {i}): {e}", s.av.name)),
                }
            }
            _ => {}
        }
    }
}

fn build_mesh<'a>(s: &Shape, parent: &Xf, get: &impl Fn(i32) -> Option<&'a Block>) -> Result<Mesh> {
    let skinned = s.skin >= 0;
    let body_parts = match get(s.skin) {
        Some(Block::SkinInstance { parts, .. }) => parts.clone(),
        _ => Vec::new(),
    };
    let geometry = if let Some(g) = &s.own {
        Some(g.clone())
    } else if skinned {
        match get(s.skin) {
            Some(Block::SkinInstance { partition, .. }) => match get(*partition) {
                Some(Block::SkinPartition(g)) => Some(g.clone()),
                _ => None,
            },
            _ => None,
        }
    } else {
        None
    };
    let geometry = geometry
        .or_else(|| match get(s.data) {
            Some(Block::TriData(g)) => Some(g.clone()),
            _ => None,
        })
        .context(if skinned { "skinned, but its skin partition couldn't be read" } else { "no geometry" })?;
    // A shape with indices pointing outside its vertices is broken; drawing it would panic callers.
    let n = geometry.positions.len();
    ensure!(n > 0, "no vertices");
    ensure!(!geometry.triangles.iter().flatten().any(|&i| i as usize >= n), "a triangle uses a vertex that doesn't exist");
    let mut m = Mesh {
        name: s.av.name.clone(),
        positions: geometry.positions,
        normals: if geometry.normals.len() == n { geometry.normals } else { vec![] },
        uvs: if geometry.uvs.len() == n { geometry.uvs } else { vec![] },
        colors: if geometry.colors.len() == n { geometry.colors } else { vec![] },
        triangles: geometry.triangles,
        hidden: s.av.flags & 1 != 0,
        skinned,
        body_parts,
        ..Mesh::default()
    };
    // Every shape's transform applies, skinned or not: gloves, hands and capes are stored around
    // their own origin and placed by the shape's translation (61 of 440 skinned shapes in a
    // typical install have one); armour shapes mostly have an identity transform.
    let world = parent.then(&s.av.xf);
    if !world.is_ident() {
        for p in &mut m.positions {
            *p = world.point(*p);
        }
        for v in &mut m.normals {
            *v = world.rotate(*v);
        }
    }
    match get(s.shader) {
        Some(Block::Lighting { shader_type, flags2, texture_set }) => {
            m.shader_type = Some(*shader_type);
            m.double_sided = flags2 & 0x10 != 0;
            if let Some(Block::TextureSet(t)) = get(*texture_set) {
                m.textures = t.clone();
            }
        }
        Some(Block::Effect { flags2, texture }) => {
            m.double_sided = flags2 & 0x10 != 0;
            m.textures = vec![texture.clone()];
        }
        _ => {}
    }
    if let Some(Block::Alpha(a)) = get(s.alpha) {
        m.alpha = Some(*a);
    }
    Ok(m)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // ---- a small NIF writer, enough for the shapes the tests need

    fn s32(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn u32le(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn f32le(out: &mut Vec<u8>, v: f32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    /// A whole file: `strings` is the string table, `blocks` are `(type, bytes)`.
    pub(crate) fn file(bs: u32, strings: &[&str], blocks: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut types: Vec<&str> = vec![];
        for (t, _) in blocks {
            if !types.contains(t) {
                types.push(t);
            }
        }
        let mut o = b"Gamebryo File Format, Version 20.2.0.7\n".to_vec();
        u32le(&mut o, VERSION_SSE);
        o.push(1);
        u32le(&mut o, 12);
        u32le(&mut o, blocks.len() as u32);
        u32le(&mut o, bs);
        o.extend_from_slice(&[1, 0, 1, 0, 1, 0]); // three empty short strings
        o.extend_from_slice(&(types.len() as u16).to_le_bytes());
        for t in &types {
            s32(&mut o, t);
        }
        for (t, _) in blocks {
            o.extend_from_slice(&(types.iter().position(|x| x == t).unwrap() as u16).to_le_bytes());
        }
        for (_, b) in blocks {
            u32le(&mut o, b.len() as u32);
        }
        u32le(&mut o, strings.len() as u32);
        u32le(&mut o, strings.iter().map(|s| s.len()).max().unwrap_or(0) as u32);
        for s in strings {
            s32(&mut o, s);
        }
        u32le(&mut o, 0);
        for (_, b) in blocks {
            o.extend_from_slice(b);
        }
        o
    }

    /// `NiAVObject`: name string index, translation, scale; no rotation.
    fn av(name: i32, t: [f32; 3], scale: f32, flags: u32) -> Vec<u8> {
        let mut o = vec![];
        o.extend_from_slice(&name.to_le_bytes());
        u32le(&mut o, 0); // extra data
        o.extend_from_slice(&(-1i32).to_le_bytes()); // controller
        u32le(&mut o, flags);
        for v in t {
            f32le(&mut o, v);
        }
        for v in [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
            f32le(&mut o, v);
        }
        f32le(&mut o, scale);
        o.extend_from_slice(&(-1i32).to_le_bytes()); // collision
        o
    }

    pub(crate) fn ni_node(name: i32, t: [f32; 3], children: &[i32], switch: Option<u32>) -> Vec<u8> {
        let mut o = av(name, t, 1.0, 0);
        u32le(&mut o, children.len() as u32);
        for c in children {
            o.extend_from_slice(&c.to_le_bytes());
        }
        u32le(&mut o, 0); // effects
        if let Some(i) = switch {
            o.extend_from_slice(&[0, 0]);
            u32le(&mut o, i);
        }
        o
    }

    /// Format: float position (+ bitangent x), half UV, byte normal (+ bitangent y). 24 bytes.
    const DESC: u64 = 6 | (4 << 8) | (5 << 16) | (0xB << 44);

    fn vertex_bytes(verts: &[([f32; 3], [u16; 2], [u8; 3])]) -> Vec<u8> {
        let mut o = vec![];
        for (p, uv, n) in verts {
            for v in p {
                f32le(&mut o, *v);
            }
            f32le(&mut o, 0.0);
            for h in uv {
                o.extend_from_slice(&h.to_le_bytes());
            }
            o.extend_from_slice(&[n[0], n[1], n[2], 128]);
        }
        o
    }

    const TRI: [([f32; 3], [u16; 2], [u8; 3]); 3] = [
        ([0.0, 0.0, 0.0], [0x0000, 0x0000], [128, 128, 255]),
        ([1.0, 0.0, 0.0], [0x3C00, 0x0000], [128, 128, 255]),
        ([0.0, 1.0, 0.0], [0x0000, 0x3800], [128, 128, 255]),
    ];

    pub(crate) fn bs_tri_shape_block(name: i32, t: [f32; 3], skin: i32, shader: i32, alpha: i32, with_data: bool) -> Vec<u8> {
        let mut o = av(name, t, 1.0, 0);
        o.extend_from_slice(&[0u8; 16]);
        for r in [skin, shader, alpha] {
            o.extend_from_slice(&r.to_le_bytes());
        }
        o.extend_from_slice(&DESC.to_le_bytes());
        o.extend_from_slice(&1u16.to_le_bytes()); // triangles
        o.extend_from_slice(&3u16.to_le_bytes()); // vertices
        if with_data {
            let v = vertex_bytes(&TRI);
            u32le(&mut o, v.len() as u32 + 6); // vertices, then the one triangle
            o.extend_from_slice(&v);
            for i in [0u16, 1, 2] {
                o.extend_from_slice(&i.to_le_bytes());
            }
        } else {
            u32le(&mut o, 0);
        }
        o
    }

    fn texture_set(paths: &[&str]) -> Vec<u8> {
        let mut o = vec![];
        u32le(&mut o, paths.len() as u32);
        for p in paths {
            s32(&mut o, p);
        }
        o
    }

    fn lighting(shader_type: u32, flags2: u32, texture_set: i32) -> Vec<u8> {
        let mut o = vec![];
        u32le(&mut o, shader_type);
        o.extend_from_slice(&(-1i32).to_le_bytes());
        u32le(&mut o, 0);
        o.extend_from_slice(&(-1i32).to_le_bytes());
        u32le(&mut o, 0);
        u32le(&mut o, flags2);
        o.extend_from_slice(&[0u8; 16]);
        o.extend_from_slice(&texture_set.to_le_bytes());
        o.extend_from_slice(&[0u8; 40]); // the rest of the material, unread
        o
    }

    fn alpha(flags: u16, threshold: u8) -> Vec<u8> {
        let mut o = vec![];
        o.extend_from_slice(&(-1i32).to_le_bytes());
        u32le(&mut o, 0);
        o.extend_from_slice(&(-1i32).to_le_bytes());
        o.extend_from_slice(&flags.to_le_bytes());
        o.push(threshold);
        o
    }

    #[test]
    fn reads_a_shape_with_material_and_baked_transforms() {
        let strings = ["Root", "Tri"];
        let bytes = file(
            100,
            &strings,
            &[
                ("NiNode", ni_node(0, [10.0, 0.0, 0.0], &[1], None)),
                ("BSTriShape", bs_tri_shape_block(1, [0.0, 5.0, 0.0], -1, 2, 4, true)),
                ("BSLightingShaderProperty", lighting(5, 0x10, 3)),
                ("BSShaderTextureSet", texture_set(&["textures\\a\\Diffuse.dds", "textures\\a\\normal.dds"])),
                ("NiAlphaProperty", alpha(0x1 | 0x200, 128)),
            ],
        );
        let nif = parse(&bytes).unwrap();
        assert!(nif.warnings.is_empty(), "{:?}", nif.warnings);
        assert_eq!(nif.meshes.len(), 1);
        let m = &nif.meshes[0];
        assert_eq!(m.name, "Tri");
        // Root translates by x+10, the shape by y+5.
        assert_eq!(m.positions, [[10.0, 5.0, 0.0], [11.0, 5.0, 0.0], [10.0, 6.0, 0.0]]);
        assert_eq!(m.triangles, [[0, 1, 2]]);
        assert_eq!(m.uvs, [[0.0, 0.0], [1.0, 0.0], [0.0, 0.5]]);
        assert!(m.normals.iter().all(|n| (n[2] - 1.0).abs() < 0.01 && n[0].abs() < 0.01));
        assert_eq!(m.texture(slot::DIFFUSE), Some("textures\\a\\Diffuse.dds"));
        assert_eq!(m.texture(slot::NORMAL), Some("textures\\a\\normal.dds"));
        assert_eq!(m.texture(slot::SPECULAR), None);
        assert_eq!(m.shader_type, Some(5));
        assert!(m.double_sided && !m.skinned && !m.hidden);
        assert_eq!(m.alpha, Some(Alpha { blend: true, test: true, threshold: 128 }));
        assert_eq!(nif.bounds(), Some(([10.0, 5.0, 0.0], [11.0, 6.0, 0.0])));
    }

    #[test]
    fn skinned_shape_takes_its_geometry_from_the_skin_partition() {
        let mut partition = vec![];
        u32le(&mut partition, 1); // partitions
        let verts = vertex_bytes(&TRI);
        u32le(&mut partition, verts.len() as u32);
        u32le(&mut partition, 24);
        partition.extend_from_slice(&DESC.to_le_bytes());
        partition.extend_from_slice(&verts);
        for v in [3u16, 1, 0, 0, 0] {
            partition.extend_from_slice(&v.to_le_bytes()); // vertices, triangles, bones, strips, weights per vertex
        }
        partition.extend_from_slice(&[0, 0]); // no vertex map, no weights
        partition.push(1); // has faces
        for i in [0u16, 1, 2] {
            partition.extend_from_slice(&i.to_le_bytes());
        }
        partition.push(0); // no bone indices
        partition.extend_from_slice(&[0, 0]);
        partition.extend_from_slice(&0u64.to_le_bytes());
        for i in [0u16, 1, 2] {
            partition.extend_from_slice(&i.to_le_bytes()); // triangles copy
        }
        let mut instance = vec![];
        instance.extend_from_slice(&(-1i32).to_le_bytes());
        instance.extend_from_slice(&2i32.to_le_bytes()); // partition = block 2
        instance.extend_from_slice(&(-1i32).to_le_bytes());
        u32le(&mut instance, 0); // bones
        u32le(&mut instance, 2); // skin partitions: (flags, body part)
        for (flags, part) in [(0x101u16, 54u16), (0x101, 37)] {
            instance.extend_from_slice(&flags.to_le_bytes());
            instance.extend_from_slice(&part.to_le_bytes());
        }
        let bytes = file(
            100,
            &["Root", "Body"],
            &[
                ("NiNode", ni_node(0, [100.0, 0.0, 0.0], &[3], None)),
                ("BSDismemberSkinInstance", instance),
                ("NiSkinPartition", partition),
                ("BSTriShape", bs_tri_shape_block(1, [0.0, 0.0, 0.0], 1, -1, -1, false)),
            ],
        );
        let nif = parse(&bytes).unwrap();
        assert!(nif.warnings.is_empty(), "{:?}", nif.warnings);
        let m = &nif.meshes[0];
        assert!(m.skinned);
        assert_eq!(m.body_parts, [54, 37], "the skin partitions' slots are read");
        // The root's offset applies to skinned shapes too, as the shape's own transform does.
        assert_eq!(m.positions[1], [101.0, 0.0, 0.0]);
        assert_eq!(m.triangles, [[0, 1, 2]]);
    }

    #[test]
    fn old_style_ni_tri_shape_with_data() {
        let mut shape = av(0, [0.0; 3], 2.0, 1); // scale 2, hidden
        shape.extend_from_slice(&1i32.to_le_bytes()); // data
        shape.extend_from_slice(&(-1i32).to_le_bytes()); // skin
        u32le(&mut shape, 0); // materials
        shape.extend_from_slice(&(-1i32).to_le_bytes());
        shape.push(0);
        shape.extend_from_slice(&(-1i32).to_le_bytes()); // shader
        shape.extend_from_slice(&(-1i32).to_le_bytes()); // alpha
        let mut data = vec![];
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&3u16.to_le_bytes());
        data.extend_from_slice(&[0, 0, 1]); // keep, compress, has vertices
        for v in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for c in v {
                f32le(&mut data, c);
            }
        }
        data.extend_from_slice(&1u16.to_le_bytes()); // BS flags: one UV set
        u32le(&mut data, 0); // material crc
        data.push(0); // no normals
        data.extend_from_slice(&[0u8; 16]); // bounding sphere
        data.push(0); // no colours
        for uv in [[0.0f32, 0.0], [1.0, 0.0], [0.0, 1.0]] {
            for c in uv {
                f32le(&mut data, c);
            }
        }
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&(-1i32).to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes()); // triangles
        u32le(&mut data, 3);
        data.push(1);
        for i in [0u16, 2, 1] {
            data.extend_from_slice(&i.to_le_bytes());
        }
        data.extend_from_slice(&0u16.to_le_bytes()); // match groups
        let bytes = file(100, &["Old"], &[("NiTriShape", shape), ("NiTriShapeData", data)]);
        let nif = parse(&bytes).unwrap();
        assert!(nif.warnings.is_empty(), "{:?}", nif.warnings);
        let m = &nif.meshes[0];
        assert_eq!(m.positions[1], [2.0, 0.0, 0.0], "scale is baked in");
        assert_eq!(m.triangles, [[0, 2, 1]]);
        assert_eq!(m.uvs.len(), 3);
        assert!(m.hidden);
    }

    #[test]
    fn switch_node_shows_only_the_chosen_child() {
        let bytes = file(
            100,
            &["Switch", "A", "B"],
            &[
                ("NiSwitchNode", ni_node(0, [0.0; 3], &[1, 2], Some(1))),
                ("BSTriShape", bs_tri_shape_block(1, [0.0; 3], -1, -1, -1, true)),
                ("BSTriShape", bs_tri_shape_block(2, [0.0; 3], -1, -1, -1, true)),
            ],
        );
        let nif = parse(&bytes).unwrap();
        assert_eq!(nif.meshes.len(), 1);
        assert_eq!(nif.meshes[0].name, "B");
    }

    #[test]
    fn legacy_and_foreign_files_are_recognised_not_parsed() {
        let le = file(83, &[], &[("NiNode", ni_node(-1, [0.0; 3], &[], None))]);
        let e = parse(&le).unwrap_err();
        assert_eq!(e.downcast_ref::<LegacyFormat>(), Some(&LegacyFormat { bs_version: 83 }));
        assert!(e.to_string().contains("Skyrim LE"));
        assert!(parse(b"not a nif at all").is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn a_bad_block_is_a_warning_and_the_rest_still_loads() {
        let mut bad = bs_tri_shape_block(1, [0.0; 3], -1, -1, -1, true);
        // Corrupt the vertex descriptor so the layout doesn't add up.
        let desc_at = av(1, [0.0; 3], 1.0, 0).len() + 16 + 12;
        bad[desc_at + 5] = 0xF0;
        let bytes = file(
            100,
            &["Root", "Bad", "Good"],
            &[
                ("NiNode", ni_node(0, [0.0; 3], &[1, 2], None)),
                ("BSTriShape", bad),
                ("BSTriShape", bs_tri_shape_block(2, [0.0; 3], -1, -1, -1, true)),
                ("BSDynamicTriShape", vec![0; 8]),
                ("bhkRigidBody", vec![0; 8]),
            ],
        );
        let nif = parse(&bytes).unwrap();
        assert_eq!(nif.meshes.len(), 1);
        assert_eq!(nif.meshes[0].name, "Good");
        assert_eq!(nif.warnings.len(), 1, "{:?}", nif.warnings);
        assert!(nif.warnings[0].contains("block 1 (BSTriShape)"));
        assert_eq!(nif.unsupported, ["BSDynamicTriShape"], "collision isn't visible geometry");
    }

    #[test]
    fn out_of_range_triangles_drop_the_shape() {
        let mut shape = bs_tri_shape_block(0, [0.0; 3], -1, -1, -1, true);
        let n = shape.len();
        shape[n - 2..].copy_from_slice(&9u16.to_le_bytes()); // last index 9, but 3 vertices
        let nif = parse(&file(100, &["X"], &[("BSTriShape", shape)])).unwrap();
        assert!(nif.meshes.is_empty());
        assert!(nif.warnings[0].contains("vertex that doesn't exist"), "{:?}", nif.warnings);
    }

    #[test]
    fn child_cycles_terminate() {
        let bytes = file(100, &["A", "B"], &[("NiNode", ni_node(0, [0.0; 3], &[1], None)), ("NiNode", ni_node(1, [0.0; 3], &[0], None))]);
        let nif = parse(&bytes).unwrap();
        assert!(nif.meshes.is_empty());
    }

    /// Whatever is done to a valid file, parsing must return, not panic or allocate wildly.
    #[test]
    fn damaged_files_never_panic() {
        let bytes = file(
            100,
            &["Root", "Tri"],
            &[
                ("NiNode", ni_node(0, [0.0; 3], &[1], None)),
                ("BSTriShape", bs_tri_shape_block(1, [0.0; 3], -1, 2, 4, true)),
                ("BSLightingShaderProperty", lighting(0, 0, 3)),
                ("BSShaderTextureSet", texture_set(&["a.dds"])),
                ("NiAlphaProperty", alpha(0, 0)),
            ],
        );
        for cut in 0..bytes.len() {
            let _ = parse(&bytes[..cut]);
        }
        for i in 0..bytes.len() {
            for v in [0x00u8, 0xFF, 0x7F, 0x80] {
                let mut b = bytes.clone();
                b[i] = v;
                let _ = parse(&b);
            }
        }
    }

    /// Read-only sweep of a real install: every `.nif`, loose and in archives.
    /// Skipped unless `SKYPLUG_TEST_GAME_DIR` is set (see the crate README).
    #[test]
    fn real_install_meshes() {
        use crate::steam::{DEFAULT_APPID, GameInstall};
        use crate::GamePaths;
        let Some(dir) = std::env::var_os("SKYPLUG_TEST_GAME_DIR") else {
            eprintln!("SKYPLUG_TEST_GAME_DIR not set; skipping");
            return;
        };
        let prefix = std::env::var_os("SKYPLUG_TEST_PREFIX").map(std::path::PathBuf::from);
        let paths = GamePaths::new(&GameInstall::new(std::path::PathBuf::from(dir), prefix).unwrap(), DEFAULT_APPID);
        let resolver = paths.asset_resolver().unwrap();

        let mut files: Vec<(String, Vec<u8>)> = vec![];
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("nif")) {
                    out.push((format!("loose {}", p.display()), std::fs::read(&p).unwrap()));
                }
            }
        }
        walk(&paths.data_dir.join(resolver_dir_name(&paths)), &mut files);
        for a in resolver.archives() {
            for (i, e) in a.entries().iter().enumerate() {
                if e.path.to_ascii_lowercase().ends_with(".nif") {
                    files.push((format!("{} in {}", e.path, a.path().file_name().unwrap().to_string_lossy()), a.read(i).unwrap()));
                }
            }
        }

        let (mut ok, mut legacy, mut failed, mut meshes, mut skinned, mut warned) = (0, 0, 0, 0, 0, 0);
        let mut warnings: std::collections::BTreeMap<String, (usize, String)> = Default::default();
        let mut unsupported: std::collections::BTreeMap<String, usize> = Default::default();
        let mut failures: Vec<String> = vec![];
        let mut empty = 0;
        for (name, bytes) in &files {
            match parse(bytes) {
                Ok(n) => {
                    ok += 1;
                    meshes += n.meshes.len();
                    skinned += n.meshes.iter().filter(|m| m.skinned).count();
                    if n.meshes.is_empty() {
                        empty += 1;
                    }
                    if !n.warnings.is_empty() {
                        warned += 1;
                    }
                    for w in &n.warnings {
                        // Group by the message without block numbers.
                        let key: String = w.split_once(": ").map_or(w.as_str(), |x| x.1).chars().take(70).collect();
                        let e = warnings.entry(key).or_insert((0, name.clone()));
                        e.0 += 1;
                    }
                    for u in n.unsupported {
                        *unsupported.entry(u).or_default() += 1;
                    }
                }
                Err(e) if e.downcast_ref::<LegacyFormat>().is_some() => legacy += 1,
                Err(e) => {
                    failed += 1;
                    failures.push(format!("{name}: {e:#}"));
                }
            }
        }
        eprintln!("{} files: {ok} parsed ({meshes} meshes, {skinned} skinned, {empty} with no mesh, {warned} with warnings), {legacy} LE, {failed} failed", files.len());
        for (k, (n, example)) in &warnings {
            eprintln!("  warning x{n}: {k}   e.g. {example}");
        }
        for (k, n) in &unsupported {
            eprintln!("  unsupported x{n}: {k}");
        }
        for f in failures.iter().take(10) {
            eprintln!("  FAILED {f}");
        }
        assert_eq!(failed, 0);
    }

    fn resolver_dir_name(paths: &crate::GamePaths) -> std::path::PathBuf {
        // The meshes folder as spelled on disk.
        std::fs::read_dir(&paths.data_dir)
            .into_iter()
            .flatten()
            .flatten()
            .find(|e| e.file_name().eq_ignore_ascii_case("meshes"))
            .map(|e| e.file_name().into())
            .unwrap_or_else(|| "meshes".into())
    }
}
