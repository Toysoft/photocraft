//! A global named ParagraphSheet parent is another paragraph sheet, not a character style.
//! These fixtures are synthetic: no user PSD, font asset, or native application is required.

use std::sync::Arc;

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::text::{CharStyle, FontFeature, TextRun};
use photocraft_doc::{Document, Layer, LayerContent, TextLayer};
use photocraft_geom::{Affine, Size};
use photocraft_psd::PsdFile;
use photocraft_text::engine_data::{self as ed, Value as E};

fn dict(items: Vec<(&str, E)>) -> E {
    E::Dict(items.into_iter().map(|(key, value)| (key.into(), value)).collect())
}

fn remove(value: &mut E, key: &str) {
    let E::Dict(items) = value else { panic!("dictionary required") };
    items.retain(|(candidate, _)| candidate != key);
}

fn encode(tree: &E) -> Vec<u8> {
    let E::Dict(items) = tree else { panic!("global dictionary required") };
    ed::write_bare(items)
}

fn source() -> Document {
    let mut doc = Document::new("paragraph-parent-save", Size::new(256, 192), ColorMode::Rgb, SampleType::U8);
    for (name, value, size, x, y) in [("editable", "TEST13", 48.0, 45.0, 70.0), ("sibling", "Caption1", 17.0, 25.0, 155.0)] {
        let style = CharStyle {
            font_family: "Inter".into(),
            postscript_name: Some("Inter-Regular".into()),
            size_pt: size,
            features: vec![FontFeature { tag: "calt".into(), value: 1 }],
            ..Default::default()
        };
        let mut text =
            TextLayer { text: value.into(), transform: Affine::translate(x, y), runs: vec![TextRun { len: value.len(), style }], ..Default::default() };
        text.sync_summary();
        photocraft_text::TextEngine::new().render_layer(&mut text, doc.resolution_dpi, doc.pixel_format());
        let mut layer = Layer::new(name, LayerContent::Text(text));
        layer.psd_blocks.push((*b"zZlc", Arc::new(vec![2, 3, 5, 7])));
        doc.layers.push(layer);
    }
    doc.metadata.psd_global_blocks.push((*b"8BIM", *b"zZgc", Arc::new(vec![11, 13, 17, 19])));
    doc
}

fn save(doc: &Document, extension: &str) -> PsdFile {
    let bytes = photocraft_io::export(doc, &format!("saved.{extension}"), &Default::default()).unwrap().bytes;
    PsdFile::from_bytes(&bytes).unwrap()
}

fn raw_global(file: &PsdFile) -> &[u8] {
    &file.global_blocks.iter().find(|block| &block.key == b"Txt2").unwrap().data
}

fn global(file: &PsdFile) -> E {
    photocraft_text::psd::parse_txt2(raw_global(file)).unwrap()
}

fn sheets_mut(tree: &mut E) -> &mut Vec<E> {
    let E::Array(sheets) = tree.get_mut("0").unwrap().get_mut("6").unwrap().get_mut("0").unwrap() else { panic!("named paragraph sheets required") };
    sheets
}

fn root_mut(tree: &mut E) -> &mut E {
    sheets_mut(tree)[0].get_mut("0").unwrap()
}

fn replace_global(file: &mut PsdFile, tree: &E) {
    let block = file.global_blocks.iter_mut().find(|block| &block.key == b"Txt2").unwrap();
    block.data = encode(tree);
    block.padding = Some(vec![0; (4 - block.data.len() % 4) % 4]);
}

fn reopen(file: &PsdFile) -> Document {
    photocraft_io::import("fixture.psd", &file.to_bytes().unwrap()).unwrap().document
}

fn text<'a>(doc: &'a Document, name: &str) -> &'a TextLayer {
    let LayerContent::Text(text) = &doc.layers.iter().find(|layer| layer.name == name).unwrap().content else { panic!("editable type required") };
    text
}

