//! The document text engine and each local type descriptor must describe the same edit.
//! Synthetic native frames use different frame and TextIndex numbers deliberately.

use std::sync::Arc;

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::text::{CharStyle, FontFeature, Kerning, ParagraphRun, ParagraphStyle, TextAlign, TextRun};
use photocraft_doc::text_styles::{CharacterStyleDef, ParagraphStyleDef, diff_attrs};
use photocraft_doc::{Document, Layer, LayerContent, TextLayer, TextStyles};
use photocraft_geom::{Affine, Size};
use photocraft_psd::PsdFile;
use photocraft_psd::descriptor::Value as D;
use photocraft_psd::tagged::TaggedBlock;
use photocraft_text::engine_data::{self as ed, Value as E};

fn dict(items: Vec<(&str, E)>) -> E {
    E::Dict(items.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

fn new_type(value: &str, transform: Affine) -> TextLayer {
    let style = CharStyle { font_family: "Inter".into(), size_pt: 18.0, ..Default::default() };
    let mut text = TextLayer { text: value.into(), transform, runs: vec![TextRun { len: value.len(), style }], ..Default::default() };
    text.sync_summary();
    text
}

fn refresh(text: &mut TextLayer, dpi: f32, fmt: photocraft_color::PixelFormat) {
    let (layout, rendered) = photocraft_text::TextEngine::new().render(text, dpi, fmt);
    text.cache = Some(rendered.surface);
    text.psd_raw = Some(Arc::new(photocraft_text::psd::build_tysh(text, dpi, layout.bounds())));
}

fn frame(x: f64, y: f64, marker: i64) -> E {
    dict(vec![
        (
            "0",
            dict(vec![
                ("0", E::Array(vec![E::Real(x), E::Real(y)])),
                (
                    "2",
                    dict(vec![
                        ("2", E::Array(vec![E::Real(1.0), E::Real(0.0), E::Real(0.0), E::Real(1.0), E::Real(0.0), E::Real(0.0)])),
                        ("6", E::Array(vec![E::Int(-1), E::Int(-1)])),
                    ]),
                ),
                ("UnmodeledFrameProperty", E::Int(marker)),
            ]),
        ),
        ("UnmodeledFrameEnvelope", E::Int(marker + 1)),
    ])
}

fn object(value: &str, frame_index: i64, marker: i64) -> E {
    let style = dict(vec![
        ("0", E::Int(0)),
        ("1", E::Real(18.0)),
        ("6", E::Real(1.0)),
        ("7", E::Real(1.0)),
        ("8", E::Real(0.0)),
        ("9", E::Real(0.0)),
        ("11", E::Int(1)),
    ]);
    let run =
        dict(vec![("0", dict(vec![("0", dict(vec![("0", E::String(String::new())), ("6", style)]))])), ("1", E::Int(value.encode_utf16().count() as i64 + 1))]);
    let paragraph = dict(vec![
        ("0", dict(vec![("0", dict(vec![("0", E::String(String::new())), ("5", dict(vec![("0", E::Int(0))])), ("6", E::Int(0))]))])),
        ("1", E::Int(value.encode_utf16().count() as i64 + 1)),
    ]);
    dict(vec![
        (
            "0",
            dict(vec![
                ("0", E::String(format!("{value}\r"))),
                ("5", dict(vec![("0", E::Array(vec![paragraph]))])),
                ("6", dict(vec![("0", E::Array(vec![run]))])),
                ("UnmodeledTextModelProperty", E::Int(marker)),
            ]),
        ),
        (
            "1",
            dict(vec![
                ("0", E::Array(vec![dict(vec![("0", E::Int(frame_index))])])),
                ("4", dict(vec![("0", E::Int(7)), ("1", E::Int(11)), ("2", E::Int(0)), ("3", E::String("Photoshop".into()))])),
                ("5", E::Int(0)),
                ("2", E::Array(vec![dict(vec![("6", E::Array(vec![dict(vec![("10", E::Int(frame_index))])]))])])),
            ]),
        ),
        ("UnmodeledObjectProperty", E::Int(marker + 1)),
    ])
}

fn native_global() -> Vec<u8> {
    // Objects Alpha and Other are at slots 2 and 0, while their frames are at 4 and 1.
    let mut frames = vec![E::dict(); 5];
    frames[0] = dict(vec![("UnreferencedFrame", E::Int(211))]);
    frames[1] = frame(120.0, 110.0, 223);
    frames[4] = frame(40.0, 70.0, 227);
    let objects = vec![object("Other", 1, 229), E::dict(), object("Alpha", 4, 233)];
    let font =
        dict(vec![("0", dict(vec![("99", E::Name("CoolTypeFont".into())), ("0", dict(vec![("0", E::String("Inter-Regular".into())), ("2", E::Int(0))]))]))]);
    let base_char = dict(vec![("0", E::Int(0)), ("1", E::Real(18.0)), ("11", E::Int(1))]);
    let base_para = dict(vec![("0", E::Int(0)), ("7", E::Real(1.2))]);
    let version = dict(vec![("0", E::Int(7)), ("1", E::Int(11)), ("2", E::Int(0)), ("3", E::String("Photoshop".into()))]);
    let mut bytes = ed::write_bare(&[
        ("98".into(), dict(vec![("0", E::Int(14))])),
        (
            "0".into(),
            dict(vec![
                ("1", dict(vec![("0", E::Array(vec![font]))])),
                ("5", dict(vec![("0", E::Array(vec![dict(vec![("0", dict(vec![("0", E::String("Normal RGB".into())), ("6", base_char.clone())]))])]))])),
                (
                    "6",
                    dict(vec![(
                        "0",
                        // A root paragraph style has no parent. Per-run /6=0 references
                        // this style, but the named style itself must not refer to itself.
                        E::Array(vec![dict(vec![("0", dict(vec![("0", E::String("Normal RGB".into())), ("5", base_para.clone())]))])]),
                    )]),
                ),
                ("8", dict(vec![("0", E::Array(frames))])),
                ("UnmodeledDocumentResource", E::Int(239)),
            ]),
        ),
        (
            "1".into(),
            dict(vec![
                ("0", dict(vec![("8", E::Bool(true))])),
                ("1", E::Array(objects)),
                ("2", base_char),
                ("3", base_para),
                ("4", version),
                ("UnmodeledTextEngineSetting", E::Int(241)),
            ]),
        ),
        ("UnmodeledGlobalProperty".into(), E::Int(251)),
    ]);
    bytes.extend_from_slice(b" /UnmodeledOpaqueGlobal (G\\000\\377Q)");
    bytes
}

fn native_fixture() -> (Document, Vec<u8>) {
    let mut doc = Document::new("complete-text", Size::new(256, 192), ColorMode::Rgb, SampleType::U8);
    for (name, index, x, y) in [("Alpha", 2, 40.0, 70.0), ("Other", 0, 120.0, 110.0)] {
        let mut text = new_type(name, Affine::translate(x, y));
        photocraft_text::TextEngine::new().render_layer(&mut text, doc.resolution_dpi, doc.pixel_format());
        let mut local = text.clone();
        local.transform = Affine::IDENTITY;
        let raw = photocraft_text::psd::build_tysh(&local, doc.resolution_dpi, Some([x as f32, y as f32 - 18.0, x as f32 + 55.0, y as f32 + 4.0]));
        let raw = photocraft_text::psd::set_text_index(&raw, index).unwrap();
        let mut tysh = photocraft_text::psd::parse_tysh(&raw).unwrap();
        let D::RawData(engine) = &mut tysh.text.items.iter_mut().find(|(key, _)| key.is("EngineData")).unwrap().1 else { panic!("missing EngineData") };
        let at = engine.windows(2).rposition(|bytes| bytes == b">>").unwrap();
        engine.splice(at..at, b"/UnmodeledOpaque (A\\000\\377B)\n".iter().copied());
        let raw = Arc::new(photocraft_text::psd::write_tysh(&tysh));
        text.psd_raw = Some(raw.clone());
        let mut layer = Layer::new(name, LayerContent::Text(text));
        layer.psd_blocks.push((*b"TySh", raw));
        layer.psd_blocks.push((*b"zZXx", Arc::new(vec![2, 3, 5, 7])));
        doc.layers.push(layer);
    }
    // Install the synthetic global block after writing the PSD container, so fixture creation
    // does not use the global-object writer whose edited behavior these tests exercise.
    let bytes = photocraft_io::export(&doc, "fixture.psd", &Default::default()).unwrap().bytes;
    let mut file = PsdFile::from_bytes(&bytes).unwrap();
    // The container export makes fresh local descriptions independent. Restore the planned
    // native local matrices before attaching their separate global anchors; otherwise this
    // fixture would apply each anchor twice before the save under test even starts.
    for record in &mut file.layer_info.as_mut().unwrap().layers {
        let name = record.name();
        let source = doc.layers.iter().find(|layer| layer.name == name).unwrap();
        let raw = &source.psd_blocks.iter().find(|(key, _)| key == b"TySh").unwrap().1;
        record.block_mut(b"TySh").unwrap().data = raw.as_ref().clone();
    }
    file.global_blocks.retain(|block| &block.key != b"Txt2");
    let previous = native_global();
    let mut block = TaggedBlock::new(*b"Txt2", previous.clone());
    block.padding = Some(vec![0; (4 - previous.len() % 4) % 4]);
    file.global_blocks.push(block);
    file.global_blocks.push(TaggedBlock::new(*b"zZzz", vec![11, 13, 17, 19]));
    let bytes = file.to_bytes().unwrap();
    (photocraft_io::import("fixture.psd", &bytes).unwrap().document, previous)
}

fn text<'a>(doc: &'a Document, name: &str) -> &'a TextLayer {
    let LayerContent::Text(text) = &doc.layers.iter().find(|layer| layer.name == name).unwrap().content else { panic!("expected type") };
    text
}

