//! Decoding `.dds` textures to RGBA8, enough for what Skyrim SE ships and
//! its mods use: BC1 to BC5 and BC7 (legacy FourCC or DX10 header), and
//! uncompressed 8-, 24- and 32-bit formats.
//!
//! Decoding is per mip level, so a preview of a 4096x4096 texture can decode
//! a small level instead of the whole picture. Nothing here encodes or
//! writes DDS files.

use anyhow::{Result, bail, ensure};

const MAGIC: &[u8; 4] = b"DDS ";
const HEADER_LEN: usize = 124;
const DX10_LEN: usize = 20;

// Header flags / pixel format flags.
const PF_ALPHAPIXELS: u32 = 0x1;
const PF_ALPHA: u32 = 0x2;
const PF_FOURCC: u32 = 0x4;
const PF_RGB: u32 = 0x40;
const PF_LUMINANCE: u32 = 0x2_0000;

/// The storage format of a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Bc1,
    Bc2,
    Bc3,
    Bc4,
    Bc5,
    Bc7,
    /// Uncompressed with the given masks, `bytes` per pixel (1, 3 or 4).
    Rgba { bytes: usize, r: u32, g: u32, b: u32, a: u32 },
    /// One channel, shown as grey.
    Luminance8,
}

impl Format {
    /// Bytes per 4x4 block, for the block-compressed formats.
    fn block_bytes(self) -> Option<usize> {
        match self {
            Format::Bc1 | Format::Bc4 => Some(8),
            Format::Bc2 | Format::Bc3 | Format::Bc5 | Format::Bc7 => Some(16),
            _ => None,
        }
    }

    fn level_len(self, w: usize, h: usize) -> usize {
        match self.block_bytes() {
            Some(b) => w.div_ceil(4) * h.div_ceil(4) * b,
            None => match self {
                Format::Rgba { bytes, .. } => w * h * bytes,
                _ => w * h,
            },
        }
    }
}

/// A texture's header and where each mip level's bytes are. Borrows the file.
#[derive(Debug)]
pub struct Dds<'a> {
    pub width: usize,
    pub height: usize,
    pub format: Format,
    /// Number of mip levels stored (at least 1).
    pub mip_count: usize,
    /// True for a cube map; only its first face is decoded.
    pub cube_map: bool,
    data: &'a [u8],
}

/// A decoded picture: `width * height` pixels, 4 bytes each (R, G, B, A).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

