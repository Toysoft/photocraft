//! CR3 camera-preview import; no sensor entropy decoding or camera calibration.
use crate::{ImportResult, IoError, flat::image_to_document};
use photocraft_cms::Builtin;
use photocraft_codecs::{self as codecs, DecodeOptions, Format};
use photocraft_raw::{Limits, RawError, cr3};

pub(crate) fn import(name: &str, bytes: &[u8], limits: &Limits) -> Result<ImportResult, IoError> {
    let preview = photocraft_raw::embedded_preview(bytes).ok_or_else(|| RawError::Unsupported("CR3 has no supported camera JPEG preview".into()))?;
    let mut image = codecs::decode_as_with(
        Format::Jpeg,
        preview.jpeg,
        &DecodeOptions {
            keep_orientation: true,
            limits: codecs::Limits { max_width: limits.max_width, max_height: limits.max_height, max_pixels: limits.max_pixels, max_alloc: limits.max_alloc },
        },
    )?;
    let own_orientation = image.meta.exif.as_deref().map_or(1, codecs::exif_orientation);
    let metadata = cr3::metadata(bytes);
    if image.icc.is_none() {
        image.icc = match metadata.color_space() {
            Some(1) => Some(Builtin::Srgb.profile().to_bytes().as_ref().clone()),
            Some(2) => Some(Builtin::AdobeRgbCompat.profile().to_bytes().as_ref().clone()),
            _ => None,
        };
    }
    let mut notes = Vec::new();
    match metadata.exif(limits) {
        Ok(Some(exif)) => image.meta.exif = Some(exif),
        Ok(None) => {}
        Err(e) => notes.push(format!("CR3 container metadata could not be preserved: {e}")),
    }
    if let Some(xmp) = metadata.xmp {
        image.meta.xmp = Some(xmp.to_string());
    }
    let orientation = if own_orientation != 1 { own_orientation } else { photocraft_raw::orientation(bytes).unwrap_or(1) };
    image = image.oriented(orientation)?;
    let mut result = image_to_document(name, &image)?;
    result.warnings.insert(0, format!("CR3: opened the camera's {}x{} JPEG preview (8-bit, not the raw sensor data)", image.width(), image.height()));
    result.warnings.extend(notes);
    Ok(result)
}
