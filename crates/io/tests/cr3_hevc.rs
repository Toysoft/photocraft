//! Existing HEIF decoding of a camera preview, never CRX sensor reconstruction.
use photocraft_raw::{Limits, RawError, cr3::HevcPreview};

fn coded_picture() -> (Vec<u8>, Vec<u8>) {
    let stream = include_bytes!("../../heif/tests/fixtures/intra-422-10.hevc");
    let starts: Vec<_> = stream.windows(3).enumerate().filter_map(|(i, b)| (b == [0, 0, 1]).then_some(i)).collect();
    let units: Vec<_> = starts
        .iter()
        .enumerate()
        .map(|(i, start)| {
            let mut unit = &stream[start + 3..starts.get(i + 1).copied().unwrap_or(stream.len())];
            while unit.last() == Some(&0) {
                unit = &unit[..unit.len() - 1];
            }
            unit
        })
        .collect();
    let mut config = vec![0u8; 23];
    config[0] = 1;
    config[1] = 4;
    config[16] = 0xfe;
    config[17] = 0xfa;
    config[18] = 0xfa;
    config[21] = 3;
    config[22] = 3;
    for kind in [32, 33, 34] {
        let matching: Vec<_> = units.iter().filter(|n| (n[0] >> 1) & 63 == kind).collect();
        config.push(128 | kind);
        config.extend((matching.len() as u16).to_be_bytes());
        for unit in matching {
            config.extend((unit.len() as u16).to_be_bytes());
            config.extend(*unit);
        }
    }
    let mut cell = Vec::new();
    for unit in units.iter().filter(|n| (n[0] >> 1) & 63 <= 31) {
        cell.extend((unit.len() as u32).to_be_bytes());
        cell.extend(*unit);
    }
    let sample = [(cell.len() as u32).to_be_bytes().as_slice(), cell.as_slice()].concat();
    (config, sample)
}

fn preview<'a>(configuration: &'a [u8], sample: &'a [u8]) -> HevcPreview<'a> {
    HevcPreview {
        width: 120,
        height: 100,
        tile_width: 128,
        tile_height: 128,
        rows: 1,
        columns: 1,
        configuration,
        sample,
        color: [9, 16, 9],
        full_range: false,
        bits: 10,
    }
}

#[cfg(feature = "heif")]
#[test]
fn ten_bit_pq_preview_uses_existing_decoder_and_preserves_crop_metadata_and_pixels() {
    let (config, sample) = coded_picture();
    let p = preview(&config, &sample);
    let framed = p.to_heif(&Limits::default()).unwrap();
    let expected = photocraft_codecs::decode_as(photocraft_codecs::Format::Heif, &framed).unwrap().oriented(6).unwrap();
    let bytes = photocraft_raw::testgen::cr3::hevc_preview(&p, 6);
    let r = photocraft_io::import("camera.cr3", &bytes).unwrap();
    assert_eq!((r.document.size.width, r.document.size.height), (100, 120));
    assert_eq!(r.document.depth, photocraft_color::SampleType::U16);
    let image = photocraft_io::document_to_image(&r.document, &mut Vec::new()).unwrap();
    assert_eq!(image.data(), expected.data());
    assert!(image.to_u16_samples().unwrap().iter().any(|v| v % 257 != 0));
    let profile = photocraft_cms::Profile::parse(r.document.icc_profile.as_ref().unwrap()).unwrap();
    assert!(profile.description.contains("PQ"));
    assert_eq!(photocraft_codecs::exif_orientation(r.document.metadata.exif.as_ref().unwrap()), 1);
    assert!(r.document.metadata.xmp.as_ref().unwrap().contains("preview='retained'"));
    assert!(r.warnings.iter().any(|w| w.contains("10-bit HDR PQ HEVC") && w.contains("not the raw sensor data")));
    assert!(!r.warnings.iter().any(|w| w.contains("developed with default settings")));
    let back = photocraft_format::load_from_bytes(&photocraft_format::save_to_bytes(&r.document, &Default::default()).unwrap()).unwrap();
    assert_eq!(back.metadata, r.document.metadata);
    assert_eq!(back.icc_profile, r.document.icc_profile);
}