impl<'a> Dds<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        ensure!(bytes.len() >= 4 + HEADER_LEN, "too short for a DDS header");
        ensure!(&bytes[0..4] == MAGIC, "not a DDS file (bad magic)");
        let h = &bytes[4..4 + HEADER_LEN];
        ensure!(u32_at(h, 0) == 124, "unexpected DDS header size {}", u32_at(h, 0));
        let (height, width) = (u32_at(h, 8) as usize, u32_at(h, 12) as usize);
        ensure!(width > 0 && height > 0, "texture has no size");
        ensure!(width <= 1 << 15 && height <= 1 << 15, "implausible size {width}x{height}");
        let mip_count = (u32_at(h, 24) as usize).max(1);
        ensure!(mip_count <= 16, "implausible mip count {mip_count}");
        let caps2 = u32_at(h, 108);
        let pf_flags = u32_at(h, 76);
        let fourcc = &h[80..84];
        let mut offset = 4 + HEADER_LEN;
        let mut cube_map = caps2 & 0x200 != 0;

        let format = if pf_flags & PF_FOURCC != 0 {
            match fourcc {
                b"DXT1" => Format::Bc1,
                b"DXT2" | b"DXT3" => Format::Bc2,
                b"DXT4" | b"DXT5" => Format::Bc3,
                b"ATI1" | b"BC4U" => Format::Bc4,
                b"ATI2" | b"BC5U" => Format::Bc5,
                b"DX10" => {
                    ensure!(bytes.len() >= offset + DX10_LEN, "DX10 header is cut off");
                    let dxgi = u32_at(bytes, offset);
                    let misc = u32_at(bytes, offset + 8);
                    let array = u32_at(bytes, offset + 12);
                    ensure!(array <= 1 || cube_map, "texture arrays aren't supported");
                    cube_map |= misc & 0x4 != 0;
                    offset += DX10_LEN;
                    match dxgi {
                        71 | 72 => Format::Bc1,
                        74 | 75 => Format::Bc2,
                        77 | 78 => Format::Bc3,
                        80 | 81 => Format::Bc4,
                        83 | 84 => Format::Bc5,
                        98 | 99 => Format::Bc7,
                        // B8G8R8A8 (UNORM / SRGB) and B8G8R8X8
                        87 | 91 => Format::Rgba { bytes: 4, r: 0x00FF_0000, g: 0x0000_FF00, b: 0x0000_00FF, a: 0xFF00_0000 },
                        88 | 93 => Format::Rgba { bytes: 4, r: 0x00FF_0000, g: 0x0000_FF00, b: 0x0000_00FF, a: 0 },
                        // R8G8B8A8 (UNORM / SRGB)
                        28 | 29 => Format::Rgba { bytes: 4, r: 0x0000_00FF, g: 0x0000_FF00, b: 0x00FF_0000, a: 0xFF00_0000 },
                        61 => Format::Luminance8,
                        other => bail!("DXGI format {other} isn't supported"),
                    }
                }
                other => bail!("DDS FourCC {:?} isn't supported", String::from_utf8_lossy(other)),
            }
        } else if pf_flags & PF_RGB != 0 {
            let bits = u32_at(h, 84);
            ensure!(bits == 24 || bits == 32, "{bits}-bit uncompressed DDS isn't supported");
            let a = if pf_flags & PF_ALPHAPIXELS != 0 { u32_at(h, 100) } else { 0 };
            Format::Rgba { bytes: bits as usize / 8, r: u32_at(h, 88), g: u32_at(h, 92), b: u32_at(h, 96), a }
        } else if pf_flags & (PF_LUMINANCE | PF_ALPHA) != 0 && u32_at(h, 84) == 8 {
            Format::Luminance8
        } else {
            bail!("unrecognised DDS pixel format (flags {pf_flags:#x})");
        };
        Ok(Dds { width, height, format, mip_count, cube_map, data: &bytes[offset..] })
    }

    /// Size of mip level `level` (0 is the full size).
    pub fn level_size(&self, level: usize) -> (usize, usize) {
        ((self.width >> level).max(1), (self.height >> level).max(1))
    }

    /// The smallest level whose longer side is at least `max_side`, or the
    /// last level if all are smaller. Decode that to get a picture of about
    /// the size wanted without decoding the whole texture.
    pub fn level_for(&self, max_side: usize) -> usize {
        (0..self.mip_count).rev().find(|&l| {
            let (w, h) = self.level_size(l);
            w.max(h) >= max_side
        }).unwrap_or(0)
    }

    /// Decode mip `level` to RGBA.
    pub fn decode(&self, level: usize) -> Result<Rgba> {
        ensure!(level < self.mip_count, "level {level} of {}", self.mip_count);
        let mut offset = 0;
        for l in 0..level {
            let (w, h) = self.level_size(l);
            offset += self.format.level_len(w, h);
        }
        let (w, h) = self.level_size(level);
        let len = self.format.level_len(w, h);
        ensure!(self.data.len() >= offset + len, "texture data is cut off (level {level})");
        let src = &self.data[offset..offset + len];
        let mut pixels = vec![0u8; w * h * 4];
        match self.format {
            Format::Rgba { bytes, r, g, b, a } => decode_masks(src, &mut pixels, bytes, [r, g, b, a]),
            Format::Luminance8 => {
                for (px, &v) in pixels.chunks_exact_mut(4).zip(src) {
                    px.copy_from_slice(&[v, v, v, 255]);
                }
            }
            f => decode_blocks(f, src, w, h, &mut pixels),
        }
        Ok(Rgba { width: w, height: h, pixels })
    }
}

