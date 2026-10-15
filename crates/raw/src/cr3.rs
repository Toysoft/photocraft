//! Bounded CR3 camera-rendered JPEG/HEVC previews and container metadata.
//! No CRX sensor payload is interpreted or decoded.

use crate::preview::{Preview, candidate};
use crate::tiff::{Tiff, tag};
use crate::{Limits, RawError};

#[path = "cr3/heif.rs"]
mod heif;
#[path = "cr3/metadata.rs"]
mod metadata;
pub use metadata::Metadata;

const META_UUID: &[u8] = &[0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48];
const PREVIEW_UUID: &[u8] = &[0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16];
const MAX_BOXES: usize = 8192;
const MAX_DEPTH: usize = 12;

fn u32_at(b: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(p..p.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(b: &[u8], p: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(p..p.checked_add(8)?)?.try_into().ok()?))
}

struct BoxRef<'a> {
    kind: &'a [u8],
    body: &'a [u8],
}

/// Walks only complete sibling boxes, including extended lengths and a final
/// zero-length box. Invalid lengths end this container; siblings never escape it.
fn boxes<'a>(mut b: &'a [u8], budget: &mut usize, mut f: impl FnMut(BoxRef<'a>, &mut usize)) {
    while b.len() >= 8 && *budget > 0 {
        *budget -= 1;
        let Some(n) = u32_at(b, 0) else {
            return;
        };
        let (size, header) = match n {
            0 => (b.len(), 8),
            1 => {
                let Some(size) = u64_at(b, 8).and_then(|n| usize::try_from(n).ok()) else {
                    return;
                };
                (size, 16)
            }
            n => (n as usize, 8),
        };
        if size < header {
            return;
        }
        let Some(body) = b.get(header..size) else {
            return;
        };
        let Some(kind) = b.get(4..8) else {
            return;
        };
        f(BoxRef { kind, body }, budget);
        let Some(rest) = b.get(size..) else {
            return;
        };
        b = rest;
    }
}

#[derive(Default)]
struct Track<'a> {
    jpeg: bool,
    sizes: Option<&'a [u8]>,
    offsets: Option<(&'a [u8], bool)>,
    mapping: bool,
    hevc: Option<&'a [u8]>,
    timed: bool,
}

/// Accept only a single JPEG sample description, with CRAW's fixed sample-entry
/// header followed by an explicit JPEG codec box.
fn jpeg_description(b: &[u8], budget: &mut usize) -> bool {
    if u32_at(b, 0) != Some(0) || u32_at(b, 4) != Some(1) {
        return false;
    }
    let mut jpeg = false;
    if let Some(entries) = b.get(8..) {
        boxes(entries, budget, |entry, budget| {
            if entry.kind == b"CRAW"
                && let Some(children) = entry.body.get(82..)
            {
                boxes(children, budget, |codec, _| {
                    jpeg |= codec.kind == b"JPEG";
                });
            }
        });
    }
    jpeg
}

fn track_boxes<'a>(b: &'a [u8], depth: usize, budget: &mut usize, t: &mut Track<'a>) {
    if depth > MAX_DEPTH {
        return;
    }
    boxes(b, budget, |item, budget| match item.kind {
        b"mdia" | b"minf" | b"stbl" => track_boxes(item.body, depth + 1, budget, t),
        b"stsd" => {
            t.jpeg = jpeg_description(item.body, budget);
            if u32_at(item.body, 0) == Some(0)
                && u32_at(item.body, 4) == Some(1)
                && let Some(entries) = item.body.get(8..)
            {
                boxes(entries, budget, |entry, budget| {
                    t.timed |= entry.kind == b"CTMD";
                    if entry.kind == b"CRAW"
                        && let Some(children) = entry.body.get(82..)
                    {
                        boxes(children, budget, |codec, _| {
                            if codec.kind == b"HEVC" {
                                t.hevc = Some(codec.body);
                            }
                        });
                    }
                });
            }
        }
        b"stsz" => t.sizes = Some(item.body),
        b"co64" => t.offsets = Some((item.body, true)),
        b"stco" => t.offsets = Some((item.body, false)),
        // One entry, starting at chunk 1, one sample per chunk, description 1.
        b"stsc" => t.mapping = item.body.len() == 20 && (0..5).all(|i| u32_at(item.body, i * 4) == Some(if i == 0 { 0 } else { 1 })),
        _ => {}
    });
}

fn track_preview<'a>(raw: &'a [u8], track: &'a [u8], budget: &mut usize, media: &[(usize, usize)]) -> Option<Preview<'a>> {
    let mut t = Track::default();
    track_boxes(track, 0, budget, &mut t);
    if !t.jpeg || !t.mapping {
        return None;
    }
    let (off, len) = sample_range(&t, media)?;
    candidate(raw, off, len)
}

