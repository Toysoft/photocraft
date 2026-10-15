//! Normalise Canon's HEVC preview grid into ISO/IEC 23008-12 item framing.
use super::HevcPreview;
use crate::{Limits, RawError};

fn malformed() -> RawError {
    RawError::malformed("invalid CR3 HEVC preview framing")
}

fn bx(kind: &[u8; 4], body: &[u8]) -> Result<Vec<u8>, RawError> {
    let n = body.len().checked_add(8).and_then(|n| u32::try_from(n).ok()).ok_or_else(malformed)?;
    Ok([n.to_be_bytes().as_slice(), kind, body].concat())
}
fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Result<Vec<u8>, RawError> {
    bx(kind, &[&[version, 0, 0, 0][..], body].concat())
}
fn nals<'a>(p: &HevcPreview<'a>, count: usize) -> Result<Vec<&'a [u8]>, RawError> {
    let width = usize::from(p.configuration.get(21).ok_or_else(malformed)? & 3) + 1;
    let mut rest = p.sample;
    let mut tiles = Vec::new();
    while !rest.is_empty() && tiles.len() < count {
        // Canon prefixes each grid cell with a 32-bit byte count; the cell itself
        // contains hvcC-length-prefixed NAL units, possibly several intra slices.
        let head = rest.get(..4).ok_or_else(malformed)?;
        let n = head.iter().fold(0usize, |n, b| n.saturating_mul(256).saturating_add(usize::from(*b)));
        let end = 4usize.checked_add(n).ok_or_else(malformed)?;
        let tile = rest.get(4..end).ok_or_else(malformed)?;
        let mut coded = tile;
        let mut slices = 0usize;
        while !coded.is_empty() && slices < 4096 {
            let size = coded.get(..width).ok_or_else(malformed)?.iter().fold(0usize, |n, b| n.saturating_mul(256).saturating_add(usize::from(*b)));
            let stop = width.checked_add(size).ok_or_else(malformed)?;
            let nal = coded.get(width..stop).ok_or_else(malformed)?;
            let typ = nal.first().map(|b| (b >> 1) & 63).ok_or_else(malformed)?;
            let first = nal.get(2).ok_or_else(malformed)? & 128 != 0;
            if !(16..=23).contains(&typ) || first != (slices == 0) {
                return Err(RawError::unsupported("CR3 HEVC preview requires one intra picture per grid cell"));
            }
            slices += 1;
            coded = coded.get(stop..).ok_or_else(malformed)?;
        }
        if slices == 0 || !coded.is_empty() {
            return Err(malformed());
        }
        tiles.push(tile);
        rest = rest.get(end..).ok_or_else(malformed)?;
    }
    if tiles.len() != count || !rest.is_empty() {
        return Err(malformed());
    }
    Ok(tiles)
}

