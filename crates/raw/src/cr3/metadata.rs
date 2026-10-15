//! Reframe independent Canon TIFF metadata blocks as a normal EXIF TIFF.
//! Values are relocated, not copied with stale offsets. Unknown scalar tags survive.
use crate::tiff::{Ifd, Tiff, type_size};
use crate::{Limits, RawError};
pub(super) const MAX_METADATA: usize = 1 << 20;

#[derive(Debug, Clone, Copy, Default)]
pub struct Metadata<'a> {
    /// Original CMT1–4 TIFF blocks (camera, EXIF, maker note, GPS).
    pub blocks: [Option<&'a [u8]>; 4],
    /// Original XMP packet, including unknown properties.
    pub xmp: Option<&'a str>,
    /// Per-frame Canon maker TIFF from the bounded CTMD sample (levels and white balance).
    pub timed_maker: Option<&'a [u8]>,
    pub timed_makers: [Option<&'a [u8]>; 3],
    pub timed_exif: Option<&'a [u8]>,
    pub timed: Option<&'a [u8]>,
}
impl<'a> Metadata<'a> {
    pub(super) fn read_timed(&mut self, mut bytes: &'a [u8]) {
        if bytes.len() > MAX_METADATA {
            return;
        }
        self.timed = Some(bytes);
        let le32 = |b: &[u8], at: usize| b.get(at..at.checked_add(4)?).and_then(|v| v.try_into().ok()).map(u32::from_le_bytes);
        for _ in 0..64 {
            if bytes.is_empty() {
                return;
            }
            let Some(size) = le32(bytes, 0).map(|v| v as usize).filter(|v| *v >= 12) else {
                return;
            };
            let Some(record) = bytes.get(..size) else {
                return;
            };
            if matches!(record.get(4..6), Some([7..=9, 0]))
                && record.get(6..8) == Some(&[1, 1])
                && record.get(9) == Some(&1)
                && record.get(8).is_some_and(|v| *v <= 2)
            {
                let mut blocks = record.get(12..).unwrap_or_default();
                for _ in 0..64 {
                    if blocks.is_empty() {
                        break;
                    }
                    let Some(n) = le32(blocks, 0).map(|v| v as usize).filter(|v| *v >= 8) else {
                        return;
                    };
                    let Some(data) = blocks.get(8..n) else {
                        return;
                    };
                    if le32(blocks, 4) == Some(0x927c)
                        && let Some(t) = Tiff::new(data)
                    {
                        if let Some(slot) = record.get(4).and_then(|v| self.timed_makers.get_mut(usize::from(*v) - 7)) {
                            *slot = Some(data);
                        }
                        if t.ifd_at(t.first_ifd, 0).is_some_and(|i| i.has(0x4001)) {
                            self.timed_maker = Some(data);
                        }
                    }
                    if le32(blocks, 4) == Some(0x8769) && Tiff::new(data).is_some() {
                        self.timed_exif = Some(data);
                    }
                    blocks = blocks.get(n..).unwrap_or_default();
                }
            }
            bytes = bytes.get(size..).unwrap_or_default();
        }
    }
    /// Camera preview colour declaration: 1 = sRGB, 2 = Adobe RGB, otherwise unknown.
    /// Canon documents maker tag 0x00b4; EXIF ColorSpace=65535 alone is not Adobe RGB.
    pub fn color_space(&self) -> Option<u16> {
        if let Some(t) = self.blocks.get(1).copied().flatten().and_then(Tiff::new)
            && let Some(ifd) = t.ifd_at(t.first_ifd, 0)
            && t.tag_uint(&ifd, 40961) == Some(1)
        {
            return Some(1);
        }
        let t = self.blocks.get(2).copied().flatten().and_then(Tiff::new)?;
        let ifd = t.ifd_at(t.first_ifd, 0)?;
        t.tag_uint(&ifd, 0xb4).filter(|v| matches!(v, 1 | 2)).map(|v| v as u16)
    }
    pub(super) fn set(&mut self, index: usize, body: &'a [u8]) {
        if body.len() <= MAX_METADATA
            && Tiff::new(body).is_some()
            && let Some(slot) = self.blocks.get_mut(index)
        {
            *slot = Some(body);
        }
    }
    /// Rebuilds a bounded little-endian EXIF TIFF, retaining Canon maker data and GPS.
    pub fn exif(&self, limits: &Limits) -> Result<Option<Vec<u8>>, RawError> {
        let Some(root) = self.blocks.first().copied().flatten().and_then(Tiff::new) else { return Ok(None) };
        let mut writer =
            Writer { bytes: b"II*\0\x08\0\0\0".to_vec(), max: limits.max_alloc.min((4 * MAX_METADATA) as u64) as usize, seen: Vec::new(), timed_exif: None };
        let root_ifd = root.ifd_at(root.first_ifd, 0).ok_or_else(bad)?;
        let mut extras = Vec::new();
        for (index, tag) in [(1usize, 34665u16), (3, 34853)] {
            if let Some(t) = self.blocks.get(index).copied().flatten().and_then(Tiff::new) {
                extras.push((tag, t, index));
            }
        }
        let makers: Vec<_> =
            self.blocks.get(2).copied().flatten().into_iter().chain(self.timed_makers.iter().copied().flatten()).filter_map(Tiff::new).collect();
        writer.timed_exif = self.timed_exif.and_then(Tiff::new);
        writer.ifd(&root, &root_ifd, 0, 0, &extras, (!makers.is_empty()).then_some(makers.as_slice()))?;
        Ok(Some(writer.bytes))
    }
}
fn bad() -> RawError {
    RawError::malformed("invalid CR3 TIFF metadata")
}
struct Writer<'a> {
    bytes: Vec<u8>,
    max: usize,
    seen: Vec<(usize, usize)>,
    timed_exif: Option<Tiff<'a>>,
}
impl Writer<'_> {
    fn append(&mut self, bytes: &[u8]) -> Result<usize, RawError> {
        let at = self.bytes.len();
        if at.checked_add(bytes.len()).is_none_or(|n| n > self.max) {
            return Err(RawError::LimitExceeded("CR3 metadata".into()));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(at)
    }
    fn patch(&mut self, at: usize, bytes: &[u8]) -> Result<(), RawError> {
        self.bytes.get_mut(at..at.checked_add(bytes.len()).ok_or_else(bad)?).ok_or_else(bad)?.copy_from_slice(bytes);
        Ok(())
    }
    fn ifd(
        &mut self,
        t: &Tiff<'_>,
        ifd: &Ifd,
        block: usize,
        depth: usize,
        extras: &[(u16, Tiff<'_>, usize)],
        maker: Option<&[Tiff<'_>]>,
    ) -> Result<usize, RawError> {
        if depth >= 8 || self.seen.contains(&(block, ifd.offset)) {
            return Err(bad());
        }
        self.seen.push((block, ifd.offset));
        let mut sources: Vec<_> = ifd.entries.iter().copied().map(|e| (*t, e, block)).collect();
        for (_, other, other_block) in extras.iter().filter(|e| e.0 == 0) {
            let child = other.ifd_at(other.first_ifd, 0).ok_or_else(bad)?;
            for e in child.entries {
                sources.retain(|(_, old, _)| old.tag != e.tag);
                sources.push((*other, e, *other_block));
            }
        }
        let has_maker = maker.is_some() && extras.iter().all(|e| e.0 == 0);
        sources.retain(|(_, e, _)| !extras.iter().any(|(tag, _, _)| *tag != 0 && *tag == e.tag));
        if has_maker {
            sources.retain(|(_, e, _)| e.tag != 37500);
        }
        let count =
            sources.len().checked_add(extras.iter().filter(|e| e.0 != 0).count()).and_then(|n| n.checked_add(usize::from(has_maker))).ok_or_else(bad)?;
        if count > 4096 {
            return Err(bad());
        }
        let table_len = 2usize.checked_add(count.checked_mul(12).ok_or_else(bad)?).and_then(|n| n.checked_add(4)).ok_or_else(bad)?;
        if table_len > self.max {
            return Err(bad());
        }
        let at = self.append(&vec![0; table_len])?;
        self.patch(at, &(count as u16).to_le_bytes())?;
        let mut entries = Vec::new();
        for (t, e, source_block) in sources {
            let size = type_size(e.typ).checked_mul(e.count as usize).ok_or_else(bad)?;
            if size == 0 || size > self.max {
                return Err(bad());
            }
            let mut data = t.bytes(e.at, size).ok_or_else(bad)?.to_vec();
            let unit = match e.typ {
                3 | 8 => 2,
                4 | 5 | 9 | 10 | 11 | 13 => 4,
                12 => 8,
                _ => 1,
            };
            if !t.le {
                for chunk in data.chunks_exact_mut(unit) {
                    chunk.reverse();
                }
            }
            if matches!(e.tag, 330 | 34665 | 34853 | 40965) || e.typ == 13 {
                if !matches!(e.typ, 4 | 13) || e.count > 64 {
                    return Err(bad());
                }
                for (i, word) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let offset = t.uint(&e, i).ok_or_else(bad)? as usize;
                    if offset != 0 {
                        let child = t.ifd_at(offset, 0).ok_or_else(bad)?;
                        let dest = self.ifd(&t, &child, source_block, depth + 1, &[], None)?;
                        word.copy_from_slice(&u32::try_from(dest).map_err(|_| bad())?.to_le_bytes());
                    }
                }
            }
            entries.push((e.tag, e.typ, e.count, data));
        }
        for (tag, other, other_block) in extras.iter().filter(|e| e.0 != 0) {
            let child = other.ifd_at(other.first_ifd, 0).ok_or_else(bad)?;
            let maker = if *tag == 34665 { maker } else { None };
            let overlay = if *tag == 34665 { self.timed_exif.map(|t| (0, t, 8)).into_iter().collect::<Vec<_>>() } else { Vec::new() };
            let dest = self.ifd(other, &child, *other_block, depth + 1, &overlay, maker)?;
            entries.push((*tag, 4, 1, u32::try_from(dest).map_err(|_| bad())?.to_le_bytes().to_vec()));
        }
        if has_maker && let Some(notes) = maker {
            let note = notes.first().ok_or_else(bad)?;
            let child = note.ifd_at(note.first_ifd, 0).ok_or_else(bad)?;
            let overlays: Vec<_> = notes.iter().copied().skip(1).enumerate().map(|(i, t)| (0, t, 4 + i)).collect();
            let dest = self.ifd(note, &child, 2, depth + 1, &overlays, None)?;
            let len = u32::try_from(self.bytes.len().saturating_sub(dest)).map_err(|_| bad())?;
            // Values in Canon maker notes are relative to the outer TIFF header.
            entries.push((37500, 7, len, u32::try_from(dest).map_err(|_| bad())?.to_le_bytes().to_vec()));
        }
        entries.sort_by_key(|e| e.0);
        // Root includes the maker-note only inside EXIF, not as a duplicate IFD0 tag.
        self.patch(at, &(entries.len() as u16).to_le_bytes())?;
        for (i, (tag, typ, count, data)) in entries.into_iter().enumerate() {
            let mut entry = tag.to_le_bytes().to_vec();
            entry.extend_from_slice(&typ.to_le_bytes());
            entry.extend_from_slice(&count.to_le_bytes());
            if has_maker && tag == 37500 && typ == 7 && count > 4 {
                entry.extend_from_slice(&data);
            } else if data.len() > 4 {
                let dest = self.append(&data)?;
                entry.extend_from_slice(&u32::try_from(dest).map_err(|_| bad())?.to_le_bytes());
            } else {
                entry.extend_from_slice(&data);
                entry.resize(12, 0);
            }
            self.patch(at + 2 + i * 12, &entry)?;
        }
        self.seen.pop();
        Ok(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{TiffBuilder, Val};
    fn tiff(entries: Vec<(u16, Val)>) -> Vec<u8> {
        let mut t = TiffBuilder::default();
        let id = t.ifd(entries);
        t.chain = vec![id];
        t.build()
    }
    #[test]
    fn timed_maker_notes_merge_unknown_values_and_relocate_color_data() {
        let root = tiff(vec![(271, Val::Ascii("Canon".into()))]);
        let exif = tiff(vec![(34855, Val::Short(vec![800]))]);
        let maker = tiff(vec![(0xb4, Val::Short(vec![1])), (65000, Val::Undefined(vec![1, 2, 3, 4, 5]))]);
        let timed = tiff(vec![(0x4001, Val::Short(vec![16, 100, 200, 300, 400])), (65001, Val::Ascii("unknown timed tag".into()))]);
        let block = [((timed.len() + 8) as u32).to_le_bytes().as_slice(), 0x927cu32.to_le_bytes().as_slice(), &timed].concat();
        let record = [((block.len() + 12) as u32).to_le_bytes().as_slice(), &[8, 0, 1, 1, 1, 1, 255, 255], &block].concat();
        let mut metadata = Metadata { blocks: [Some(&root), Some(&exif), Some(&maker), None], ..Default::default() };
        metadata.read_timed(&record);
        assert_eq!(metadata.timed_maker, Some(timed.as_slice()));
        let result = metadata.exif(&Limits::default()).unwrap().unwrap();
        let t = Tiff::new(&result).unwrap();
        let root = t.ifd_at(t.first_ifd, 0).unwrap();
        let exif = t.ifd_at(t.tag_uint(&root, 34665).unwrap() as usize, 0).unwrap();
        let note = t.ifd_at(exif.get(37500).unwrap().at, 0).unwrap();
        assert_eq!(t.tag_uint(&note, 0xb4), Some(1));
        assert_eq!(t.tag_uints(&note, 0x4001), [16, 100, 200, 300, 400]);
        assert_eq!(t.tag_ascii(&note, 65001).as_deref(), Some("unknown timed tag"));
        assert_eq!(t.raw(note.get(65000).unwrap()).unwrap(), [1, 2, 3, 4, 5]);
        for n in 0..record.len() {
            let mut m = Metadata::default();
            m.read_timed(&record[..n]);
            assert!(m.timed_maker.is_none());
        }
    }
    #[test]
    fn big_endian_values_and_cyclic_ifd_pointers_are_bounded() {
        let root = [b"MM\0*\0\0\0\x08".as_slice(), &[0, 1, 0xfe, 0xb0, 0, 3, 0, 0, 0, 1, 0, 7, 0, 0, 0, 0, 0, 0]].concat();
        let m = Metadata { blocks: [Some(&root), None, None, None], ..Default::default() };
        let b = m.exif(&Limits::default()).unwrap().unwrap();
        let t = Tiff::new(&b).unwrap();
        assert_eq!(t.tag_uint(&t.ifd_at(8, 0).unwrap(), 65200), Some(7));
        let root = tiff(vec![(34665, Val::Long(vec![8]))]);
        let m = Metadata { blocks: [Some(&root), None, None, None], ..Default::default() };
        assert!(matches!(m.exif(&Limits::default()), Err(RawError::Malformed(_))));
    }
}
