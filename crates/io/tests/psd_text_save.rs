//! Regressions for edited imported type. The global text engine must describe the current
//! local type data while untouched native anchors and opaque fields remain unchanged.
//! All inputs are generated here; no customer documents or fonts are embedded.

use std::sync::Arc;

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::text::{CharStyle, FontFeature, Kerning, TextRun};
use photocraft_doc::{Document, Layer, LayerContent, TextLayer};
use photocraft_geom::{Affine, Size};
use photocraft_psd::PsdFile;
use photocraft_psd::descriptor::{Id, Value as D};
use photocraft_text::engine_data::{self as ed, Value as E};

fn dict(items: Vec<(&str, E)>) -> E {
    E::Dict(items.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

fn new_type(text: &str, mode: Kerning, transform: Affine) -> TextLayer {
    let style = CharStyle { font_family: "Inter".into(), size_pt: 18.0, kerning: mode, ..Default::default() };
    let mut t = TextLayer { text: text.into(), transform, runs: vec![TextRun { len: text.len(), style }], ..Default::default() };
    t.sync_summary();
    t
}

fn refresh(t: &mut TextLayer, dpi: f32, fmt: photocraft_color::PixelFormat) {
    let (layout, rendered) = photocraft_text::TextEngine::new().render(t, dpi, fmt);
    t.cache = Some(rendered.surface);
    t.psd_raw = Some(Arc::new(photocraft_text::psd::build_tysh(t, dpi, layout.bounds())));
}

fn global_for(texts: &[(&str, Kerning)], frames: bool, stylistic_set: bool) -> Vec<u8> {
    let objects = texts
        .iter()
        .enumerate()
        .map(|(i, (text, mode))| {
            let mut style = dict(vec![("0", E::Int(0)), ("11", E::Int(if *mode == Kerning::Optical { 2 } else { 1 }))]);
            if stylistic_set {
                style.set("85", E::Int(1));
            }
            let run = dict(vec![
                ("0", dict(vec![("0", dict(vec![("0", E::String(String::new())), ("6", style)]))])),
                ("1", E::Int(text.encode_utf16().count() as i64 + 1)),
            ]);
            let mut object = dict(vec![("0", dict(vec![("0", E::String(format!("{text}\r"))), ("6", dict(vec![("0", E::Array(vec![run]))]))]))]);
            if frames {
                object.set("1", dict(vec![("2", E::Array(vec![dict(vec![("6", E::Array(vec![dict(vec![("10", E::Int(i as i64))])]))])]))]));
            }
            object
        })
        .collect();
    let frames = texts
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let x = 40.0 + i as f64 * 80.0;
            let y = 70.0 + i as f64 * 40.0;
            dict(vec![(
                "0",
                dict(vec![
                    ("0", E::Array(vec![E::Real(x), E::Real(y)])),
                    ("2", dict(vec![("2", E::Array(vec![E::Real(1.0), E::Real(0.0), E::Real(0.0), E::Real(1.0), E::Real(0.0), E::Real(0.0)]))])),
                ]),
            )])
        })
        .collect();
    ed::write_bare(&[
        ("98".into(), dict(vec![("0", E::Int(14))])),
        ("0".into(), dict(vec![("8", dict(vec![("0", E::Array(frames))]))])),
        ("1".into(), dict(vec![("1", E::Array(objects))])),
    ])
}

/// Native-like frame anchors are kept separate from the local TySh transforms.
/// Import must fold them into the model, while an unchanged save keeps both blocks.
fn imported(mode: Kerning, stylistic_set: bool) -> (Document, Vec<u8>) {
    let texts = [("Alpha", mode), ("Other", Kerning::Metrics)];
    let previous = global_for(&texts, true, stylistic_set);
    let mut doc = Document::new("text-save", Size::new(256, 192), ColorMode::Rgb, SampleType::U8);
    for (i, (text, mode)) in texts.iter().enumerate() {
        let x = 40.0 + i as f64 * 80.0;
        let y = 70.0 + i as f64 * 40.0;
        let mut t = new_type(text, *mode, Affine::translate(x, y));
        if stylistic_set {
            t.runs[0].style.features.push(FontFeature { tag: "ss01".into(), value: 1 });
            t.sync_summary();
        }
        photocraft_text::TextEngine::new().render_layer(&mut t, doc.resolution_dpi, doc.pixel_format());
        let mut local = t.clone();
        local.transform = Affine::IDENTITY;
        let data = photocraft_text::psd::build_tysh(&local, doc.resolution_dpi, Some([x as f32, y as f32 - 18.0, x as f32 + 55.0, y as f32 + 4.0]));
        let data = Arc::new(photocraft_text::psd::set_text_index(&data, i as i32).unwrap());
        t.psd_raw = Some(data.clone());
        let mut layer = Layer::new(*text, LayerContent::Text(t));
        layer.psd_blocks.push((*b"TySh", data));
        doc.layers.push(layer);
    }
    doc.metadata.psd_global_blocks.push((*b"8BIM", *b"Txt2", Arc::new(previous.clone())));
    let bytes = photocraft_io::export(&doc, "fixture.psd", &Default::default()).unwrap().bytes;
    let file = PsdFile::from_bytes(&bytes).unwrap();
    let normalized = txt2(&file).unwrap().to_vec();
    (photocraft_io::import("fixture.psd", &bytes).unwrap().document, normalized)
}