fn text_mut<'a>(doc: &'a mut Document, name: &str) -> &'a mut TextLayer {
    let LayerContent::Text(text) = &mut doc.layers.iter_mut().find(|layer| layer.name == name).unwrap().content else { panic!("expected type") };
    text
}

fn export(doc: &Document) -> (PsdFile, Document) {
    let bytes = photocraft_io::export(doc, "saved.psd", &Default::default()).unwrap().bytes;
    (PsdFile::from_bytes(&bytes).unwrap(), photocraft_io::import("saved.psd", &bytes).unwrap().document)
}

fn txt2(file: &PsdFile) -> &[u8] {
    &file.global_blocks.iter().find(|block| &block.key == b"Txt2").expect("a native complete text block must survive an edit").data
}

fn tree(file: &PsdFile) -> E {
    photocraft_text::psd::parse_txt2(txt2(file)).unwrap()
}

fn object_for<'a>(global: &'a E, text: &TextLayer) -> &'a E {
    let index = photocraft_text::psd::text_index(text.psd_raw.as_deref().unwrap()).unwrap() as usize;
    &global.path(&["1", "1"]).and_then(E::as_array).unwrap()[index]
}

fn frame_for<'a>(global: &'a E, object: &E) -> &'a E {
    let view = object.get("1").expect("text object needs its native view");
    let index = view
        .get("0")
        .and_then(E::as_array)
        .and_then(|frames| frames.first())
        .and_then(|reference| reference.get("0"))
        .and_then(E::as_i64)
        .or_else(|| {
            view.get("2")
                .and_then(E::as_array)
                .and_then(|rendered| rendered.first())
                .and_then(|rendered| rendered.get("6"))
                .and_then(E::as_array)
                .and_then(|components| components.first())
                .and_then(|component| component.get("10"))
                .and_then(E::as_i64)
        })
        .expect("native view needs its frame resource index");
    assert!(index >= 0);
    global.path(&["0", "8", "0"]).and_then(E::as_array).unwrap()[index as usize].get("0").expect("referenced frame must exist")
}

