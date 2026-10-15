//! Photoshop may save the complete text model only in the document's Txt2 block.

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::text::{CharStyle, FontFeature, Kerning, ParagraphRun, ParagraphStyle, TextAlign, TextRun, TextShape};
use photocraft_doc::{Document, Layer, LayerContent, TextLayer};
use photocraft_geom::{Affine, Size};
use photocraft_psd::PsdFile;
use photocraft_psd::descriptor::Value as D;
use photocraft_text::TextEngine;

fn source() -> Document {
    let mut doc = Document::new("global-only-type", Size::new(320, 240), ColorMode::Rgb, SampleType::U8);
    doc.resolution_dpi = 144.0;
    let first = CharStyle {
        font_family: "Inter".into(),
        postscript_name: Some("Inter-Regular".into()),
        size_pt: 18.0,
        leading_pt: Some(23.0),
        tracking: 85.0,
        baseline_shift_pt: 2.5,
        horizontal_scale: 0.9,
        color: photocraft_color::Color::rgba(0.2, 0.4, 0.7, 1.0),
        features: vec![FontFeature { tag: "ss03".into(), value: 1 }],
        ..Default::default()
    };
    let second = CharStyle {
        font_family: "Inter".into(),
        postscript_name: Some("Inter-Bold".into()),
        weight: 700,
        size_pt: 31.0,
        kerning: Kerning::Optical,
        underline: true,
        ..Default::default()
    };
    let value = "A\u{1f600}\nBC";
    let mut text = TextLayer {
        text: value.into(),
        transform: Affine { m: [0.9, 0.2, -0.1, 1.1, 25.0, 55.0] },
        runs: vec![TextRun { len: "A\u{1f600}\n".len(), style: first }, TextRun { len: 2, style: second }],
        paragraphs: vec![
            ParagraphRun {
                len: "A\u{1f600}\n".len(),
                style: ParagraphStyle { align: TextAlign::Right, start_indent_pt: 3.5, space_after_pt: 4.0, ..Default::default() },
            },
            ParagraphRun { len: 2, style: ParagraphStyle { align: TextAlign::Center, first_line_indent_pt: 1.5, auto_leading: 1.6, ..Default::default() } },
        ],
        ..Default::default()
    };
    text.sync_summary();
    TextEngine::new().render_layer(&mut text, doc.resolution_dpi, doc.pixel_format());
    doc.layers.push(Layer::new("type", LayerContent::Text(text)));
    doc
}

fn text(doc: &Document) -> &TextLayer {
    let LayerContent::Text(text) = &doc.layers[0].content else { panic!("editable type required") };
    text
}

fn saved(doc: &Document) -> PsdFile {
    PsdFile::from_bytes(&photocraft_io::export(doc, "global.psd", &Default::default()).unwrap().bytes).unwrap()
}

fn remove_local(file: &mut PsdFile, malformed: bool) {
    let block = file.layer_info.as_mut().unwrap().layers[0].block_mut(b"TySh").unwrap();
    let mut tysh = photocraft_text::psd::parse_tysh(&block.data).unwrap();
    tysh.text.items.retain(|(key, _)| !key.is("EngineData"));
    if malformed {
        tysh.text.items.push((photocraft_psd::descriptor::Id::new("EngineData"), D::RawData(b"<< /broken [".to_vec())));
    }
    block.data = photocraft_text::psd::write_tysh(&tysh);
}

fn reopen(file: &PsdFile) -> Document {
    photocraft_io::import("global.psd", &file.to_bytes().unwrap()).unwrap().document
}

fn assert_model(actual: &TextLayer, expected: &TextLayer) {
    assert_eq!(actual.text, expected.text);
    assert_eq!(actual.transform, expected.transform, "global styles must preserve local TySh geometry");
    assert_eq!(actual.shape, expected.shape);
    assert_eq!(actual.paragraph_runs(), expected.paragraph_runs());
    let actual = actual.char_runs();
    let expected = expected.char_runs();
    assert_eq!(actual.len(), expected.len());
    for (a, e) in actual.iter().zip(expected) {
        assert_eq!(a.len, e.len, "UTF-16 global lengths must become whole UTF-8 characters");
        assert_eq!(a.style.postscript_name, e.style.postscript_name);
        assert_eq!(a.style.size_pt, e.style.size_pt, "font size uses document DPI");
        assert_eq!(a.style.leading_pt, e.style.leading_pt);
        assert_eq!(a.style.tracking, e.style.tracking);
        assert_eq!(a.style.baseline_shift_pt, e.style.baseline_shift_pt);
        assert_eq!(a.style.horizontal_scale, e.style.horizontal_scale);
        assert_eq!(a.style.color, e.style.color);
        assert_eq!(a.style.underline, e.style.underline);
        assert_eq!(a.style.kerning, e.style.kerning);
        for feature in &e.style.features {
            assert!(a.style.features.contains(feature), "missing feature {feature:?}");
        }
    }
}