fn export(doc: &Document) -> (PsdFile, Document) {
    let bytes = photocraft_io::export(doc, "saved.psd", &Default::default()).unwrap().bytes;
    (PsdFile::from_bytes(&bytes).unwrap(), photocraft_io::import("saved.psd", &bytes).unwrap().document)
}

fn txt2(file: &PsdFile) -> Option<&[u8]> {
    file.global_blocks.iter().find(|b| &b.key == b"Txt2").map(|b| b.data.as_slice())
}

fn text<'a>(doc: &'a Document, name: &str) -> &'a TextLayer {
    let layer = doc.layers.iter().find(|l| l.name == name).unwrap();
    let LayerContent::Text(t) = &layer.content else { panic!("expected type layer") };
    t
}

fn text_mut<'a>(doc: &'a mut Document, name: &str) -> &'a mut TextLayer {
    let layer = doc.layers.iter_mut().find(|l| l.name == name).unwrap();
    let LayerContent::Text(t) = &mut layer.content else { panic!("expected type layer") };
    t
}

fn assert_transform(actual: Affine, expected: Affine) {
    for (a, b) in actual.m.iter().zip(expected.m) {
        assert!((a - b).abs() < 1e-6, "transform changed: {actual:?} != {expected:?}");
    }
}

fn assert_global_matches(file: &PsdFile, current: &TextLayer) {
    let global = photocraft_text::psd::parse_txt2(txt2(file).expect("live text must keep its complete global block")).unwrap();
    let raw = current.psd_raw.as_deref().unwrap();
    let index = photocraft_text::psd::text_index(raw).unwrap() as usize;
    let object = &global.path(&["1", "1"]).and_then(E::as_array).unwrap()[index];
    let value = format!("{}\r", current.text.replace('\n', "\r"));
    assert_eq!(object.path(&["0", "0"]).and_then(E::as_str), Some(value.as_str()));
    let runs = object.path(&["0", "6", "0"]).and_then(E::as_array).unwrap();
    assert_eq!(runs.iter().map(|run| run.get("1").and_then(E::as_i64).unwrap()).sum::<i64>(), value.encode_utf16().count() as i64);
    for run in runs {
        assert_eq!(run.path(&["0", "0", "6", "1"]).and_then(E::as_f64), Some(f64::from(current.size_pt)), "global style must describe the current size");
    }
    let view = object.get("1").expect("text object needs a native view");
    let frame_index = view.get("0").and_then(E::as_array).unwrap()[0].get("0").and_then(E::as_i64).unwrap();
    assert!(frame_index >= 0);
    let frame = global.path(&["0", "8", "0"]).and_then(E::as_array).unwrap()[frame_index as usize].get("0").unwrap();
    assert_eq!(frame.path(&["2", "2"]).and_then(E::as_array).unwrap().len(), 6);
    assert!(view.get("4").is_some());
    assert!(view.get("5").is_some());
    let mut reconstructed = photocraft_text::psd::text_layer_from_tysh(raw, 72.0).unwrap();
    photocraft_text::psd::apply_txt2(&mut reconstructed, raw, &global);
    photocraft_text::psd::apply_txt2_frame(&mut reconstructed, raw, &global);
    assert_eq!(reconstructed.text, current.text);
    assert_transform(reconstructed.transform, current.transform);
}

#[test]
fn unchanged_import_preserves_native_txt2_and_tysh_bytes() {
    let (doc, previous) = imported(Kerning::Metrics, false);
    assert_transform(text(&doc, "Alpha").transform, Affine::translate(40.0, 70.0));
    let (file, back) = export(&doc);
    assert_eq!(txt2(&file), Some(previous.as_slice()));
    for name in ["Alpha", "Other"] {
        assert_eq!(text(&back, name).psd_raw, text(&doc, name).psd_raw, "unedited {name} TySh changed");
    }
}