fn assert_changed_cache_is_invalidated(global: &E, current: &TextLayer) {
    let view = object_for(global, current).get("1").expect("changed type must keep its native view");
    assert!(view.get("2").is_none(), "cached glyph output from the previous text must be invalidated");
    assert!(view.get("4").is_some(), "native view version must remain");
    assert!(view.get("5").is_some(), "native view metadata must remain");
}

fn assert_transform(actual: Affine, expected: Affine) {
    for (a, b) in actual.m.iter().zip(expected.m) {
        assert!((a - b).abs() < 1e-6, "position changed: {actual:?} != {expected:?}");
    }
}

fn assert_current_object(global: &E, current: &TextLayer) {
    let object = object_for(global, current);
    let expected = format!("{}\r", current.text.replace('\n', "\r"));
    assert_eq!(object.path(&["0", "0"]).and_then(E::as_str), Some(expected.as_str()), "global text must contain this edit");
    let runs = object.path(&["0", "6", "0"]).and_then(E::as_array).unwrap();
    assert_eq!(runs.iter().map(|run| run.get("1").and_then(E::as_i64).unwrap()).sum::<i64>(), expected.encode_utf16().count() as i64);
    for run in runs {
        let style = run.path(&["0", "0", "6"]).unwrap();
        assert_eq!(style.get("1").and_then(E::as_f64), Some(f64::from(current.size_pt)), "global size must match local type");
        let index = style.get("0").and_then(E::as_i64).unwrap();
        assert!(index >= 0);
        let font = &global.path(&["0", "1", "0"]).and_then(E::as_array).unwrap()[index as usize];
        assert_eq!(font.path(&["0", "0", "0"]).and_then(E::as_str), current.char_runs()[0].style.postscript_name.as_deref());
    }
    let frame = frame_for(global, object);
    assert_eq!(frame.path(&["2", "2"]).and_then(E::as_array).unwrap().len(), 6);
    let raw = current.psd_raw.as_deref().unwrap();
    let mut reconstructed = photocraft_text::psd::text_layer_from_tysh(raw, 72.0).unwrap();
    photocraft_text::psd::apply_txt2(&mut reconstructed, raw, global);
    photocraft_text::psd::apply_txt2_frame(&mut reconstructed, raw, global);
    assert_eq!(reconstructed.text, current.text);
    assert_transform(reconstructed.transform, current.transform);
}

fn engine_bytes(text: &TextLayer) -> Vec<u8> {
    let tysh = photocraft_text::psd::parse_tysh(text.psd_raw.as_deref().unwrap()).unwrap();
    let D::RawData(data) = tysh.text.get("EngineData").unwrap() else { panic!("missing EngineData") };
    data.clone()
}

#[test]
fn unchanged_native_text_keeps_global_and_local_bytes() {
    let (doc, previous) = native_fixture();
    assert_transform(text(&doc, "Alpha").transform, Affine::translate(40.0, 70.0));
    assert_transform(text(&doc, "Other").transform, Affine::translate(120.0, 110.0));
    let (file, back) = export(&doc);
    assert_eq!(txt2(&file), previous);
    for name in ["Alpha", "Other"] {
        assert_eq!(text(&back, name).psd_raw, text(&doc, name).psd_raw);
        assert_eq!(engine_bytes(text(&back, name)), engine_bytes(text(&doc, name)));
    }
}

#[test]
fn edited_metrics_text_keeps_a_frame_and_never_restores_old_text() {
    let (mut doc, previous) = native_fixture();
    let before = photocraft_text::psd::parse_txt2(&previous).unwrap();
    let sibling = text(&doc, "Other").clone();
    let sibling_object = object_for(&before, &sibling).clone();
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let edited = text_mut(&mut doc, "Alpha");
    edited.text = "Beta \u{1f600}\nEND".into();
    edited.runs[0].len = edited.text.len();
    edited.runs[0].style.size_pt = 27.0;
    edited.sync_summary();
    refresh(edited, dpi, fmt);
    let expected = edited.clone();
    let (file, back) = export(&doc);
    let global = tree(&file);
    assert_current_object(&global, text(&back, "Alpha"));
    assert_changed_cache_is_invalidated(&global, text(&back, "Alpha"));
    assert_eq!(text(&back, "Alpha").text, expected.text);
    assert_eq!(text(&back, "Alpha").size_pt, 27.0);
    assert_transform(text(&back, "Alpha").transform, expected.transform);
    assert_eq!(object_for(&global, text(&back, "Other")), &sibling_object);
    assert_eq!(frame_for(&global, &sibling_object), frame_for(&before, &sibling_object));
    assert_eq!(text(&back, "Other").psd_raw, sibling.psd_raw);
    assert_eq!(engine_bytes(text(&back, "Other")), engine_bytes(&sibling));
    assert_eq!(global.path(&["0", "UnmodeledDocumentResource"]), before.path(&["0", "UnmodeledDocumentResource"]));
    assert_eq!(global.path(&["1", "UnmodeledTextEngineSetting"]), before.path(&["1", "UnmodeledTextEngineSetting"]));
    assert_eq!(global.get("UnmodeledGlobalProperty"), before.get("UnmodeledGlobalProperty"));
    assert_eq!(global.get("UnmodeledOpaqueGlobal"), before.get("UnmodeledOpaqueGlobal"), "global byte strings must remain bytes when another object changes");
    assert_eq!(file.global_blocks.iter().find(|block| &block.key == b"zZzz").unwrap().data, [11, 13, 17, 19]);
    let (again, back_again) = export(&back);
    assert_current_object(&tree(&again), text(&back_again, "Alpha"));
    assert_eq!(text(&back_again, "Alpha").text, expected.text);
    assert_eq!(text(&back_again, "Alpha").size_pt, 27.0);
    assert_transform(text(&back_again, "Alpha").transform, expected.transform);
}

