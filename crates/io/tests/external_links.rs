//! External sources must remain external across PSD/PSB save/open, including missing files.
use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::{Document, Layer, LayerContent, LayerMask, SmartObject, SmartSource};
use photocraft_geom::{Affine, Rect, Size};
use photocraft_io::{export, import, linked};
use photocraft_psd::PsdFile;
use photocraft_raster::Surface;

#[test]
fn external_psd_links_keep_shared_source_transform_mask_and_cached_pixels() {
    for ext in ["psd", "psb"] {
        for (mode, depth) in [
            (ColorMode::Rgb, SampleType::U8),
            (ColorMode::Rgb, SampleType::U16),
            (ColorMode::Rgb, SampleType::F32),
            (ColorMode::Cmyk, SampleType::U8),
            (ColorMode::Cmyk, SampleType::U16),
        ] {
            let mut doc = Document::new("parent", Size::new(30, 30), mode, depth);
            let path = "C:/missing artwork/logo ü %20 #.psd";
            let mut cache = Surface::new(doc.pixel_format());
            let color: &[f32] = if mode == ColorMode::Cmyk { &[0.2, 0.3, 0.1, 0.4, 1.0] } else { &[0.8, 0.2, 0.5, 1.0] };
            cache.fill_rect(doc.bounds(), color);
            let mut sm = SmartObject::new(SmartSource::Linked { path: path.into() }, Affine::IDENTITY, Some(cache));
            sm.transform = Affine { m: [1.1, 0.2, -0.2, 1.1, 3.0, 4.0] };
            doc.layers.push(Layer::new("front", LayerContent::Smart(sm.clone())));
            doc.layers.push(Layer::new("back", LayerContent::Smart(sm)));
            for layer in &mut doc.layers {
                let mut mask = LayerMask::reveal_all();
                mask.surface.fill_rect(Rect::new(0, 0, 12, 30), &[0.0]);
                layer.mask = Some(mask);
            }
            let before = photocraft_compose::flatten(&doc);
            let out = export(&doc, ext, &Default::default()).unwrap();
            assert!(out.warnings.is_empty(), "{:?}", out.warnings);
            let psd = PsdFile::from_bytes(&out.bytes).unwrap();
            let files: Vec<_> = psd.global_blocks.iter().filter(|b| b.key == *b"lnkE").flat_map(|b| linked::parse_external_files(&b.data)).collect();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path, path);
            for l in psd.layers() {
                assert!(l.block(b"SoLE").is_some());
                assert!(l.block(b"SoLd").is_none());
            }
            let back = import("parent.psd", &out.bytes).unwrap().document;
            for l in &back.layers {
                let mask = l.mask.as_ref().unwrap();
                assert_eq!(mask.value(5, 5), 0.0);
                assert_eq!(mask.value(20, 5), 1.0);
                let LayerContent::Smart(s) = &l.content else { panic!("source became pixels") };
                assert_eq!(s.source, SmartSource::Linked { path: path.into() });
                for (a, b) in s.transform.m.iter().zip([1.1, 0.2, -0.2, 1.1, 3.0, 4.0]) {
                    assert!((a - b).abs() < 1e-12);
                }
            }
            let LayerContent::Smart(a) = &back.layers[0].content else { panic!() };
            let LayerContent::Smart(b) = &back.layers[1].content else { panic!() };
            assert_eq!(a.contents_id, b.contents_id);
            let rendered = photocraft_compose::flatten(&back);
            let error = before.px.iter().zip(rendered.px).flat_map(|(a, b)| (0..4).map(move |c| (a[c] - b[c]).abs())).fold(0.0, f32::max);
            assert!(error <= 1.0 / 255.0 + 1e-6, "{error}");
            let again = export(&back, ext, &Default::default()).unwrap();
            let saved = PsdFile::from_bytes(&again.bytes).unwrap();
            let original_block = psd.global_blocks.iter().find(|b| b.key == *b"lnkE").unwrap();
            let saved_block = saved.global_blocks.iter().find(|b| b.key == *b"lnkE").unwrap();
            assert_eq!(original_block.data, saved_block.data, "untouched external descriptors must survive byte-for-byte");
            let files: Vec<_> = saved.global_blocks.iter().filter(|b| b.key == *b"lnkE").flat_map(|b| linked::parse_external_files(&b.data)).collect();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path, path);
        }
    }
}

#[test]
fn layered_tiff_keeps_external_file_descriptor_across_byte_order_conversion() {
    let mut doc = Document::new("tiff parent", Size::new(12, 10), ColorMode::Rgb, SampleType::U16);
    let path = "C:/missing artwork/bron ü.psd";
    let mut pixels = Surface::new(doc.pixel_format());
    pixels.fill_rect(doc.bounds(), &[0.2, 0.5, 0.8, 1.0]);
    let sm = SmartObject::new(SmartSource::Linked { path: path.into() }, Affine::IDENTITY, Some(pixels));
    doc.layers.push(Layer::new("external", LayerContent::Smart(sm)));
    let opts = photocraft_io::ExportOptions { tiff_layers: true, ..Default::default() };
    let out = export(&doc, "tif", &opts).unwrap();
    let back = import("parent.tif", &out.bytes).unwrap().document;
    let LayerContent::Smart(sm) = &back.layers[0].content else { panic!("external TIFF layer flattened") };
    assert_eq!(sm.source, SmartSource::Linked { path: path.into() });
    let a = photocraft_compose::flatten(&doc);
    let b = photocraft_compose::flatten(&back);
    assert!(a.px.iter().zip(&b.px).all(|(a, b)| (0..4).all(|c| (a[c] - b[c]).abs() < 0.001)));
}