#[test]
fn edited_metrics_text_keeps_complete_global_data_and_other_layers_position() {
    let (mut doc, _) = imported(Kerning::Metrics, false);
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let other = text(&doc, "Other").clone();
    let t = text_mut(&mut doc, "Alpha");
    t.text = "Beta 😀".into();
    t.runs[0].len = t.text.len();
    refresh(t, dpi, fmt);
    let expected = t.clone();
    let (file, back) = export(&doc);
    assert_global_matches(&file, text(&back, "Alpha"));
    assert_global_matches(&file, text(&back, "Other"));
    assert_eq!(text(&back, "Alpha").text, expected.text);
    assert_eq!(text(&back, "Alpha").size_pt, expected.size_pt);
    assert_transform(text(&back, "Alpha").transform, expected.transform);
    assert_eq!(text(&back, "Other").text, other.text);
    assert_transform(text(&back, "Other").transform, other.transform);
}

#[test]
fn editing_a_sibling_preserves_every_unedited_tysh_byte_and_unknown_field() {
    let (mut doc, previous) = imported(Kerning::Metrics, false);
    let layer = doc.layers.iter_mut().find(|l| l.name == "Other").unwrap();
    let LayerContent::Text(other) = &mut layer.content else { panic!("expected type layer") };
    let mut tysh = photocraft_text::psd::parse_tysh(other.psd_raw.as_deref().unwrap()).unwrap();
    let mut engine = photocraft_text::psd::engine_data(&tysh.text).unwrap();
    let dict = engine.get_mut("EngineDict").unwrap();
    let Some(E::Array(char_runs)) = dict.get_mut("StyleRun").and_then(|r| r.get_mut("RunArray")) else { panic!("missing character runs") };
    char_runs[0].get_mut("StyleSheet").and_then(|s| s.get_mut("StyleSheetData")).unwrap().set("UnmodeledCharProperty", E::Int(173));
    let Some(E::Array(paragraph_runs)) = dict.get_mut("ParagraphRun").and_then(|r| r.get_mut("RunArray")) else { panic!("missing paragraph runs") };
    paragraph_runs[0].get_mut("ParagraphSheet").and_then(|s| s.get_mut("Properties")).unwrap().set("UnmodeledParagraphProperty", E::Int(179));
    let rendered = dict.get_mut("Rendered").unwrap();
    rendered.set("UnmodeledRenderedProperty", E::Int(181));
    let Some(E::Array(children)) = rendered.get_mut("Shapes").and_then(|s| s.get_mut("Children")) else { panic!("missing rendered shapes") };
    children[0].get_mut("Cookie").and_then(|c| c.get_mut("Photoshop")).unwrap().set("UnmodeledShapeProperty", E::Int(187));
    for key in ["ResourceDict", "DocumentResources"] {
        let Some(E::Array(fonts)) = engine.get_mut(key).and_then(|r| r.get_mut("FontSet")) else { panic!("missing font set") };
        fonts[0].set("UnmodeledFontProperty", E::Int(191));
    }
    tysh.text.items.iter_mut().find(|(k, _)| k.is("EngineData")).unwrap().1 = D::RawData(ed::write(&engine));
    tysh.text.items.push((Id::new("UnmodeledTypeDescriptor"), D::Integer(197)));
    tysh.warp.as_mut().unwrap().items.push((Id::new("UnmodeledWarpDescriptor"), D::Integer(199)));
    let mut raw = photocraft_text::psd::write_tysh(&tysh);
    if raw.len() % 2 == 1 {
        raw.push(0);
    }
    other.psd_raw = Some(Arc::new(raw.clone()));
    let expected_transform = other.transform;
    let expected_runs = other.char_runs();
    let expected_paragraphs = other.paragraph_runs();
    layer.psd_blocks.iter_mut().find(|(k, _)| k == b"TySh").unwrap().1 = Arc::new(raw.clone());
    let unknown_block = vec![2, 3, 5, 7];
    layer.psd_blocks.push((*b"zZXx", Arc::new(unknown_block.clone())));

    let (unchanged, unchanged_doc) = export(&doc);
    assert_eq!(txt2(&unchanged), Some(previous.as_slice()));
    assert_eq!(text(&unchanged_doc, "Other").psd_raw.as_deref().unwrap().as_slice(), raw.as_slice());
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let edited = text_mut(&mut doc, "Alpha");
    edited.text = "Beta".into();
    edited.runs[0].len = edited.text.len();
    refresh(edited, dpi, fmt);
    let (file, back) = export(&doc);
    assert_global_matches(&file, text(&back, "Alpha"));
    assert_global_matches(&file, text(&back, "Other"));
    let saved = text(&back, "Other").psd_raw.as_deref().unwrap();
    assert_eq!(saved.as_slice(), raw.as_slice(), "an untouched native anchor must not require any local type rewrite");
    assert_transform(text(&back, "Other").transform, expected_transform);
    assert_eq!(text(&back, "Other").char_runs(), expected_runs);
    assert_eq!(text(&back, "Other").paragraph_runs(), expected_paragraphs);
    assert_eq!(back.layers.iter().find(|l| l.name == "Other").unwrap().psd_blocks.iter().find(|(k, _)| k == b"zZXx").unwrap().1.as_slice(), unknown_block);
}