#[test]
fn a_style_only_edit_updates_the_object_without_changing_its_text_or_sibling() {
    let (mut doc, _) = native_fixture();
    let sibling = text(&doc, "Other").clone();
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let edited = text_mut(&mut doc, "Alpha");
    edited.runs[0].style.size_pt = 31.0;
    edited.runs[0].style.tracking = 85.0;
    edited.runs[0].style.underline = true;
    edited.runs[0].style.font_family = "Courier New".into();
    edited.runs[0].style.postscript_name = Some("CourierNewPSMT".into());
    edited.sync_summary();
    refresh(edited, dpi, fmt);
    let (file, back) = export(&doc);
    let global = tree(&file);
    assert_current_object(&global, text(&back, "Alpha"));
    assert_changed_cache_is_invalidated(&global, text(&back, "Alpha"));
    let actual = text(&back, "Alpha");
    assert_eq!(actual.text, "Alpha");
    assert_eq!(actual.size_pt, 31.0);
    assert_eq!(actual.char_runs()[0].style.tracking, 85.0);
    assert!(actual.char_runs()[0].style.underline);
    assert_eq!(actual.char_runs()[0].style.postscript_name.as_deref(), Some("CourierNewPSMT"));
    let object = object_for(&global, actual);
    for run in object.path(&["0", "6", "0"]).and_then(E::as_array).unwrap() {
        assert_eq!(run.path(&["0", "0", "6", "8"]).and_then(E::as_f64), Some(85.0), "global tracking must match the edited style");
    }
    assert_eq!(text(&back, "Other").psd_raw, sibling.psd_raw);
}

#[test]
fn a_moved_type_frame_matches_the_local_descriptor_after_reopening_twice() {
    let (mut doc, _) = native_fixture();
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let moved = text_mut(&mut doc, "Alpha");
    moved.transform = Affine { m: [1.1, 0.2, -0.1, 0.9, 86.0, 132.0] };
    refresh(moved, dpi, fmt);
    let expected = moved.transform;
    let (file, back) = export(&doc);
    let global = tree(&file);
    assert_current_object(&global, text(&back, "Alpha"));
    assert_changed_cache_is_invalidated(&global, text(&back, "Alpha"));
    assert_transform(text(&back, "Alpha").transform, expected);
    let (again, back_again) = export(&back);
    assert_current_object(&tree(&again), text(&back_again, "Alpha"));
    assert_transform(text(&back_again, "Alpha").transform, expected);
}

#[test]
fn a_new_front_layer_does_not_steal_an_existing_text_index_or_frame() {
    let (mut doc, _) = native_fixture();
    let alpha = text(&doc, "Alpha").clone();
    let other = text(&doc, "Other").clone();
    let mut added = new_type("NEW \u{1f600}", Affine::translate(160.0, 160.0));
    refresh(&mut added, doc.resolution_dpi, doc.pixel_format());
    doc.layers.insert(0, Layer::new("Added", LayerContent::Text(added)));
    let (file, back) = export(&doc);
    let global = tree(&file);
    for name in ["Alpha", "Other", "Added"] {
        assert_current_object(&global, text(&back, name));
    }
    assert_changed_cache_is_invalidated(&global, text(&back, "Added"));
    assert_eq!(text(&back, "Alpha").psd_raw, alpha.psd_raw);
    assert_eq!(text(&back, "Other").psd_raw, other.psd_raw);
    assert_eq!(photocraft_text::psd::text_index(text(&back, "Alpha").psd_raw.as_deref().unwrap()), Some(2));
    assert_eq!(photocraft_text::psd::text_index(text(&back, "Other").psd_raw.as_deref().unwrap()), Some(0));
    let added_index = photocraft_text::psd::text_index(text(&back, "Added").psd_raw.as_deref().unwrap()).unwrap();
    assert!(![0, 2].contains(&added_index));
    assert_transform(text(&back, "Added").transform, Affine::translate(160.0, 160.0));
}

#[test]
fn edited_text_keeps_global_only_features_in_the_current_object() {
    let (mut doc, _) = native_fixture();
    let (dpi, fmt) = (doc.resolution_dpi, doc.pixel_format());
    let edited = text_mut(&mut doc, "Alpha");
    edited.text = "AVATAR".into();
    edited.runs[0].len = edited.text.len();
    edited.runs[0].style.kerning = Kerning::Optical;
    edited.runs[0].style.features = vec![FontFeature { tag: "ss03".into(), value: 1 }, FontFeature { tag: "swsh".into(), value: 1 }];
    edited.sync_summary();
    refresh(edited, dpi, fmt);
    let (file, back) = export(&doc);
    let global = tree(&file);
    assert_current_object(&global, text(&back, "Alpha"));
    assert_changed_cache_is_invalidated(&global, text(&back, "Alpha"));
    let runs = text(&back, "Alpha").char_runs();
    assert!(runs.iter().all(|run| run.style.kerning == Kerning::Optical));
    for tag in ["ss03", "swsh"] {
        assert!(runs.iter().all(|run| run.style.features.contains(&FontFeature { tag: tag.into(), value: 1 })));
    }
}