fn native_fixture(local_engine: bool) -> PsdFile {
    let mut file = save(&source(), "psd");
    let mut tree = global(&file);
    remove(root_mut(&mut tree), "6");
    root_mut(&mut tree).set("0", E::String("Normal RGB".into()));
    root_mut(&mut tree).set("UnmodeledRootProperty", E::Int(101));
    let mut child = sheets_mut(&mut tree)[0].clone();
    child.get_mut("0").unwrap().set("0", E::String("Derived paragraph".into()));
    child.get_mut("0").unwrap().set("6", E::Int(0));
    sheets_mut(&mut tree).push(child);
    // The native corpus stores this list in reverse order; it is not a per-slot ID map.
    // Parent references still address the named-sheet array above.
    tree.get_mut("0").unwrap().get_mut("6").unwrap().set("1", E::Array(vec![dict(vec![("0", E::Int(1))]), dict(vec![("0", E::Int(0))])]));
    tree.set("UnmodeledGlobalProperty", E::Int(103));
    // A current object's cache and frame must not be rebuilt merely to repair a resource edge.
    let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!("objects required") };
    for object in objects {
        object.set("UnmodeledObjectProperty", E::Int(107));
        let E::Array(runs) = object.get_mut("0").unwrap().get_mut("5").unwrap().get_mut("0").unwrap() else { panic!("paragraph runs required") };
        for run in runs {
            run.get_mut("0").unwrap().get_mut("0").unwrap().set("6", E::Int(1));
        }
        let frame = object.path(&["1", "0"]).and_then(E::as_array).unwrap()[0].get("0").unwrap().clone();
        object
            .get_mut("1")
            .unwrap()
            .set("2", E::Array(vec![dict(vec![("6", E::Array(vec![dict(vec![("10", frame)])])), ("UnmodeledCacheProperty", E::Int(109))])]));
    }
    replace_global(&mut file, &tree);
    if !local_engine {
        for layer in &mut file.layer_info.as_mut().unwrap().layers {
            let Some(block) = layer.block_mut(b"TySh") else { continue };
            let mut tysh = photocraft_text::psd::parse_tysh(&block.data).unwrap();
            tysh.text.items.retain(|(key, _)| !key.is("EngineData"));
            block.data = photocraft_text::psd::write_tysh(&tysh);
            // Native type blocks include their even alignment in the declared payload.
            if block.data.len() % 2 == 1 {
                block.data.push(0);
            }
        }
    }
    file
}

fn assert_acyclic(tree: &E) {
    let sheets = tree.path(&["0", "6", "0"]).and_then(E::as_array).unwrap();
    for start in 0..sheets.len() {
        let mut seen = std::collections::HashSet::new();
        let mut current = start;
        loop {
            assert!(seen.insert(current), "named paragraph parent cycle from slot {start}");
            let Some(parent) = sheets[current].path(&["0", "6"]) else { break };
            let parent = usize::try_from(parent.as_i64().expect("paragraph parent must be an integer")).unwrap();
            assert!(parent < sheets.len(), "named paragraph parent must exist");
            current = parent;
        }
    }
}

fn assert_current(actual: &Document, expected: &Document) {
    assert_eq!(actual.size, expected.size);
    assert_eq!(actual.layers.iter().map(|layer| &layer.name).collect::<Vec<_>>(), expected.layers.iter().map(|layer| &layer.name).collect::<Vec<_>>());
    for name in ["editable", "sibling"] {
        let (actual, expected) = (text(actual, name), text(expected, name));
        assert_eq!(actual.text, expected.text);
        assert_eq!(actual.transform, expected.transform);
        assert_eq!(actual.char_runs(), expected.char_runs());
        assert_eq!(actual.paragraph_runs(), expected.paragraph_runs());
    }
}

fn assert_other_blocks(file: &PsdFile) {
    assert_eq!(file.global_blocks.iter().find(|block| &block.key == b"zZgc").unwrap().data, [11, 13, 17, 19]);
    for layer in &file.layer_info.as_ref().unwrap().layers {
        assert_eq!(layer.block(b"zZlc").unwrap().data, [2, 3, 5, 7]);
    }
}