#[cfg(not(feature = "heif"))]
#[test]
fn builds_without_heif_explain_the_missing_optional_decoder() {
    let (config, sample) = coded_picture();
    let bytes = photocraft_raw::testgen::cr3::hevc_preview(&preview(&config, &sample), 1);
    let error = photocraft_io::import("camera.cr3", &bytes).unwrap_err();
    assert!(error.to_string().contains("HEIC/HEIF support isn't included"), "{error}");
}

#[test]
fn unsupported_colour_and_invalid_framing_are_errors_and_crx_remains_unsupported() {
    let (config, sample) = coded_picture();
    let mut p = preview(&config, &sample);
    let bytes = photocraft_raw::testgen::cr3::hevc_preview(&p, 1);
    assert!(matches!(photocraft_raw::decode(&bytes, &Limits::default()), Err(RawError::Unsupported(_))));
    assert!(p.to_heif(&Limits { max_alloc: 64, ..Default::default() }).is_err());
    p.color = [9, 18, 9];
    let bytes = photocraft_raw::testgen::cr3::hevc_preview(&p, 1);
    assert!(photocraft_io::import("camera.cr3", &bytes).unwrap_err().to_string().contains("colour declaration"));
    p.rows = 0;
    assert!(p.to_heif(&Limits::default()).is_err());
    p.rows = 1;
    p.sample = &sample[..sample.len() - 1];
    assert!(p.to_heif(&Limits::default()).is_err());
    for n in 0..bytes.len() {
        let _ = photocraft_raw::cr3::hevc_previews(&bytes[..n]);
    }
}

#[test]
fn an_available_jpeg_still_opens_when_the_hevc_preview_cannot_be_used() {
    let (config, sample) = coded_picture();
    let mut p = preview(&config, &sample);
    p.color = if cfg!(feature = "heif") { [9, 18, 9] } else { [9, 16, 9] };
    let image = photocraft_codecs::Image::from_u8(32, 20, photocraft_codecs::ChannelLayout::Rgb, vec![180; 32 * 20 * 3]).unwrap();
    let jpeg = photocraft_codecs::encode(&image, photocraft_codecs::Format::Jpeg, &Default::default()).unwrap();
    let mut bytes = photocraft_raw::testgen::cr3::preview(&jpeg, 1);
    let hevc = photocraft_raw::testgen::cr3::hevc_preview(&p, 1);
    let at = hevc.windows(4).position(|b| b == b"PRVW").unwrap() - 4;
    bytes.extend_from_slice(&hevc[at..]);
    let r = photocraft_io::import("camera.cr3", &bytes).unwrap();
    assert_eq!((r.document.size.width, r.document.size.height), (32, 20));
    assert_eq!(r.document.depth, photocraft_color::SampleType::U8);
    assert!(r.warnings.iter().any(|w| w.contains("8-bit, not the raw sensor data")));
    if cfg!(feature = "heif") {
        assert!(r.warnings.iter().any(|w| w.contains("HEVC preview unavailable")));
    }
}

#[cfg(feature = "heif")]
#[test]
fn several_preview_cells_decode_as_a_cropped_grid() {
    let (config, cell) = coded_picture();
    let samples = cell.repeat(4);
    let mut p = preview(&config, &samples);
    p.rows = 2;
    p.columns = 2;
    p.width = 250;
    p.height = 240;
    let image = photocraft_codecs::decode_as(photocraft_codecs::Format::Heif, &p.to_heif(&Limits::default()).unwrap()).unwrap();
    assert_eq!(image.dimensions(), (250, 240));
    for channel in 0..3 {
        assert_eq!(image.get(10, 15, channel), image.get(138, 15, channel));
        assert_eq!(image.get(10, 15, channel), image.get(10, 143, channel));
    }
}
