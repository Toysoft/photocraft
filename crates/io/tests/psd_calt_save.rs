//! PSD must materialize the shaper's implicit contextual-alternates default without turning
//! it off, including when new type is added to a document whose native defaults are off.

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::text::{CharStyle, FontFeature, TextRun};
use photocraft_doc::{Document, Layer, LayerContent, TextLayer};
use photocraft_geom::{Affine, Size};
use photocraft_psd::PsdFile;
use photocraft_text::TextEngine;
use photocraft_text::engine_data::Value as E;

fn layer(name: &str, calt: Option<u16>, y: f64, doc: &Document) -> Layer {
    let features = calt.map(|value| FontFeature { tag: "calt".into(), value }).into_iter().collect();
    let style = CharStyle { font_family: "Inter".into(), size_pt: 24.0, features, ..Default::default() };
    let mut text = TextLayer { text: "->".into(), runs: vec![TextRun { len: 2, style }], transform: Affine::translate(16.0, y), ..Default::default() };
    text.sync_summary();
    // Bundled Inter only, so the assertion cannot depend on the user's installed fonts.
    TextEngine::new().render_layer(&mut text, doc.resolution_dpi, doc.pixel_format());
    Layer::new(name, LayerContent::Text(text))
}

fn document(calt: Option<u16>) -> Document {
    let mut doc = Document::new("contextual-alternates", Size::new(160, 120), ColorMode::Rgb, SampleType::U8);
    doc.layers.push(layer("type", calt, 40.0, &doc));
    doc
}

fn text<'a>(doc: &'a Document, name: &str) -> &'a TextLayer {
    let LayerContent::Text(text) = &doc.layers.iter().find(|layer| layer.name == name).unwrap().content else { panic!("expected editable type") };
    text
}

fn glyph_ids(doc: &Document, name: &str) -> Vec<u32> {
    TextEngine::new().layout(text(doc, name), doc.resolution_dpi).glyphs.iter().map(|glyph| glyph.id).collect()
}

fn save_and_reopen(doc: &Document) -> (PsdFile, Document) {
    let output = photocraft_io::export(doc, "calt.psd", &Default::default()).unwrap();
    assert!(!output.warnings.iter().any(|warning| warning.contains("text layer")), "{:?}", output.warnings);
    (PsdFile::from_bytes(&output.bytes).unwrap(), photocraft_io::import("calt.psd", &output.bytes).unwrap().document)
}

fn global(file: &PsdFile) -> E {
    let block = file.global_blocks.iter().find(|block| &block.key == b"Txt2").unwrap();
    photocraft_text::psd::parse_txt2(&block.data).unwrap()
}

fn object<'a>(global: &'a E, text: &TextLayer) -> &'a E {
    let index = photocraft_text::psd::text_index(text.psd_raw.as_deref().unwrap()).unwrap() as usize;
    &global.path(&["1", "1"]).and_then(E::as_array).unwrap()[index]
}

#[test]
fn implicit_and_explicit_contextual_alternates_keep_their_glyphs_after_two_psd_saves() {
    let implicit_ids = glyph_ids(&document(None), "type");
    let off_ids = glyph_ids(&document(Some(0)), "type");
    assert_eq!(implicit_ids.len(), 1, "bundled Inter turns -> into an arrow by default");
    assert_eq!(off_ids.len(), 2, "explicit calt 0 must keep the two original glyphs");
    assert_ne!(implicit_ids, off_ids);

    for setting in [None, Some(0), Some(1)] {
        let source = document(setting);
        let before = glyph_ids(&source, "type");
        let (file, once) = save_and_reopen(&source);
        assert_eq!(glyph_ids(&once, "type"), before, "first PSD save changed calt {setting:?}");
        let tree = global(&file);
        let styles = object(&tree, text(&once, "type")).path(&["0", "6", "0"]).and_then(E::as_array).unwrap();
        assert!(styles.iter().all(|style| style.path(&["0", "0", "6", "20"]).and_then(E::as_bool) == Some(setting != Some(0))));
        let (_, twice) = save_and_reopen(&once);
        assert_eq!(glyph_ids(&twice, "type"), before, "second PSD save changed calt {setting:?}");
        assert_eq!(text(&twice, "type").text, "->");
    }
}

#[test]
fn new_implicit_type_overrides_an_existing_off_default_without_changing_siblings() {
    let (original_file, mut doc) = save_and_reopen(&document(Some(0)));
    let original_global = global(&original_file);
    assert_eq!(original_global.path(&["1", "2", "20"]).and_then(E::as_bool), Some(false));
    let old_object = object(&original_global, text(&doc, "type")).clone();
    let old_tysh = text(&doc, "type").psd_raw.clone().unwrap();
    let old_ids = glyph_ids(&doc, "type");
    doc.layers.insert(0, layer("new", None, 80.0, &doc));
    let new_ids = glyph_ids(&doc, "new");
    assert_eq!((old_ids.len(), new_ids.len()), (2, 1));

    let (file, once) = save_and_reopen(&doc);
    let saved_global = global(&file);
    assert_eq!(saved_global.path(&["1", "2"]), original_global.path(&["1", "2"]), "native document defaults must stay unchanged");
    assert_eq!(saved_global.path(&["0", "5"]), original_global.path(&["0", "5"]), "native named styles must stay unchanged");
    assert_eq!(object(&saved_global, text(&once, "type")), &old_object, "untouched native sibling must stay unchanged");
    assert_eq!(text(&once, "type").psd_raw.as_deref(), Some(old_tysh.as_ref()));
    assert_eq!(glyph_ids(&once, "type"), old_ids);
    assert_eq!(glyph_ids(&once, "new"), new_ids);
    let (_, twice) = save_and_reopen(&once);
    assert_eq!(glyph_ids(&twice, "type"), old_ids);
    assert_eq!(glyph_ids(&twice, "new"), new_ids);
}