#[test]
fn incomplete_singular_type_keeps_native_frames_without_partial_local_patches() {
    let (doc, _) = native_fixture();
    let (mut file, _) = export(&doc);
    let record = file.layer_info.as_mut().unwrap().layers.iter_mut().find(|layer| layer.name() == "Other").unwrap();
    record.block_mut(b"TySh").unwrap().data[2..10].copy_from_slice(&0.0f64.to_be_bytes());
    let mut global = tree(&file);
    let Some(E::Array(objects)) = global.get_mut("1").and_then(|engine| engine.get_mut("1")) else { panic!("missing text objects") };
    // Both objects need repair. Alpha can be made independent, but Other cannot be
    // inverted; neither native descriptor may be patched before all frames pass.
    for index in [0, 2] {
        let E::Dict(model) = objects[index].get_mut("0").unwrap() else { panic!("missing text model") };
        model.retain(|(key, _)| key != "5");
    }
    let E::Dict(items) = global else { panic!("missing global dictionary") };
    let block = file.global_blocks.iter_mut().find(|block| &block.key == b"Txt2").unwrap();
    block.data = ed::write_bare(&items);
    block.padding = Some(vec![0; (4 - block.data.len() % 4) % 4]);
    let bytes = file.to_bytes().unwrap();
    let incomplete = photocraft_io::import("singular-incomplete.psd", &bytes).unwrap().document;
    let before = tree(&file);
    let alpha = text(&incomplete, "Alpha").clone();
    let other = text(&incomplete, "Other").clone();
    assert_eq!(other.transform.determinant(), 0.0);
    assert_transform(alpha.transform, Affine::translate(40.0, 70.0));
    assert_transform(other.transform, Affine { m: [0.0, 0.0, 0.0, 1.0, 0.0, 110.0] });
    let (saved, back) = export(&incomplete);
    let after = tree(&saved);
    for (name, original) in [("Alpha", &alpha), ("Other", &other)] {
        let current = text(&back, name);
        assert_current_object(&after, current);
        assert_changed_cache_is_invalidated(&after, current);
        assert_transform(current.transform, original.transform);
        assert_eq!(current.psd_raw, original.psd_raw, "one unrepresentable frame must prevent partial local patches");
        assert_eq!(
            frame_for(&after, object_for(&after, current)),
            frame_for(&before, object_for(&before, original)),
            "native frame geometry must remain available after repair"
        );
    }
}

#[test]
fn invalid_native_frame_references_are_repaired_without_moving_type() {
    // Slot 0 exists but contains opaque metadata rather than a usable frame.
    for invalid in [-1, 0, 99_999] {
        let (doc, _) = native_fixture();
        let (mut file, _) = export(&doc);
        let mut global = tree(&file);
        let Some(E::Array(objects)) = global.get_mut("1").and_then(|engine| engine.get_mut("1")) else { panic!("missing text objects") };
        let Some(E::Array(references)) = objects[2].get_mut("1").and_then(|view| view.get_mut("0")) else { panic!("missing frame references") };
        references[0].set("0", E::Int(invalid));
        let E::Dict(items) = global else { panic!("missing global dictionary") };
        let block = file.global_blocks.iter_mut().find(|block| &block.key == b"Txt2").unwrap();
        block.data = ed::write_bare(&items);
        block.padding = Some(vec![0; (4 - block.data.len() % 4) % 4]);
        let bytes = file.to_bytes().unwrap();
        let imported = photocraft_io::import("invalid-frame.psd", &bytes).unwrap().document;
        let expected = text(&imported, "Alpha").clone();
        let sibling = text(&imported, "Other").clone();
        let (saved, back) = export(&imported);
        let repaired = tree(&saved);
        assert_current_object(&repaired, text(&back, "Alpha"));
        assert_transform(text(&back, "Alpha").transform, expected.transform);
        assert_eq!(text(&back, "Alpha").text, expected.text);
        assert_eq!(text(&back, "Other").psd_raw, sibling.psd_raw);
    }
}

#[test]
fn new_type_does_not_replace_an_unreferenced_native_text_object() {
    let (doc, _) = native_fixture();
    let (mut file, _) = export(&doc);
    let mut global = tree(&file);
    let archived = object("Archived", 4, 257);
    let Some(E::Array(objects)) = global.get_mut("1").and_then(|engine| engine.get_mut("1")) else { panic!("missing text objects") };
    objects[1] = archived.clone();
    let E::Dict(items) = global else { panic!("missing global dictionary") };
    let block = file.global_blocks.iter_mut().find(|block| &block.key == b"Txt2").unwrap();
    block.data = ed::write_bare(&items);
    block.padding = Some(vec![0; (4 - block.data.len() % 4) % 4]);
    let bytes = file.to_bytes().unwrap();
    let mut imported = photocraft_io::import("unnamed-object.psd", &bytes).unwrap().document;
    let alpha = text(&imported, "Alpha").clone();
    let other = text(&imported, "Other").clone();
    let mut added = new_type("NEW", Affine::translate(160.0, 160.0));
    refresh(&mut added, imported.resolution_dpi, imported.pixel_format());
    imported.layers.insert(0, Layer::new("Added", LayerContent::Text(added)));
    let (saved, back) = export(&imported);
    let after = tree(&saved);
    assert_eq!(&after.path(&["1", "1"]).and_then(E::as_array).unwrap()[1], &archived, "unnamed native text objects must remain opaque and untouched");
    assert_current_object(&after, text(&back, "Added"));
    let index = photocraft_text::psd::text_index(text(&back, "Added").psd_raw.as_deref().unwrap()).unwrap();
    assert!(![0, 1, 2].contains(&index));
    assert_eq!(text(&back, "Alpha").psd_raw, alpha.psd_raw);
    assert_eq!(text(&back, "Other").psd_raw, other.psd_raw);
    assert_eq!(back.layers.len(), 3, "an unnamed global object must not become an extra document layer");
}