#[test]
fn adding_type_keeps_singular_and_unaffected_native_anchors_unchanged() {
    let (mut doc, previous) = imported(Kerning::Metrics, false);
    let layer = doc.layers.iter_mut().find(|l| l.name == "Other").unwrap();
    let LayerContent::Text(other) = &mut layer.content else { panic!("expected type layer") };
    let mut raw = other.psd_raw.as_ref().unwrap().as_ref().clone();
    // Collapse the x axis while retaining the native position. Adding another layer must
    // not require an inverse or a rewrite of this unchanged singular type transform.
    raw[2..10].copy_from_slice(&0.0f64.to_be_bytes());
    other.psd_raw = Some(Arc::new(raw.clone()));
    layer.psd_blocks.iter_mut().find(|(k, _)| k == b"TySh").unwrap().1 = Arc::new(raw);
    let (native, mut doc) = export(&doc);
    assert_eq!(txt2(&native), Some(previous.as_slice()));
    assert_eq!(text(&doc, "Other").transform.determinant(), 0.0);
    let alpha_raw = text(&doc, "Alpha").psd_raw.clone();
    let other_raw = text(&doc, "Other").psd_raw.clone();
    let mut added = new_type("TEST", Kerning::Metrics, Affine::translate(160.0, 160.0));
    refresh(&mut added, doc.resolution_dpi, doc.pixel_format());
    doc.layers.push(Layer::new("Added", LayerContent::Text(added)));

    let saved = photocraft_io::export(&doc, "singular-frame.psd", &Default::default()).unwrap();
    let file = PsdFile::from_bytes(&saved.bytes).unwrap();
    let back = photocraft_io::import("singular-frame.psd", &saved.bytes).unwrap().document;
    assert_global_matches(&file, text(&back, "Alpha"));
    assert_global_matches(&file, text(&back, "Other"));
    assert_global_matches(&file, text(&back, "Added"));
    assert_eq!(text(&back, "Alpha").psd_raw, alpha_raw, "adding type must not rewrite an existing native anchor");
    assert_eq!(text(&back, "Other").psd_raw, other_raw);
    assert_transform(text(&back, "Alpha").transform, text(&doc, "Alpha").transform);
    assert_transform(text(&back, "Other").transform, text(&doc, "Other").transform);
    assert_eq!(text(&back, "Added").text, "TEST");
}

#[test]
fn same_text_font_size_edit_updates_the_global_style() {
    let (mut doc, _) = imported(Kerning::Metrics, false);
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let t = text_mut(&mut doc, "Alpha");
    t.runs[0].style.size_pt = 27.0;
    t.sync_summary();
    refresh(t, dpi, fmt);
    let expected_transform = t.transform;
    let (file, back) = export(&doc);
    assert_global_matches(&file, text(&back, "Alpha"));
    assert_eq!(text(&back, "Alpha").text, "Alpha");
    assert_eq!(text(&back, "Alpha").size_pt, 27.0);
    assert_transform(text(&back, "Alpha").transform, expected_transform);
}

#[test]
fn adding_metrics_type_writes_a_complete_current_object() {
    let (mut doc, _) = imported(Kerning::Metrics, false);
    let mut added = new_type("TEST", Kerning::Metrics, Affine::translate(160.0, 160.0));
    refresh(&mut added, doc.resolution_dpi, doc.pixel_format());
    doc.layers.push(Layer::new("Added", LayerContent::Text(added)));
    let (file, back) = export(&doc);
    for name in ["Alpha", "Other", "Added"] {
        assert_global_matches(&file, text(&back, name));
    }
    assert_eq!(text(&back, "Added").text, "TEST");
    assert_transform(text(&back, "Added").transform, Affine::translate(160.0, 160.0));
}