pub(super) fn container(p: &HevcPreview<'_>, limits: &Limits) -> Result<Vec<u8>, RawError> {
    limits.check(u64::from(p.width), u64::from(p.height), 6)?;
    limits.check(u64::from(p.tile_width), u64::from(p.tile_height), 6)?;
    if !matches!(p.bits, 8 | 10) || p.configuration.first() != Some(&1) || p.configuration.len() < 23 {
        return Err(malformed());
    }
    let count = usize::from(p.rows).checked_mul(usize::from(p.columns)).ok_or_else(malformed)?;
    let covered_w = u64::from(p.columns) * u64::from(p.tile_width);
    let covered_h = u64::from(p.rows) * u64::from(p.tile_height);
    if count == 0
        || count > 256
        || p.rows > 256
        || p.columns > 256
        || p.width > u32::from(u16::MAX)
        || p.height > u32::from(u16::MAX)
        || covered_w < u64::from(p.width)
        || covered_h < u64::from(p.height)
        || covered_w.saturating_sub(u64::from(p.tile_width)) >= u64::from(p.width)
        || covered_h.saturating_sub(u64::from(p.tile_height)) >= u64::from(p.height)
    {
        return Err(malformed());
    }
    // Account for the temporary idat/meta/final containers together, before copying any data.
    if p.configuration.len() > 65536 || (p.sample.len() as u64).saturating_add(65536).saturating_mul(4) > limits.max_alloc {
        return Err(RawError::LimitExceeded("CR3 HEVC preview container working memory".into()));
    }
    let tiles = nals(p, count)?;
    let mut idat = vec![0, 0, (p.rows - 1) as u8, (p.columns - 1) as u8];
    idat.extend_from_slice(&(p.width as u16).to_be_bytes());
    idat.extend_from_slice(&(p.height as u16).to_be_bytes());
    let mut extents = vec![(0u32, 8u32)];
    for tile in tiles {
        let offset = u32::try_from(idat.len()).map_err(|_| malformed())?;
        let len = u32::try_from(tile.len()).map_err(|_| malformed())?;
        extents.push((offset, len));
        idat.extend_from_slice(tile);
    }
    let mut iinf = ((count + 1) as u16).to_be_bytes().to_vec();
    let mut iloc = vec![0x44, 0];
    iloc.extend_from_slice(&((count + 1) as u16).to_be_bytes());
    let mut associations = ((count + 1) as u32).to_be_bytes().to_vec();
    for (index, (offset, len)) in extents.iter().enumerate() {
        let id = (index + 1) as u16;
        let mut info = id.to_be_bytes().to_vec();
        info.extend_from_slice(&[0, 0]);
        info.extend_from_slice(if index == 0 { b"grid" } else { b"hvc1" });
        info.push(0);
        iinf.extend(full(b"infe", 2, &info)?);
        iloc.extend_from_slice(&id.to_be_bytes());
        iloc.extend_from_slice(&[0, 1, 0, 0, 0, 1]);
        iloc.extend_from_slice(&offset.to_be_bytes());
        iloc.extend_from_slice(&len.to_be_bytes());
        associations.extend_from_slice(&id.to_be_bytes());
        associations.extend_from_slice(if index == 0 { &[3, 0x85, 3, 4][..] } else { &[4, 0x81, 0x82, 3, 4][..] });
    }
    let mut ispe = p.tile_width.to_be_bytes().to_vec();
    ispe.extend_from_slice(&p.tile_height.to_be_bytes());
    let mut grid_size = p.width.to_be_bytes().to_vec();
    grid_size.extend_from_slice(&p.height.to_be_bytes());
    let mut color = b"nclx".to_vec();
    for code in p.color {
        color.extend_from_slice(&code.to_be_bytes());
    }
    color.push(if p.full_range { 128 } else { 0 });
    let properties = [
        bx(b"hvcC", p.configuration)?,
        full(b"ispe", 0, &ispe)?,
        full(b"pixi", 0, &[3, p.bits, p.bits, p.bits])?,
        bx(b"colr", &color)?,
        full(b"ispe", 0, &grid_size)?,
    ]
    .concat();
    let iprp = bx(b"iprp", &[bx(b"ipco", &properties)?, full(b"ipma", 0, &associations)?].concat())?;
    let mut dimg = vec![0, 1];
    dimg.extend_from_slice(&(count as u16).to_be_bytes());
    for id in 2..=(count + 1) as u16 {
        dimg.extend_from_slice(&id.to_be_bytes());
    }
    let hdlr = full(b"hdlr", 0, &[&[0; 4][..], b"pict", &[0; 13]].concat())?;
    let meta = full(
        b"meta",
        0,
        &[
            hdlr,
            full(b"pitm", 0, &[0, 1])?,
            full(b"iinf", 0, &iinf)?,
            full(b"iloc", 1, &iloc)?,
            iprp,
            full(b"iref", 0, &bx(b"dimg", &dimg)?)?,
            bx(b"idat", &idat)?,
        ]
        .concat(),
    )?;
    Ok([bx(b"ftyp", b"heic\0\0\0\0mif1heic")?, meta].concat())
}