#[test]
fn a_singular_type_without_a_native_anchor_fails_export_without_bytes() {
    let (doc, _) = native_fixture();
    let (mut file, _) = export(&doc);
    let record = file.layer_info.as_mut().unwrap().layers.iter_mut().find(|layer| layer.name() == "Other").unwrap();
    record.block_mut(b"TySh").unwrap().data[2..10].copy_from_slice(&0.0f64.to_be_bytes());
    let bytes = file.to_bytes().unwrap();
    let mut imported = photocraft_io::import("singular-native.psd", &bytes).unwrap().document;
    assert_transform(text(&imported, "Other").transform, Affine { m: [0.0, 0.0, 0.0, 1.0, 0.0, 110.0] });
    let block = imported.metadata.psd_global_blocks.iter_mut().find(|(_, key, _)| key == b"Txt2").unwrap();
    let mut global = photocraft_text::psd::parse_txt2(&block.2).unwrap();
    let Some(E::Array(frames)) = global.get_mut("0").and_then(|resources| resources.get_mut("8")).and_then(|frames| frames.get_mut("0")) else {
        panic!("missing frame resources")
    };
    frames[1] = E::dict();
    let E::Dict(items) = global else { panic!("missing global dictionary") };
    block.2 = Arc::new(ed::write_bare(&items));
    let error =
        photocraft_io::export(&imported, "cannot-represent.psd", &Default::default()).expect_err("a missing native anchor must not become an identity frame");
    let message = error.to_string();
    assert!(message.contains("frame") || message.contains("anchor"), "the failure must identify the unrepresentable type geometry: {message}");
}

#[test]
fn incomplete_native_run_coverage_is_repaired_without_losing_its_anchor() {
    let (doc, _) = native_fixture();
    let (mut file, _) = export(&doc);
    let mut global = tree(&file);
    let Some(E::Array(objects)) = global.get_mut("1").and_then(|engine| engine.get_mut("1")) else { panic!("missing text objects") };
    let Some(E::Array(runs)) = objects[2].get_mut("0").and_then(|model| model.get_mut("6")).and_then(|style| style.get_mut("0")) else {
        panic!("missing character runs")
    };
    runs[0].set("1", E::Int(1));
    let E::Dict(items) = global else { panic!("missing global dictionary") };
    let block = file.global_blocks.iter_mut().find(|block| &block.key == b"Txt2").unwrap();
    block.data = ed::write_bare(&items);
    block.padding = Some(vec![0; (4 - block.data.len() % 4) % 4]);
    let bytes = file.to_bytes().unwrap();
    let imported = photocraft_io::import("incomplete-coverage.psd", &bytes).unwrap().document;
    let original = text(&imported, "Alpha").clone();
    let sibling = text(&imported, "Other").clone();
    assert_transform(original.transform, Affine::translate(40.0, 70.0));
    let (saved, back) = export(&imported);
    assert_current_object(&tree(&saved), text(&back, "Alpha"));
    assert_transform(text(&back, "Alpha").transform, original.transform);
    assert_eq!(text(&back, "Alpha").text, original.text);
    assert_eq!(text(&back, "Other").psd_raw, sibling.psd_raw);
}

