//! External PSD smart-object records. Parsing only resolves names; it never reads files.
use super::{Rd, split_items};
use photocraft_doc::Metadata;
use photocraft_psd::descriptor::{Descriptor, UnicodeString, Value, VersionedDescriptor};

#[derive(Clone, Debug, PartialEq)]
pub struct ExternalFile {
    pub uuid: String,
    pub file_name: String,
    pub path: String,
    pub relative_path: Option<String>,
    pub file_type: [u8; 4],
    pub file_size: u64,
    /// Unix modification time of the file whose appearance was cached; zero = unknown.
    pub modified: f64,
}

fn text(d: &Descriptor, key: &str) -> Option<String> {
    match d.get(key) {
        Some(Value::Text(s)) => Some(s.to_string_lossy()).filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn local_path(path: &str) -> Option<String> {
    let p = path.strip_prefix("file://").unwrap_or(path);
    if p.contains("://") || p.is_empty() {
        return None;
    }
    // Photoshop writes literal (unescaped) file URIs. Keep %, # and Unicode intact.
    let bytes = p.as_bytes();
    if bytes.first() == Some(&b'/') && bytes.get(2) == Some(&b':') { Some(p.get(1..)?.to_string()) } else { Some(p.to_string()) }
}

fn parse_item_positions(item: &[u8]) -> Option<(ExternalFile, usize, Option<usize>)> {
    let mut r = Rd { d: item, p: 0 };
    if r.take(4)? != b"liFE" {
        return None;
    }
    let version = r.u32()?;
    if !(1..=7).contains(&version) {
        return None;
    }
    let count = usize::from(r.u8()?);
    let uuid = String::from_utf8_lossy(r.take(count)?).to_string();
    let file_name = r.unicode()?;
    let file_type: [u8; 4] = r.take(4)?.try_into().ok()?;
    r.take(4)?;
    let data_len = usize::try_from(r.u64()?).ok()?;
    if r.u8()? != 0 {
        let (_, used) = VersionedDescriptor::parse_prefix(item.get(r.p..)?).ok()?;
        r.p = r.p.checked_add(used)?;
    }
    let (descriptor, used) = VersionedDescriptor::parse_prefix(item.get(r.p..)?).ok()?;
    r.p = r.p.checked_add(used)?;
    if version > 3 {
        r.take(16)?;
    }
    let size_at = r.p;
    let file_size = r.u64()?;
    if version > 2 {
        r.take(data_len)?;
    }
    if version >= 5 {
        r.unicode()?;
    }
    let modified_at = (version >= 6).then_some(r.p);
    let modified = if version >= 6 { f64::from_be_bytes(r.take(8)?.try_into().ok()?) } else { 0.0 };
    if !modified.is_finite() || modified < 0.0 {
        return None;
    }
    if version >= 7 {
        r.take(1)?;
    }
    if version == 2 {
        r.take(data_len)?;
    }
    let d = &descriptor.descriptor;
    let relative_path = text(d, "relPath").and_then(|s| local_path(&s));
    let path = text(d, "fullPath").or_else(|| text(d, "originalPath")).or_else(|| relative_path.clone())?;
    Some((ExternalFile { uuid, file_name, path: local_path(&path)?, relative_path, file_type, file_size, modified }, size_at, modified_at))
}

pub fn parse_external_files(data: &[u8]) -> Vec<ExternalFile> {
    split_items(data)
        .into_iter()
        .filter_map(|(_, item)| {
            let len = usize::try_from(u64::from_be_bytes(item.get(..8)?.try_into().ok()?)).ok()?;
            parse_item_positions(item.get(8..8usize.checked_add(len)?)?).map(|(f, _, _)| f)
        })
        .collect()
}

pub fn find_external_file(meta: &Metadata, uuid: &str) -> Option<ExternalFile> {
    meta.psd_global_blocks
        .iter()
        .filter(|(_, k, _)| matches!(k, b"lnkE" | b"lnk2" | b"lnk3" | b"lnkD"))
        .flat_map(|(_, _, d)| parse_external_files(d))
        .find(|f| f.uuid == uuid)
}

pub fn encode_external_file(f: &ExternalFile) -> Vec<u8> {
    let mut item = Vec::new();
    item.extend_from_slice(b"liFE");
    item.extend_from_slice(&7u32.to_be_bytes());
    let id = f.uuid.as_bytes().get(..f.uuid.len().min(255)).unwrap_or_default();
    item.push(id.len() as u8);
    item.extend_from_slice(id);
    let unicode = |out: &mut Vec<u8>, s: &str| {
        let units: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        out.extend_from_slice(&(units.len() as u32).to_be_bytes());
        for c in units {
            out.extend_from_slice(&c.to_be_bytes());
        }
    };
    unicode(&mut item, &f.file_name);
    item.extend_from_slice(&f.file_type);
    item.extend_from_slice(b"8BIM");
    item.extend_from_slice(&0u64.to_be_bytes());
    item.push(0);
    let path = f.path.replace('\\', "/");
    let uri = if path.starts_with('/') { format!("file://{path}") } else { format!("file:///{path}") };
    let mut d = Descriptor::new("ExternalFileLink")
        .with("descVersion", Value::Integer(2))
        .with("Nm  ", Value::Text(UnicodeString::new_nul(&f.file_name)))
        .with("fullPath", Value::Text(UnicodeString::new_nul(&uri)))
        .with("originalPath", Value::Text(UnicodeString::new_nul(&f.path)));
    if let Some(p) = &f.relative_path {
        d = d.with("relPath", Value::Text(UnicodeString::new_nul(p)));
    }
    item.extend(VersionedDescriptor::new(d).to_bytes());
    item.extend_from_slice(&1970u32.to_be_bytes());
    item.extend_from_slice(&[0, 1, 0, 0]);
    item.extend_from_slice(&0f64.to_be_bytes());
    item.extend_from_slice(&f.file_size.to_be_bytes());
    unicode(&mut item, "");
    item.extend_from_slice(&f.modified.to_be_bytes());
    item.push(0);
    let mut out = (item.len() as u64).to_be_bytes().to_vec();
    out.extend(item);
    out.resize(out.len().next_multiple_of(4), 0);
    out
}

/// Update only the cache stamp, preserving unknown descriptors and cached data byte-for-byte.
pub fn update_external_stamp(meta: &mut Metadata, path: &str, size: u64, modified: f64) {
    if !modified.is_finite() || modified < 0.0 {
        return;
    }
    for (_, key, data) in &mut meta.psd_global_blocks {
        if !matches!(&*key, b"lnkE" | b"lnk2" | b"lnk3" | b"lnkD") {
            continue;
        }
        let mut changed = false;
        let mut result = Vec::with_capacity(data.len());
        for (_, raw) in split_items(data) {
            let mut bytes = raw.to_vec();
            let len = raw.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_be_bytes).and_then(|n| usize::try_from(n).ok());
            if let Some((f, size_at, time_at)) = len.and_then(|n| raw.get(8..8usize.checked_add(n)?)).and_then(parse_item_positions)
                && f.path.replace('\\', "/") == path.replace('\\', "/")
            {
                if let Some(field) = size_at.checked_add(8).and_then(|at| bytes.get_mut(at..at.checked_add(8)?)) {
                    field.copy_from_slice(&size.to_be_bytes());
                    changed = true;
                }
                if let Some(field) = time_at.and_then(|n| n.checked_add(8)).and_then(|at| bytes.get_mut(at..at.checked_add(8)?)) {
                    field.copy_from_slice(&modified.to_be_bytes());
                    changed = true;
                }
            }
            result.extend(bytes);
        }
        if changed {
            *data = std::sync::Arc::new(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_roundtrip_unicode_literal_paths_and_truncation() {
        let f = ExternalFile {
            uuid: "id-1".into(),
            file_name: "logo ü %.psd".into(),
            path: "C:/art/logo ü %20 #.psd".into(),
            relative_path: Some("../art/logo ü %20 #.psd".into()),
            file_type: *b"8BPS",
            file_size: 123,
            modified: 1234.5,
        };
        let bytes = encode_external_file(&f);
        assert_eq!(parse_external_files(&bytes), vec![f]);
        photocraft_psd::TaggedBlock::new(*b"lnkE", bytes.clone()).check_structure().unwrap();
        for n in 0..bytes.len() - 4 {
            assert!(parse_external_files(&bytes[..n]).is_empty());
        }
    }
}