#[test]
fn already_stripped_metrics_object_is_repaired_by_noop_save() {
    let (doc, _) = imported(Kerning::Metrics, false);
    let (mut file, _) = export(&doc);
    let malformed = global_for(&[("Alpha", Kerning::Metrics), ("Other", Kerning::Metrics)], false, false);
    let block = file.global_blocks.iter_mut().find(|b| &b.key == b"Txt2").unwrap();
    block.padding = Some(vec![0; (4 - malformed.len() % 4) % 4]);
    block.data = malformed;
    let bytes = file.to_bytes().unwrap();
    let bad = photocraft_io::import("old-save.psd", &bytes).unwrap().document;
    let expected_transform = text(&bad, "Alpha").transform;
    let (fixed, back) = export(&bad);
    assert_global_matches(&fixed, text(&back, "Alpha"));
    assert_eq!(text(&back, "Alpha").text, "Alpha");
    assert_transform(text(&back, "Alpha").transform, expected_transform);
}

#[test]
fn optical_kerning_survives_a_complete_global_text_edit() {
    let (mut doc, _) = imported(Kerning::Optical, false);
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let t = text_mut(&mut doc, "Alpha");
    t.text = "AVATAR".into();
    t.runs[0].len = t.text.len();
    refresh(t, dpi, fmt);
    let (file, back) = export(&doc);
    assert_global_matches(&file, text(&back, "Alpha"));
    assert!(text(&back, "Alpha").char_runs().iter().all(|r| r.style.kerning == Kerning::Optical));
}

#[test]
fn active_stylistic_set_survives_a_complete_global_style_edit() {
    let (mut doc, _) = imported(Kerning::Metrics, true);
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let t = text_mut(&mut doc, "Alpha");
    assert!(t.char_runs().iter().any(|r| r.style.features.contains(&FontFeature { tag: "ss01".into(), value: 1 })));
    t.runs[0].style.size_pt = 22.0;
    t.sync_summary();
    refresh(t, dpi, fmt);
    let (file, back) = export(&doc);
    assert_global_matches(&file, text(&back, "Alpha"));
    assert!(text(&back, "Alpha").char_runs().iter().all(|r| r.style.features.contains(&FontFeature { tag: "ss01".into(), value: 1 })));
}

#[test]
fn default_contextual_alternates_off_survives_a_complete_metrics_edit() {
    let (doc, _) = imported(Kerning::Metrics, false);
    let (mut file, _) = export(&doc);
    let block = file.global_blocks.iter_mut().find(|b| &b.key == b"Txt2").unwrap();
    let mut tree = photocraft_text::psd::parse_txt2(&block.data).unwrap();
    let Some(E::Array(objects)) = tree.get_mut("1").and_then(|v| v.get_mut("1")) else { panic!("missing objects") };
    for object in objects {
        let Some(E::Array(runs)) = object.get_mut("0").and_then(|v| v.get_mut("6")).and_then(|v| v.get_mut("0")) else { panic!("missing runs") };
        for run in runs {
            run.get_mut("0").and_then(|v| v.get_mut("0")).and_then(|v| v.get_mut("6")).unwrap().set("20", E::Bool(false));
        }
    }
    let E::Dict(items) = tree else { panic!("missing global dictionary") };
    block.data = ed::write_bare(&items);
    block.padding = Some(vec![0; (4 - block.data.len() % 4) % 4]);
    let bytes = file.to_bytes().unwrap();
    let mut doc = photocraft_io::import("default-calt.psd", &bytes).unwrap().document;
    assert!(text(&doc, "Alpha").char_runs().iter().all(|r| r.style.features.contains(&FontFeature { tag: "calt".into(), value: 0 })));
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let t = text_mut(&mut doc, "Alpha");
    t.text = "Beta".into();
    t.runs[0].len = t.text.len();
    refresh(t, dpi, fmt);
    let (file, back) = export(&doc);
    assert_global_matches(&file, text(&back, "Alpha"));
    assert_eq!(text(&back, "Alpha").text, "Beta");
    assert!(text(&back, "Alpha").char_runs().iter().all(|r| r.style.features.contains(&FontFeature { tag: "calt".into(), value: 0 })));
    assert_transform(text(&back, "Alpha").transform, Affine::translate(40.0, 70.0));
}