#[test]
fn document_style_export_uses_emitted_local_properties_for_the_global_model() {
    let mut doc = Document::new("named-style-export", Size::new(256, 192), ColorMode::Rgb, SampleType::U8);
    let mut styles = TextStyles::default();
    styles.character.push(CharacterStyleDef {
        id: 3,
        name: "CurrentChar".into(),
        attrs: diff_attrs(&CharStyle { font_family: "Inter".into(), size_pt: 18.0, ..Default::default() }, &CharStyle::default()),
    });
    styles.paragraph.push(ParagraphStyleDef {
        id: 7,
        name: "CurrentPara".into(),
        para_attrs: diff_attrs(&ParagraphStyle { align: TextAlign::Center, ..Default::default() }, &ParagraphStyle::default()),
        ..Default::default()
    });
    let mut current = new_type("NAMED", Affine::translate(40.0, 70.0));
    current.runs[0].style = styles.resolve_char(None, Some(3), "Inter");
    current.paragraphs = vec![ParagraphRun { len: current.text.len(), style: styles.resolve_para(Some(7)) }];
    current.sync_summary();
    refresh(&mut current, doc.resolution_dpi, doc.pixel_format());
    let mut tysh = photocraft_text::psd::parse_tysh(current.psd_raw.as_deref().unwrap()).unwrap();
    let mut local = photocraft_text::psd::engine_data(&tysh.text).unwrap();
    let inherited_paragraph = [
        ("LeadingType", E::Int(1)),
        ("HyphenatedWordSize", E::Int(8)),
        ("PreHyphen", E::Int(3)),
        ("PostHyphen", E::Int(4)),
        ("Zone", E::Real(17.0)),
        ("WordSpacing", E::Array(vec![E::Real(0.7), E::Real(1.0), E::Real(1.4)])),
        ("LetterSpacing", E::Array(vec![E::Real(-0.2), E::Real(0.0), E::Real(0.3)])),
        ("GlyphSpacing", E::Array(vec![E::Real(0.8), E::Real(1.0), E::Real(1.2)])),
    ];
    for key in ["ResourceDict", "DocumentResources"] {
        let resources = local.get_mut(key).unwrap();
        let Some(E::Array(character_sheets)) = resources.get_mut("StyleSheetSet") else { panic!("missing character sheets") };
        let normal = character_sheets[0].get_mut("StyleSheetData").unwrap();
        for (key, value) in [("FontBaseline", E::Int(1)), ("BaselineDirection", E::Int(0)), ("Language", E::Int(0)), ("NoBreak", E::Bool(true))] {
            normal.set(key, value);
        }
        character_sheets.push(dict(vec![
            ("Name", E::String("SourceChar".into())),
            ("Parent", E::Int(0)),
            ("StyleSheetData", dict(vec![("FontBaseline", E::Int(2))])),
        ]));
        let Some(E::Array(paragraph_sheets)) = resources.get_mut("ParagraphSheetSet") else { panic!("missing paragraph sheets") };
        let normal = paragraph_sheets[0].get_mut("Properties").unwrap();
        for (key, value) in &inherited_paragraph {
            normal.set(key, value.clone());
        }
        paragraph_sheets.push(dict(vec![
            ("Name", E::String("SourcePara".into())),
            ("Parent", E::Int(0)),
            ("Properties", dict(vec![("LeadingType", E::Int(2))])),
        ]));
    }
    let Some(E::Array(character_runs)) = local.get_mut("EngineDict").and_then(|engine| engine.get_mut("StyleRun")).and_then(|run| run.get_mut("RunArray"))
    else {
        panic!("missing character runs")
    };
    for run in character_runs {
        run.get_mut("StyleSheet").unwrap().set("Parent", E::Int(1));
    }
    let Some(E::Array(paragraph_runs)) = local.get_mut("EngineDict").and_then(|engine| engine.get_mut("ParagraphRun")).and_then(|run| run.get_mut("RunArray"))
    else {
        panic!("missing paragraph runs")
    };
    for run in paragraph_runs {
        let sheet = run.get_mut("ParagraphSheet").unwrap();
        sheet.set("Parent", E::Int(1));
        sheet.set("Properties", E::dict());
    }
    tysh.text.items.iter_mut().find(|(key, _)| key.is("EngineData")).unwrap().1 = D::RawData(ed::write(&local));
    let source_raw = Arc::new(photocraft_text::psd::write_tysh(&tysh));
    current.psd_raw = Some(source_raw.clone());
    let mut layer = Layer::new("Named", LayerContent::Text(current));
    layer.psd_blocks.push((*b"TySh", source_raw.clone()));
    doc.layers.push(layer);
    doc.text_styles = styles;
    let (file, back) = export(&doc);
    let saved = text(&back, "Named");
    assert_ne!(saved.psd_raw.as_deref(), Some(source_raw.as_ref()), "document style export must replace the source named sheets");
    let saved_tysh = photocraft_text::psd::parse_tysh(saved.psd_raw.as_deref().unwrap()).unwrap();
    let emitted = photocraft_text::psd::engine_data(&saved_tysh.text).unwrap();
    let emitted_sheets = emitted.path(&["ResourceDict", "StyleSheetSet"]).and_then(E::as_array).unwrap();
    assert_eq!(emitted_sheets[1].get("Name").and_then(E::as_str), Some("CurrentChar"));
    let emitted_paragraph = &emitted.path(&["EngineDict", "ParagraphRun", "RunArray"]).and_then(E::as_array).unwrap()[0];
    assert_eq!(emitted_paragraph.path(&["ParagraphSheet", "Properties", "LeadingType"]).and_then(E::as_i64), Some(0));
    let global = tree(&file);
    assert_current_object(&global, saved);
    let object = object_for(&global, saved);
    for run in object.path(&["0", "6", "0"]).and_then(E::as_array).unwrap() {
        let properties = run.path(&["0", "0", "6"]).unwrap();
        assert_eq!(
            properties.get("13").and_then(E::as_i64),
            Some(1),
            "the replacement named sheet must inherit the current normal baseline, not SourceChar's old baseline"
        );
        assert_eq!(properties.get("35").and_then(E::as_i64), Some(0));
        assert_eq!(properties.get("38").and_then(E::as_i64), Some(0));
        assert_eq!(properties.get("52").and_then(E::as_bool), Some(true));
    }
    for run in object.path(&["0", "5", "0"]).and_then(E::as_array).unwrap() {
        let properties = run.path(&["0", "0", "5"]).unwrap();
        assert_eq!(properties.get("8").and_then(E::as_i64), Some(0), "global leading mode must follow the actual emitted local sheet");
        for (local_key, global_key) in [
            ("HyphenatedWordSize", "10"),
            ("PreHyphen", "11"),
            ("PostHyphen", "12"),
            ("Zone", "14"),
            ("WordSpacing", "17"),
            ("LetterSpacing", "18"),
            ("GlyphSpacing", "19"),
        ] {
            let expected = inherited_paragraph.iter().find(|(key, _)| *key == local_key).unwrap();
            assert_eq!(properties.get(global_key), Some(&expected.1), "current inherited local property {local_key} must survive document style export");
        }
    }
}

fn slot_boundary_document(slot_count: usize, empty_slots: &[usize]) -> Document {
    let mut doc = Document::new("text-slot-boundary", Size::new(16, 16), ColorMode::Rgb, SampleType::U8);
    doc.layers.push(Layer::new("Added", LayerContent::Text(new_type("N", Affine::translate(2.0, 12.0)))));
    // These are opaque archived objects with no owning layer. Construct the input bytes
    // directly: fixture creation must not call the allocator or global writer under test.
    let mut bytes = b"/1 << /1 [\n".to_vec();
    for index in 0..slot_count {
        if empty_slots.contains(&index) {
            bytes.extend_from_slice(b"<< >>\n");
        } else {
            bytes.extend_from_slice(format!("<< /ArchivedSlot {index} >>\n").as_bytes());
        }
    }
    bytes.extend_from_slice(b"] >>\n");
    doc.metadata.psd_global_blocks.push((*b"8BIM", *b"Txt2", Arc::new(bytes)));
    doc
}

