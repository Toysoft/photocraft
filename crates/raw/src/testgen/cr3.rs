//! ISO BMFF CR3 fixtures, with metadata independent of the JPEG.
use super::{TiffBuilder, Val};

pub fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [((body.len() + 8) as u32).to_be_bytes().as_slice(), kind, body].concat()
}
fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    bx(kind, &[&[0; 4][..], body].concat())
}
pub fn preview(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    with_metadata(jpeg, orientation, 1, "<x:xmpmeta xmlns:x='adobe:ns:meta/'/>")
}
pub fn with_metadata(jpeg: &[u8], orientation: u16, color: u16, xmp: &str) -> Vec<u8> {
    let tiff = |entries| {
        let mut t = TiffBuilder::default();
        let ifd = t.ifd(entries);
        t.chain = vec![ifd];
        t.build()
    };
    let cmt1 = tiff(vec![
        (271, Val::Ascii("Canon".into())),
        (272, Val::Ascii("Canon Synthetic CR3".into())),
        (274, Val::Short(vec![orientation])),
        (65000, Val::Undefined(vec![7, 9, 11, 13, 15])),
    ]);
    let cmt2 =
        tiff(vec![(34855, Val::Short(vec![800])), (33434, Val::Rational(vec![(1, 125)])), (40961, Val::Short(vec![if color == 1 { 1 } else { 65535 }]))]);
    let cmt3 = tiff(vec![(0xb4, Val::Short(vec![color])), (0x4001, Val::Short(vec![1, 2, 3, 4, 5]))]);
    let cmt4 = tiff(vec![(0, Val::Byte(vec![2, 3, 0, 0]))]);
    let mut metadata = vec![0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48];
    for (kind, body) in [(b"CMT1", cmt1), (b"CMT2", cmt2), (b"CMT3", cmt3), (b"CMT4", cmt4)] {
        metadata.extend(bx(kind, &body));
    }
    let mut xmp_body = vec![0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf, 0xac];
    xmp_body.extend_from_slice(xmp.as_bytes());
    let ftyp = bx(b"ftyp", b"crx \0\0\0\x01crx isom");
    let movie = |offset: u64| {
        let mut entry = vec![0; 82];
        entry.extend(bx(b"JPEG", &[0; 4]));
        let stsd = full(b"stsd", &[1u32.to_be_bytes().as_slice(), &bx(b"CRAW", &entry)].concat());
        let stsc = full(b"stsc", &[1u32; 4].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>());
        let stsz = full(b"stsz", &[(jpeg.len() as u32).to_be_bytes().as_slice(), 1u32.to_be_bytes().as_slice()].concat());
        let co64 = full(b"co64", &[1u32.to_be_bytes().as_slice(), offset.to_be_bytes().as_slice()].concat());
        let track = bx(b"trak", &bx(b"mdia", &bx(b"minf", &bx(b"stbl", &[stsd, stsc, stsz, co64].concat()))));
        bx(b"moov", &[bx(b"uuid", &metadata), track].concat())
    };
    let m = movie(0);
    let offset = (ftyp.len() + m.len() + bx(b"uuid", &xmp_body).len() + 8) as u64;
    [ftyp, movie(offset), bx(b"uuid", &xmp_body), bx(b"mdat", jpeg)].concat()
}

/// A single-cell HEVC PRVW with independently supplied coded bytes and metadata.
/// This only wraps a camera-preview bitstream; it never encodes sensor samples.
pub fn hevc_preview(p: &crate::cr3::HevcPreview<'_>, orientation: u16) -> Vec<u8> {
    let mut bytes = with_metadata(&[], orientation, 1, "<xmp preview='retained'/>");
    let size = [0u32.to_be_bytes(), p.tile_width.to_be_bytes(), p.tile_height.to_be_bytes()].concat();
    let mut color = b"nclx".to_vec();
    for code in p.color {
        color.extend(code.to_be_bytes());
    }
    color.push(if p.full_range { 128 } else { 0 });
    let properties = [bx(b"CISZ", &size), bx(b"hvcC", p.configuration), bx(b"colr", &color), bx(b"pixi", &[0, 0, 0, 0, 3, p.bits, p.bits, p.bits])].concat();
    let data = [properties, bx(b"IMGD", p.sample)].concat();
    let mut header = vec![0u8; 16];
    header[..4].copy_from_slice(&0x0100_0000u32.to_be_bytes());
    header[4..6].copy_from_slice(&2u16.to_be_bytes());
    header[6..8].copy_from_slice(&(p.width as u16).to_be_bytes());
    header[8..10].copy_from_slice(&(p.height as u16).to_be_bytes());
    header[12..16].copy_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend(bx(b"PRVW", &[header, data].concat()));
    bytes
}
