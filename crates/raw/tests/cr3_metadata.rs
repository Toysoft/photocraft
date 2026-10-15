//! Container metadata survives independently of any sensor decoder.
use photocraft_raw::{Limits, RawError, RawFormat, cr3, identify, orientation, testgen};

fn jpeg() -> Vec<u8> {
    vec![0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 0, 20, 0, 32, 1, 1, 0x11, 0, 0xff, 0xd9]
}

#[test]
fn independent_metadata_blocks_relocate_and_keep_unknown_values() {
    let bytes = testgen::cr3::with_metadata(&jpeg(), 8, 2, "<xmp custom='retained'/>");
    assert_eq!(identify(&bytes), Some(RawFormat::Cr3));
    let metadata = cr3::metadata(&bytes);
    assert_eq!(metadata.color_space(), Some(2));
    assert_eq!(metadata.xmp, Some("<xmp custom='retained'/>"));
    let exif = metadata.exif(&Limits::default()).unwrap().unwrap();
    assert_eq!(orientation(&exif), Some(8));
    assert!(exif.windows(b"Canon Synthetic CR3\0".len()).any(|w| w == b"Canon Synthetic CR3\0"));
    assert!(exif.windows(5).any(|w| w == [7, 9, 11, 13, 15]));
    assert!(metadata.exif(&Limits { max_alloc: 32, ..Default::default() }).is_err());
    assert!(matches!(photocraft_raw::decode(&bytes, &Limits::default()), Err(RawError::Unsupported(_))));
}

#[test]
fn truncations_and_corrupt_box_lengths_never_panic() {
    let bytes = testgen::cr3::preview(&jpeg(), 6);
    for n in 0..bytes.len() {
        let _ = cr3::metadata(&bytes[..n]).exif(&Limits::default());
    }
    for kind in [b"moov", b"trak", b"mdat", b"CMT1"] {
        for length in [0u32, 1, 2, 7, u32::MAX] {
            let mut broken = bytes.clone();
            let at = broken.windows(4).position(|w| w == kind).unwrap() - 4;
            broken[at..at + 4].copy_from_slice(&length.to_be_bytes());
            let _ = cr3::metadata(&broken).exif(&Limits::default());
        }
    }
}
