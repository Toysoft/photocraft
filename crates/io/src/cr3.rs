//! CR3 preview import with container metadata and the preview's actual colour space.
use crate::{ImportResult, IoError, flat::image_to_document};
use photocraft_cms::{Builtin, Curve};
use photocraft_codecs::{self as codecs, DecodeOptions, Format, SampleType};
use photocraft_raw::{Limits, RawError, cr3};
use std::sync::Arc;

fn pq(v: f64) -> f64 {
    // SMPTE ST 2084 EOTF, normalised to its 10,000-nit maximum.
    let p = v.powf(32.0 / 2523.0);
    ((p - 3424.0 / 4096.0).max(0.0) / (2413.0 / 128.0 - 2392.0 / 128.0 * p)).powf(16384.0 / 2610.0)
}
fn preview_profile(color: [u16; 3]) -> Result<Arc<Vec<u8>>, IoError> {
    if color == [1, 13, 1] {
        return Ok(Builtin::Srgb.profile().to_bytes());
    }
    if color == [9, 16, 9] {
        let mut profile = Builtin::Rec2020.profile().clone();
        profile.description = "Rec.2020 PQ camera preview (PhotoCraft, ST 2084)".into();
        let curve = Curve::Table((0..=4096).map(|i| pq(f64::from(i) / 4096.0) as f32).collect());
        profile.trc = Some([curve.clone(), curve.clone(), curve]);
        return Ok(profile.with_encoded_bytes().to_bytes());
    }
    Err(IoError::Raw(RawError::Unsupported(format!("CR3 HEVC preview colour declaration {color:?}"))))
}
pub(crate) fn import(name: &str, bytes: &[u8], limits: &Limits) -> Result<ImportResult, IoError> {
    let jpeg = photocraft_raw::embedded_preview(bytes);
    let hevc = cr3::hevc_previews(bytes).into_iter().max_by_key(|p| u64::from(p.width) * u64::from(p.height));
    let use_hevc = hevc
        .as_ref()
        .is_some_and(|p| jpeg.is_none_or(|j| cfg!(feature = "heif") && u64::from(p.width) * u64::from(p.height) > u64::from(j.width) * u64::from(j.height)));
    let mut notes = Vec::new();
    let (mut image, kind) = if use_hevc {
        let p = hevc.ok_or_else(|| RawError::Malformed("missing CR3 HEVC preview".into()))?;
        match decode_hevc(&p, limits) {
            Ok(image) => (image, format!("{}-bit {}HEVC", p.bits, if p.color.get(1) == Some(&16) { "HDR PQ " } else { "" })),
            Err(error) => {
                if let Some(jpeg) = jpeg {
                    notes.push(format!("CR3 HEVC preview unavailable: {error}; opened the JPEG preview"));
                    (codecs::decode_as_with(Format::Jpeg, jpeg.jpeg, &decode_options(limits))?, "8-bit JPEG".to_string())
                } else {
                    return Err(error);
                }
            }
        }
    } else if let Some(p) = jpeg {
        (codecs::decode_as_with(Format::Jpeg, p.jpeg, &decode_options(limits))?, "8-bit JPEG".to_string())
    } else {
        return Err(IoError::Raw(RawError::Unsupported("This CR3 sensor variant is unsupported and no supported camera preview was found".into())));
    };
    let own_orientation = image.meta.exif.as_deref().map_or(1, codecs::exif_orientation);
    let metadata = cr3::metadata(bytes);
    if image.icc.is_none() {
        image.icc = match metadata.color_space() {
            Some(1) => Some(Builtin::Srgb.profile().to_bytes().as_ref().clone()),
            Some(2) => Some(Builtin::AdobeRgbCompat.profile().to_bytes().as_ref().clone()),
            _ => None,
        };
    }
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
    let precision = if image.sample_type() == SampleType::U16 { 10 } else { 8 };
    result
        .warnings
        .insert(0, format!("CR3: opened the camera's {}x{} {kind} preview ({precision}-bit, not the raw sensor data)", image.width(), image.height()));
    result.warnings.extend(notes);
    Ok(result)
}
fn decode_hevc(p: &cr3::HevcPreview<'_>, limits: &Limits) -> Result<codecs::Image, IoError> {
    let profile = preview_profile(p.color)?;
    let framed = p.to_heif(limits)?;
    let mut image = codecs::decode_as_with(Format::Heif, &framed, &decode_options(limits))?;
    if (image.width(), image.height()) != (p.width, p.height) || (p.bits > 8) != (image.sample_type() == SampleType::U16) {
        return Err(IoError::Raw(RawError::Malformed("CR3 HEVC preview geometry or depth mismatch".into())));
    }
    image.icc = Some(profile.as_ref().clone());
    Ok(image)
}
fn decode_options(limits: &Limits) -> DecodeOptions {
    DecodeOptions {
        keep_orientation: true,
        limits: codecs::Limits { max_width: limits.max_width, max_height: limits.max_height, max_pixels: limits.max_pixels, max_alloc: limits.max_alloc },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pq_profile_has_the_standard_transfer_and_survives_serialisation() {
        assert_eq!(pq(0.0), 0.0);
        assert!((pq(1.0) - 1.0).abs() < 1e-12);
        assert!((pq(0.5080784215) - 0.01).abs() < 1e-8);
        let bytes = preview_profile([9, 16, 9]).unwrap();
        let profile = photocraft_cms::Profile::parse(&bytes).unwrap();
        let curves = profile.trc.unwrap();
        assert!((curves[0].eval64(0.5080784215) - 0.01).abs() < 2e-5);
        assert!(preview_profile([9, 18, 9]).is_err());
    }
}