#[test]
fn missing_or_malformed_local_engine_uses_current_global_runs_at_document_dpi() {
    let source = source();
    let baseline = reopen(&saved(&source));
    for malformed in [false, true] {
        let mut file = saved(&source);
        remove_local(&mut file, malformed);
        let mut imported = reopen(&file);
        assert_model(text(&imported), text(&baseline));
        let before = TextEngine::new().layout(text(&baseline), 144.0);
        let after = TextEngine::new().layout(text(&imported), 144.0);
        assert_eq!(after.glyphs.iter().map(|glyph| glyph.id).collect::<Vec<_>>(), before.glyphs.iter().map(|glyph| glyph.id).collect::<Vec<_>>());
        let fmt = imported.pixel_format();
        let LayerContent::Text(current) = &mut imported.layers[0].content else { panic!() };
        current.text.push('!');
        current.runs.last_mut().unwrap().len += 1;
        current.paragraphs.last_mut().unwrap().len += 1;
        TextEngine::new().render_layer(current, 144.0, fmt);
        current.psd_raw = Some(std::sync::Arc::new(photocraft_text::psd::build_tysh(current, 144.0, None)));
        let twice = reopen(&saved(&imported));
        assert_model(text(&twice), text(&imported));
        assert_eq!(text(&twice).text, "A\u{1f600}\nBC!");
    }
}

#[test]
fn valid_local_engine_keeps_its_styles_when_matching_global_styles_differ() {
    let source = source();
    let mut file = saved(&source);
    let block = file.global_blocks.iter_mut().find(|block| &block.key == b"Txt2").unwrap();
    let mut tree = photocraft_text::psd::parse_txt2(&block.data).unwrap();
    let objects = tree.get_mut("1").unwrap().get_mut("1").unwrap();
    let photocraft_text::engine_data::Value::Array(objects) = objects else { panic!() };
    let styles = objects[0].get_mut("0").unwrap().get_mut("6").unwrap().get_mut("0").unwrap();
    let photocraft_text::engine_data::Value::Array(styles) = styles else { panic!() };
    for style in styles {
        style.get_mut("0").unwrap().get_mut("0").unwrap().get_mut("6").unwrap().set("1", photocraft_text::engine_data::Value::Real(200.0));
    }
    let photocraft_text::engine_data::Value::Dict(items) = tree else { panic!() };
    block.data = photocraft_text::engine_data::write_bare(&items);
    assert_model(text(&reopen(&file)), text(&reopen(&saved(&source))));
}

#[test]
fn stale_global_text_cannot_supply_styles_for_a_different_local_text() {
    let mut file = saved(&source());
    remove_local(&mut file, false);
    let block = file.layer_info.as_mut().unwrap().layers[0].block_mut(b"TySh").unwrap();
    let mut tysh = photocraft_text::psd::parse_tysh(&block.data).unwrap();
    tysh.text.items.iter_mut().find(|(key, _)| key.is("Txt ")).unwrap().1 = D::Text(photocraft_psd::descriptor::UnicodeString::new("current"));
    block.data = photocraft_text::psd::write_tysh(&tysh);
    let imported = reopen(&file);
    assert_eq!(text(&imported).text, "current");
    assert!(text(&imported).char_runs().iter().all(|run| run.style.postscript_name.is_none() && run.style.size_pt == 12.0));
}

#[test]
fn global_only_box_text_keeps_its_box_and_local_transform() {
    let mut source = source();
    let LayerContent::Text(current) = &mut source.layers[0].content else { panic!() };
    current.shape = TextShape::Box { x: 4.0, y: 6.0, width: 170.0, height: 130.0 };
    let baseline = reopen(&saved(&source));
    let mut file = saved(&source);
    remove_local(&mut file, false);
    assert_model(text(&reopen(&file)), text(&baseline));
}