fn assert_exhausted_slots_fail(extension: &str, options: photocraft_io::ExportOptions) {
    let doc = slot_boundary_document(photocraft_text::psd::MAX_TEXT_INDEX as usize + 1, &[]);
    let original_global = doc.metadata.psd_global_blocks.clone();
    match photocraft_io::export(&doc, &format!("exhausted.{extension}"), &options) {
        Err(photocraft_io::IoError::Unsupported(message)) => {
            assert!(!message.is_empty(), "the slot exhaustion must report an actionable error");
        }
        Err(error) => panic!("text slot exhaustion must be an unsupported conversion, got {error}"),
        Ok(result) => panic!("exhausted text slots must fail before returning bytes; {extension} returned {} bytes", result.bytes.len()),
    }
    assert_eq!(doc.metadata.psd_global_blocks, original_global, "a failed export must not rewrite its input metadata");
    assert_eq!(text(&doc, "Added").text, "N");
    assert!(text(&doc, "Added").psd_raw.is_none(), "export must not attach a partially assigned index to the input type");
}

#[test]
fn exhausted_text_object_slots_fail_psd_export_without_bytes() {
    assert_exhausted_slots_fail("psd", Default::default());
}

#[test]
fn exhausted_text_object_slots_fail_psb_export_without_bytes() {
    assert_exhausted_slots_fail("psb", Default::default());
}

#[test]
fn exhausted_text_object_slots_fail_layered_tiff_export_without_bytes() {
    assert_exhausted_slots_fail("tif", photocraft_io::ExportOptions { tiff_layers: true, ..Default::default() });
}

fn assert_slot_boundary_roundtrip(free_slot: usize) {
    let slot_count = photocraft_text::psd::MAX_TEXT_INDEX as usize + 1;
    let doc = slot_boundary_document(slot_count, &[free_slot]);
    for extension in ["psd", "psb"] {
        let bytes = photocraft_io::export(&doc, &format!("slot-boundary.{extension}"), &Default::default()).unwrap().bytes;
        let file = PsdFile::from_bytes(&bytes).unwrap();
        let back = photocraft_io::import(&format!("slot-boundary.{extension}"), &bytes).unwrap().document;
        let current = text(&back, "Added");
        let index = photocraft_text::psd::text_index(current.psd_raw.as_deref().unwrap()).expect("the emitted TextIndex must be in the supported range");
        assert_eq!(index as usize, free_slot);
        assert_eq!(current.text, "N");
        assert_transform(current.transform, Affine::translate(2.0, 12.0));
        let global = tree(&file);
        assert_current_object(&global, current);
        let objects = global.path(&["1", "1"]).and_then(E::as_array).unwrap();
        assert_eq!(objects.len(), slot_count, "saving must neither discard archived slots nor append an out-of-range slot");
        for (index, object) in objects.iter().enumerate() {
            if index != free_slot {
                assert_eq!(object.get("ArchivedSlot").and_then(E::as_i64), Some(index as i64), "archived object {index} changed");
            }
        }
        assert_eq!(back.layers.len(), 1, "archived objects must not become visible layers");
    }
}

#[test]
fn the_last_supported_text_object_slot_roundtrips_without_losing_archived_objects() {
    assert_slot_boundary_roundtrip(photocraft_text::psd::MAX_TEXT_INDEX as usize);
}

#[test]
fn a_free_text_object_hole_is_reused_even_with_an_occupied_tail_slot() {
    assert_slot_boundary_roundtrip(7);
}

#[test]
fn malformed_principal_text_indices_are_renumbered_to_a_valid_free_slot() {
    for invalid in [-1, photocraft_text::psd::MAX_TEXT_INDEX + 1, i32::MAX] {
        let mut doc = slot_boundary_document(3, &[1]);
        let current = text_mut(&mut doc, "Added");
        let mut tysh = photocraft_text::psd::parse_tysh(&photocraft_text::psd::build_tysh(current, 72.0, None)).unwrap();
        // Represent malformed imported data without using set_text_index, whose validation
        // must not determine which hostile values this input fixture can contain.
        tysh.text.items.iter_mut().find(|(key, _)| key.is("TextIndex")).unwrap().1 = D::Integer(invalid);
        let raw = Arc::new(photocraft_text::psd::write_tysh(&tysh));
        current.psd_raw = Some(raw.clone());
        doc.layers[0].psd_blocks.push((*b"TySh", raw));
        for extension in ["psd", "psb"] {
            let bytes = photocraft_io::export(&doc, &format!("malformed-index.{extension}"), &Default::default()).unwrap().bytes;
            let file = PsdFile::from_bytes(&bytes).unwrap();
            let back = photocraft_io::import(&format!("malformed-index.{extension}"), &bytes).unwrap().document;
            let current = text(&back, "Added");
            assert_eq!(photocraft_text::psd::text_index(current.psd_raw.as_deref().unwrap()), Some(1));
            assert_eq!(current.text, "N");
            assert_transform(current.transform, Affine::translate(2.0, 12.0));
            let global = tree(&file);
            assert_current_object(&global, current);
            let objects = global.path(&["1", "1"]).and_then(E::as_array).unwrap();
            assert_eq!(objects.len(), 3);
            for index in [0, 2] {
                assert_eq!(objects[index].get("ArchivedSlot").and_then(E::as_i64), Some(index as i64));
            }
        }
    }
}