fn sample_range(t: &Track<'_>, media: &[(usize, usize)]) -> Option<(usize, usize)> {
    if !t.mapping {
        return None;
    }
    let sizes = t.sizes?;
    let (offsets, wide) = t.offsets?;
    // Burst sequences are deliberately not paired by ordinal index: their
    // sample-to-chunk mapping and picture selection need a separate implementation.
    if u32_at(sizes, 0) != Some(0) || u32_at(sizes, 8) != Some(1) || u32_at(offsets, 0) != Some(0) || u32_at(offsets, 4) != Some(1) {
        return None;
    }
    let fixed = u32_at(sizes, 4)?;
    let len = if fixed == 0 { u32_at(sizes, 12)? } else { fixed };
    let off = if wide { u64_at(offsets, 8)? } else { u64::from(u32_at(offsets, 8)?) };
    let off = usize::try_from(off).ok()?;
    let end = off.checked_add(len as usize)?;
    if !media.iter().any(|(start, stop)| off >= *start && end <= *stop) {
        return None;
    }
    Some((off, len as usize))
}

/// A camera-rendered HEVC preview. This is not CRX sensor data.
/// Borrowed coded bytes remain separate from the optional HEIF decoder.
#[derive(Debug, Clone)]
pub struct HevcPreview<'a> {
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub rows: u16,
    pub columns: u16,
    /// HEVC decoder configuration record, without its box header.
    pub configuration: &'a [u8],
    /// Length-prefixed HEVC NAL units for the grid's tiles.
    pub sample: &'a [u8],
    /// ISO/IEC 23091-2 primaries, transfer and matrix codes, plus full-range flag.
    pub color: [u16; 3],
    pub full_range: bool,
    pub bits: u8,
}

impl HevcPreview<'_> {
    /// Builds a standard HEIF still-image container for the existing optional decoder.
    /// The CR3 sample's original depth, colour declaration and grid crop are preserved.
    pub fn to_heif(&self, limits: &Limits) -> Result<Vec<u8>, RawError> {
        heif::container(self, limits)
    }
}

fn u16_at(b: &[u8], p: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(p..p.checked_add(2)?)?.try_into().ok()?))
}

fn hevc_layout<'a>(body: &'a [u8], width: u32, height: u32, sample: &'a [u8], grid: bool) -> Option<HevcPreview<'a>> {
    let mut p = HevcPreview {
        width,
        height,
        tile_width: 0,
        tile_height: 0,
        rows: 1,
        columns: 1,
        configuration: &[],
        sample,
        color: [0; 3],
        full_range: false,
        bits: 0,
    };
    let mut budget = 64;
    boxes(body, &mut budget, |item, _| match item.kind {
        b"GRID" if grid && u32_at(item.body, 0) == Some(0) && item.body.get(8..10) == Some(&[0, 0]) => {
            if let (Some(rows), Some(cols), Some(w), Some(h)) = (item.body.get(10), item.body.get(11), u16_at(item.body, 12), u16_at(item.body, 14)) {
                p.rows = u16::from(*rows) + 1;
                p.columns = u16::from(*cols) + 1;
                p.width = u32::from(w);
                p.height = u32::from(h);
                if u32_at(item.body, 4) != Some(u32::from(p.rows) * u32::from(p.columns)) {
                    p.rows = 0;
                }
            }
        }
        b"CISZ" if u32_at(item.body, 0) == Some(0) => {
            p.tile_width = u32_at(item.body, 4).unwrap_or(0);
            p.tile_height = u32_at(item.body, 8).unwrap_or(0);
        }
        b"hvcC" => p.configuration = item.body,
        b"colr" if item.body.get(..4) == Some(b"nclx") => {
            if let (Some(a), Some(b), Some(c), Some(d)) = (u16_at(item.body, 4), u16_at(item.body, 6), u16_at(item.body, 8), item.body.get(10)) {
                p.color = [a, b, c];
                p.full_range = d & 0x80 != 0;
            }
        }
        b"pixi" if u32_at(item.body, 0) == Some(0) && item.body.get(4) == Some(&3) => {
            if let Some(bits) = item.body.get(5..8)
                && bits.iter().all(|b| Some(b) == bits.first())
            {
                p.bits = bits.first().copied().unwrap_or(0);
            }
        }
        _ => {}
    });
    (p.width > 0
        && p.height > 0
        && p.rows > 0
        && p.tile_width > 0
        && p.tile_height > 0
        && !p.configuration.is_empty()
        && p.color != [0; 3]
        && matches!(p.bits, 8 | 10))
    .then_some(p)
}

fn track_hevc<'a>(raw: &'a [u8], track: &'a [u8], budget: &mut usize, media: &[(usize, usize)]) -> Option<HevcPreview<'a>> {
    let mut t = Track::default();
    track_boxes(track, 0, budget, &mut t);
    let (off, len) = sample_range(&t, media)?;
    let body = t.hevc?.get(4..)?; // HEVC full box version 1.
    if u32_at(t.hevc?, 0) != Some(1) {
        return None;
    }
    hevc_layout(body, 0, 0, raw.get(off..off.checked_add(len)?)?, true)
}

