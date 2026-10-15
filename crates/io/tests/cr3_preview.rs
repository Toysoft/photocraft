//! Camera-rendered JPEG previews stay distinct from RAW development.
use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
use photocraft_color::SampleType;
use photocraft_io::import;

fn jpeg(icc: Option<Vec<u8>>) -> Vec<u8> {
    let mut image = Image::from_u8(32, 20, ChannelLayout::Rgb, vec![180; 32 * 20 * 3]).unwrap();
    image.icc = icc;
    photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap()
}

#[test]
fn jpeg_preview_keeps_metadata_profile_and_one_rotation() {
    let bytes = photocraft_raw::testgen::cr3::with_metadata(&jpeg(None), 6, 2, "<xmp custom='retained'/>");
    let result = import("camera.CR3", &bytes).unwrap();
    assert_eq!((result.document.size.width, result.document.size.height), (20, 32));
    assert_eq!(result.document.depth, SampleType::U8);
    assert_eq!(result.document.icc_profile.as_ref().unwrap().as_slice(), photocraft_cms::Builtin::AdobeRgbCompat.profile().to_bytes().as_slice());
    let exif = result.document.metadata.exif.as_ref().unwrap();
    assert_eq!(photocraft_codecs::exif_orientation(exif), 1);
    assert!(exif.windows(5).any(|w| w == [7, 9, 11, 13, 15]));
    assert!(result.document.metadata.xmp.as_ref().unwrap().contains("custom='retained'"));
    assert!(result.warnings.iter().any(|w| w.contains("8-bit, not the raw sensor data")));
    assert!(!result.warnings.iter().any(|w| w.contains("developed with default settings")));
    let back = photocraft_format::load_from_bytes(&photocraft_format::save_to_bytes(&result.document, &Default::default()).unwrap()).unwrap();
    assert_eq!(back.metadata, result.document.metadata);
    assert_eq!(back.icc_profile, result.document.icc_profile);
}

#[test]
fn embedded_icc_wins_and_uncalibrated_exif_does_not_imply_adobe_rgb() {
    let srgb = photocraft_cms::Builtin::Srgb.profile().to_bytes();
    let image = jpeg(Some(srgb.as_ref().clone()));
    let bytes = photocraft_raw::testgen::cr3::with_metadata(&image, 1, 2, "<xmp/>");
    let result = import("camera.cr3", &bytes).unwrap();
    assert_eq!(result.document.icc_profile.as_ref().unwrap().as_slice(), srgb.as_slice());
    let bytes = photocraft_raw::testgen::cr3::with_metadata(&jpeg(None), 1, 65535, "<xmp/>");
    assert_eq!(photocraft_raw::cr3::metadata(&bytes).color_space(), None);
    let result = import("camera.cr3", &bytes).unwrap();
    assert_ne!(result.document.icc_profile.as_ref().map(|p| p.as_slice()), Some(photocraft_cms::Builtin::AdobeRgbCompat.profile().to_bytes().as_slice()));
}

#[test]
fn malformed_container_metadata_does_not_discard_a_valid_preview() {
    let mut bytes = photocraft_raw::testgen::cr3::preview(&jpeg(None), 1);
    let start = bytes.windows(4).position(|w| w == b"CMT1").unwrap() + 4;
    bytes[start + 4..start + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    let result = import("camera.cr3", &bytes).unwrap();
    assert_eq!((result.document.size.width, result.document.size.height), (32, 20));
    assert!(result.warnings.iter().any(|w| w.contains("metadata could not be preserved")));
}