fn decode_masks(src: &[u8], out: &mut [u8], bytes: usize, masks: [u32; 4]) {
    // For each channel: shift and the value that maps its max to 255.
    let ch = masks.map(|m| if m == 0 { (0u32, 0u32) } else { (m.trailing_zeros(), m >> m.trailing_zeros()) });
    for (px, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(bytes)) {
        let mut v = 0u32;
        for (i, &b) in s.iter().enumerate() {
            v |= (b as u32) << (8 * i);
        }
        for c in 0..4 {
            let (shift, max) = ch[c];
            px[c] = if masks[c] == 0 { 255 } else { (((v & masks[c]) >> shift) * 255 / max) as u8 };
        }
    }
}

fn decode_blocks(f: Format, src: &[u8], w: usize, h: usize, out: &mut [u8]) {
    let bb = f.block_bytes().unwrap();
    let blocks_x = w.div_ceil(4);
    // Each block decodes into a 4x4 scratch picture, then the part inside the image is copied out.
    let mut tile = [0u8; 4 * 4 * 4];
    let mut tile2 = [0u8; 4 * 4 * 2]; // BC5: two channels
    let mut tile1 = [0u8; 4 * 4]; // BC4: one channel
    for (i, block) in src.chunks_exact(bb).enumerate() {
        let (bx, by) = ((i % blocks_x) * 4, (i / blocks_x) * 4);
        match f {
            Format::Bc1 => bcdec_rs::bc1(block, &mut tile, 16),
            Format::Bc2 => bcdec_rs::bc2(block, &mut tile, 16),
            Format::Bc3 => bcdec_rs::bc3(block, &mut tile, 16),
            Format::Bc7 => bcdec_rs::bc7(block, &mut tile, 16),
            Format::Bc4 => {
                bcdec_rs::bc4(block, &mut tile1, 4, false);
                for (t, &v) in tile.chunks_exact_mut(4).zip(&tile1) {
                    t.copy_from_slice(&[v, v, v, 255]);
                }
            }
            Format::Bc5 => {
                // Two channels: normal map X and Y. Rebuild Z so it can be shown or lit.
                bcdec_rs::bc5(block, &mut tile2, 8, false);
                for (t, xy) in tile.chunks_exact_mut(4).zip(tile2.chunks_exact(2)) {
                    let (x, y) = (xy[0] as f32 / 127.5 - 1.0, xy[1] as f32 / 127.5 - 1.0);
                    let z = (1.0 - x * x - y * y).max(0.0).sqrt();
                    t.copy_from_slice(&[xy[0], xy[1], ((z + 1.0) * 127.5) as u8, 255]);
                }
            }
            _ => unreachable!(),
        }
        for ty in 0..4.min(h - by) {
            for tx in 0..4.min(w - bx) {
                let d = ((by + ty) * w + bx + tx) * 4;
                out[d..d + 4].copy_from_slice(&tile[(ty * 4 + tx) * 4..(ty * 4 + tx) * 4 + 4]);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A DDS with a legacy FourCC (or `DX10` plus `dxgi`) and the given mip data.
    pub(crate) fn make(w: u32, h: u32, mips: u32, fourcc: &[u8; 4], dxgi: Option<u32>, data: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 4 + HEADER_LEN];
        b[0..4].copy_from_slice(MAGIC);
        let mut set = |o: usize, v: u32| b[4 + o..4 + o + 4].copy_from_slice(&v.to_le_bytes());
        set(0, 124);
        set(8, h);
        set(12, w);
        set(24, mips);
        set(72, 32);
        set(76, PF_FOURCC);
        b[4 + 80..4 + 84].copy_from_slice(fourcc);
        if let Some(d) = dxgi {
            b.extend_from_slice(&d.to_le_bytes());
            b.extend_from_slice(&[0u8; 16]);
            b[4 + 80..4 + 84].copy_from_slice(b"DX10");
        }
        b.extend_from_slice(data);
        b
    }

    /// A `w` x `h` B8G8R8A8 texture of one colour (given as B, G, R, A).
    pub(crate) fn make_bgra(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        let data: Vec<u8> = bgra.iter().copied().cycle().take((w * h * 4) as usize).collect();
        make(w, h, 1, b"DX10", Some(87), &data)
    }

    fn solid(img: &Rgba) -> Option<[u8; 4]> {
        let first: [u8; 4] = img.pixels[0..4].try_into().unwrap();
        img.pixels.chunks_exact(4).all(|p| p == first).then_some(first)
    }

    #[test]
    fn bc1_block_is_one_colour() {
        // color0 = pure red (565 0xF800), color1 = black, all indices 0 -> color0.
        let block = [0x00, 0xF8, 0, 0, 0, 0, 0, 0];
        let d = make(4, 4, 1, b"DXT1", None, &block);
        let img = Dds::parse(&d).unwrap().decode(0).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        assert_eq!(solid(&img), Some([255, 0, 0, 255]));
    }

    #[test]
    fn bc3_uses_its_alpha_block() {
        // alpha0 = alpha1 = 128, indices 0; colour as in bc1.
        let mut block = vec![128u8, 128, 0, 0, 0, 0, 0, 0];
        block.extend_from_slice(&[0x00, 0xF8, 0, 0, 0, 0, 0, 0]);
        let d = make(4, 4, 1, b"DXT5", None, &block);
        let img = Dds::parse(&d).unwrap().decode(0).unwrap();
        assert_eq!(solid(&img), Some([255, 0, 0, 128]));
    }

    #[test]
    fn non_multiple_of_four_sizes_and_mips() {
        // 6x2 BC1: 2x1 blocks at level 0, then 3x1 -> 1 block, then 1x1 -> 1 block.
        let red = [0x00, 0xF8, 0, 0, 0, 0, 0, 0];
        let green = [0xE0, 0x07, 0, 0, 0, 0, 0, 0];
        let blue = [0x1F, 0x00, 0, 0, 0, 0, 0, 0];
        let data: Vec<u8> = [red, red, green, blue].concat();
        let d = make(6, 2, 3, b"DXT1", None, &data);
        let dds = Dds::parse(&d).unwrap();
        assert_eq!(dds.mip_count, 3);
        let l0 = dds.decode(0).unwrap();
        assert_eq!((l0.width, l0.height, l0.pixels.len()), (6, 2, 6 * 2 * 4));
        assert_eq!(&l0.pixels[0..4], &[255, 0, 0, 255]);
        let l1 = dds.decode(1).unwrap();
        assert_eq!((l1.width, l1.height), (3, 1));
        assert_eq!(solid(&l1), Some([0, 255, 0, 255]));
        assert_eq!(solid(&dds.decode(2).unwrap()), Some([0, 0, 255, 255]));
        assert!(dds.decode(3).is_err());
        assert_eq!(dds.level_for(6), 0);
        assert_eq!(dds.level_for(2), 1);
        assert_eq!(dds.level_for(1), 2);
        assert_eq!(dds.level_for(100), 0);
    }

    #[test]
    fn dx10_bgra_and_cut_off_data() {
        // 2x1 B8G8R8A8: bytes are B, G, R, A.
        let d = make(2, 1, 1, b"DX10", Some(87), &[10, 20, 30, 40, 50, 60, 70, 80]);
        let img = Dds::parse(&d).unwrap().decode(0).unwrap();
        assert_eq!(img.pixels, [30, 20, 10, 40, 70, 60, 50, 80]);
        let cut = make(2, 1, 1, b"DX10", Some(87), &[10, 20, 30]);
        assert!(Dds::parse(&cut).unwrap().decode(0).is_err());
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        assert!(Dds::parse(b"nope").is_err());
        assert!(Dds::parse(&make(4, 4, 1, b"DX10", Some(2), &[0; 64])).is_err()); // R32G32B32A32_FLOAT
        assert!(Dds::parse(&make(4, 4, 1, b"ABCD", None, &[0; 64])).is_err());
        let mut d = make(4, 4, 1, b"DXT1", None, &[0; 8]);
        d[4 + 12..4 + 16].copy_from_slice(&0u32.to_le_bytes()); // width 0
        assert!(Dds::parse(&d).is_err());
    }

    #[test]
    fn bc7_block_decodes_without_panicking() {
        // Mode 6 (one subset, 7-bit endpoints + p-bit): bit 6 set. All-zero tail = black, alpha 0.
        let mut block = [0u8; 16];
        block[0] = 1 << 6;
        let d = make(4, 4, 1, b"DX10", Some(98), &block);
        let img = Dds::parse(&d).unwrap().decode(0).unwrap();
        assert_eq!(img.pixels.len(), 4 * 4 * 4);
    }
}