fn prvw_hevc(body: &[u8]) -> Option<HevcPreview<'_>> {
    if u32_at(body, 0) != Some(0x0100_0000) || u16_at(body, 4) != Some(2) {
        return None;
    }
    let len = u32_at(body, 12)? as usize;
    let data = body.get(16..16usize.checked_add(len)?)?;
    // Version 1 starts with four property boxes followed by the length-prefixed bitstream.
    // Read only declared boxes, never search compressed data for signatures.
    let mut pos = 0usize;
    for kind in [b"CISZ", b"hvcC", b"colr", b"pixi"] {
        let n = u32_at(data, pos)? as usize;
        if n < 8 || data.get(pos.checked_add(4)?..pos.checked_add(8)?)? != kind {
            return None;
        }
        pos = pos.checked_add(n)?;
        data.get(..pos)?;
    }
    let coded = data.get(pos..)?;
    let size = u32_at(coded, 0)? as usize;
    if size < 8 || coded.get(4..8) != Some(b"IMGD") {
        return None;
    }
    let sample = coded.get(8..size)?;
    let trailing = coded.get(size..)?;
    if !trailing.is_empty() && (u32_at(trailing, 0)? as usize != trailing.len() || trailing.get(4..8) != Some(b"free")) {
        return None;
    }
    hevc_layout(data.get(..pos)?, u32::from(u16_at(body, 6)?), u32::from(u16_at(body, 8)?), sample, false)
}

#[derive(Default)]
struct Found<'a> {
    previews: Vec<Preview<'a>>,
    orientation: Option<u16>,
    media: Vec<(usize, usize)>,
    hevc: Vec<HevcPreview<'a>>,
    metadata: Metadata<'a>,
}

fn walk<'a>(raw: &'a [u8], b: &'a [u8], depth: usize, budget: &mut usize, found: &mut Found<'a>) {
    if depth > MAX_DEPTH {
        return;
    }
    boxes(b, budget, |item, budget| match item.kind {
        b"moov" => walk(raw, item.body, depth + 1, budget, found),
        b"trak" => {
            if found.previews.len() < 128 {
                found.previews.extend(track_preview(raw, item.body, budget, &found.media));
            }
            if found.hevc.len() < 128 {
                found.hevc.extend(track_hevc(raw, item.body, budget, &found.media));
            }
            let mut track = Track::default();
            track_boxes(item.body, 0, budget, &mut track);
            if track.timed
                && let Some((offset, len)) = sample_range(&track, &found.media)
            {
                found.metadata.read_timed(raw.get(offset..offset.saturating_add(len)).unwrap_or_default());
            }
        }
        b"uuid" => {
            if item.body.get(..16) == Some(&[0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf, 0xac]) {
                found.metadata.xmp = item
                    .body
                    .get(16..)
                    .filter(|b| b.len() <= metadata::MAX_METADATA)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .map(|s| s.trim_end_matches('\0'));
            }
            if item.body.get(..16) == Some(META_UUID)
                && let Some(children) = item.body.get(16..)
            {
                walk(raw, children, depth + 1, budget, found);
            } else if item.body.get(..16) == Some(PREVIEW_UUID)
                && let Some(children) = item.body.get(24..)
            {
                walk(raw, children, depth + 1, budget, found);
            }
        }
        b"PRVW" => {
            if found.hevc.len() < 128 {
                found.hevc.extend(prvw_hevc(item.body));
            }
            if found.previews.len() < 128
                && let Some(len) = u32_at(item.body, 12)
                && let Some(p) = candidate(item.body, 16, len as usize)
            {
                found.previews.push(p);
            }
        }
        b"CMT1" => {
            found.metadata.set(0, item.body);
            if let Some(tiff) = Tiff::new(item.body)
                && let Some(ifd) = tiff.ifd_at(tiff.first_ifd, 0)
                && let Some(o) = tiff.tag_uint(&ifd, tag::ORIENTATION).filter(|o| (1..=8).contains(o))
            {
                found.orientation = Some(o as u16);
            }
        }
        b"CMT2" => found.metadata.set(1, item.body),
        b"CMT3" => found.metadata.set(2, item.body),
        b"CMT4" => found.metadata.set(3, item.body),
        _ => {}
    });
}

fn read(raw: &[u8]) -> Found<'_> {
    let mut found = Found::default();
    if raw.get(4..12) == Some(b"ftypcrx ") {
        let mut budget = MAX_BOXES;
        boxes(raw, &mut budget, |item, _| {
            if item.kind == b"mdat" && found.media.len() < 128 {
                let start = (item.body.as_ptr() as usize).saturating_sub(raw.as_ptr() as usize);
                if let Some(end) = start.checked_add(item.body.len()) {
                    found.media.push((start, end));
                }
            }
        });
        walk(raw, raw, 0, &mut budget, &mut found);
    }
    found
}

pub(crate) fn previews(raw: &[u8]) -> Vec<Preview<'_>> {
    read(raw).previews
}
pub(crate) fn orientation(raw: &[u8]) -> Option<u16> {
    read(raw).orientation
}

/// All bounded HEVC preview candidates. The caller chooses a supported decoder.
pub fn hevc_previews(raw: &[u8]) -> Vec<HevcPreview<'_>> {
    read(raw).hevc
}

/// Camera metadata held by the CR3 container, even when its preview has no EXIF.
pub fn metadata(raw: &[u8]) -> Metadata<'_> {
    read(raw).metadata
}