#[test]
fn fresh_export_has_a_parentless_normal_paragraph_and_valid_run_references() {
    for extension in ["psd", "psb"] {
        let file = save(&source(), extension);
        let tree = global(&file);
        assert_acyclic(&tree);
        assert!(tree.path(&["0", "6", "0"]).and_then(E::as_array).unwrap()[0].path(&["0", "6"]).is_none());
        for object in tree.path(&["1", "1"]).and_then(E::as_array).unwrap() {
            for run in object.path(&["0", "5", "0"]).and_then(E::as_array).unwrap() {
                assert_eq!(run.path(&["0", "0", "6"]).and_then(E::as_i64), Some(0), "run inheritance remains valid");
            }
        }
    }
}

#[test]
fn complete_noop_repairs_only_the_known_factory_self_reference() {
    for local in [true, false] {
        let mut fixture = native_fixture(local);
        let expected = global(&fixture);
        let native_expected = reopen(&fixture);
        let mut bad = expected.clone();
        root_mut(&mut bad).set("6", E::Int(0));
        replace_global(&mut fixture, &bad);
        let doc = reopen(&fixture);
        assert_current(&doc, &native_expected);
        for extension in ["psd", "psb"] {
            let saved = save(&doc, extension);
            assert_eq!(global(&saved), expected, "resource repair must preserve objects, caches, frames, derived styles, and opaque data");
            assert_acyclic(&global(&saved));
            assert_current(&reopen(&saved), &doc);
            for input in &fixture.layer_info.as_ref().unwrap().layers {
                let output = saved.layer_info.as_ref().unwrap().layers.iter().find(|layer| layer.name() == input.name()).unwrap();
                assert_eq!(output.block(b"TySh").unwrap().data, input.block(b"TySh").unwrap().data, "complete no-op must preserve each local type payload");
            }
            assert_other_blocks(&saved);
        }
    }
}

#[test]
fn same_length_text_edit_repairs_the_resource_without_restoring_old_text() {
    let mut fixture = native_fixture(true);
    let mut bad = global(&fixture);
    root_mut(&mut bad).set("6", E::Int(0));
    replace_global(&mut fixture, &bad);
    for extension in ["psd", "psb"] {
        let mut doc = reopen(&fixture);
        let (dpi, format) = (doc.resolution_dpi, doc.pixel_format());
        let LayerContent::Text(edited) = &mut doc.layers.iter_mut().find(|layer| layer.name == "editable").unwrap().content else { panic!() };
        edited.text = "TEST14".into();
        edited.runs[0].len = edited.text.len();
        edited.sync_summary();
        let (layout, rendered) = photocraft_text::TextEngine::new().render(edited, dpi, format);
        edited.cache = Some(rendered.surface);
        edited.psd_raw = Some(Arc::new(photocraft_text::psd::build_tysh(edited, dpi, layout.bounds())));
        let saved = save(&doc, extension);
        let tree = global(&saved);
        assert_acyclic(&tree);
        let back = reopen(&saved);
        assert_current(&back, &doc);
        let current = text(&back, "editable");
        let index = photocraft_text::psd::text_index(current.psd_raw.as_deref().unwrap()).unwrap() as usize;
        assert_eq!(tree.path(&["1", "1"]).and_then(E::as_array).unwrap()[index].path(&["0", "0"]).and_then(E::as_str), Some("TEST14\r"));
        assert_eq!(text(&back, "sibling").psd_raw, text(&doc, "sibling").psd_raw);
        assert_other_blocks(&saved);
    }
}

#[test]
fn valid_native_parentless_root_and_derived_style_remain_byte_identical() {
    let fixture = native_fixture(true);
    let doc = reopen(&fixture);
    for extension in ["psd", "psb"] {
        let saved = save(&doc, extension);
        assert_eq!(raw_global(&saved), raw_global(&fixture), "valid native named inheritance must remain untouched");
        assert_acyclic(&global(&saved));
        assert_current(&reopen(&saved), &doc);
    }
}

#[test]
fn a_missing_global_block_regenerates_acyclic_models_and_preserves_current_type() {
    let mut fixture = native_fixture(true);
    let expected = reopen(&fixture);
    fixture.global_blocks.retain(|block| &block.key != b"Txt2");
    let doc = reopen(&fixture);
    assert_current(&doc, &expected);
    for extension in ["psd", "psb"] {
        let saved = save(&doc, extension);
        let tree = global(&saved);
        assert_acyclic(&tree);
        let back = reopen(&saved);
        assert_current(&back, &expected);
        for name in ["editable", "sibling"] {
            let current = text(&back, name);
            let index = photocraft_text::psd::text_index(current.psd_raw.as_deref().unwrap()).unwrap() as usize;
            let value = format!("{}\r", current.text);
            assert_eq!(tree.path(&["1", "1"]).and_then(E::as_array).unwrap()[index].path(&["0", "0"]).and_then(E::as_str), Some(value.as_str()));
        }
        assert_other_blocks(&saved);
    }
}

#[test]
fn unknown_named_self_and_mutual_cycles_fail_export_without_a_partial_result() {
    for (name, parent) in [("Custom paragraph", 0), ("Custom paragraph", 1), ("Normal RGB", 1)] {
        let mut fixture = native_fixture(true);
        let mut tree = global(&fixture);
        root_mut(&mut tree).set("0", E::String(name.into()));
        root_mut(&mut tree).set("6", E::Int(parent));
        replace_global(&mut fixture, &tree);
        let doc = reopen(&fixture);
        for extension in ["psd", "psb"] {
            assert!(
                photocraft_io::export(&doc, &format!("rejected.{extension}"), &Default::default()).is_err(),
                "an unknown cyclic style must not produce an unsafe file"
            );
        }
    }
}

#[test]
fn an_unreferenced_named_paragraph_cycle_is_not_preserved_for_later_native_editing() {
    let mut fixture = native_fixture(true);
    let mut tree = global(&fixture);
    let mut unused = sheets_mut(&mut tree)[1].clone();
    unused.get_mut("0").unwrap().set("0", E::String("Unused cyclic paragraph".into()));
    unused.get_mut("0").unwrap().set("6", E::Int(2));
    sheets_mut(&mut tree).push(unused);
    replace_global(&mut fixture, &tree);
    let doc = reopen(&fixture);
    for extension in ["psd", "psb"] {
        assert!(photocraft_io::export(&doc, &format!("rejected.{extension}"), &Default::default()).is_err());
    }
}

#[test]
fn invalid_used_paragraph_run_parents_fail_both_container_exports() {
    for parent in [E::Int(-1), E::Int(99), E::Real(0.0), E::Real(0.5), E::Bool(false)] {
        let mut fixture = native_fixture(true);
        let mut tree = global(&fixture);
        let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
        let E::Array(runs) = objects[0].get_mut("0").unwrap().get_mut("5").unwrap().get_mut("0").unwrap() else { panic!() };
        runs[0].get_mut("0").unwrap().get_mut("0").unwrap().set("6", parent);
        replace_global(&mut fixture, &tree);
        let doc = reopen(&fixture);
        for extension in ["psd", "psb"] {
            assert!(
                photocraft_io::export(&doc, &format!("rejected.{extension}"), &Default::default()).is_err(),
                "an invalid used run parent must not be emitted"
            );
        }
    }
    let mut fixture = native_fixture(true);
    let mut tree = global(&fixture);
    sheets_mut(&mut tree).clear();
    replace_global(&mut fixture, &tree);
    let doc = reopen(&fixture);
    for extension in ["psd", "psb"] {
        assert!(
            photocraft_io::export(&doc, &format!("rejected.{extension}"), &Default::default()).is_err(),
            "a used run cannot refer to an empty named-sheet array"
        );
    }
}
