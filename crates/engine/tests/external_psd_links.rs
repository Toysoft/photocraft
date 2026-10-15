//! Real external PSD edits, including Edit Contents, must reach every linked parent.
use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::{Document, Layer, LayerContent, SmartObject, SmartSource};
use photocraft_engine::Session;
use photocraft_geom::{Affine, Size};
use photocraft_raster::Surface;
use serde_json::json;

fn source(color: [f32; 4]) -> Document {
    let mut d = Document::new("source", Size::new(12, 10), ColorMode::Rgb, SampleType::U8);
    let mut pixels = Surface::new(d.pixel_format());
    pixels.fill_rect(d.bounds(), &color);
    d.layers.push(Layer::new("color", LayerContent::Raster(pixels)));
    d
}
fn save(d: &Document, p: &std::path::Path) {
    std::fs::write(p, photocraft_io::export(d, "psd", &Default::default()).unwrap().bytes).unwrap();
}
fn parent(path: &str, src: &Document) -> Document {
    let mut d = Document::new("parent", src.size, src.mode, src.depth);
    let cache = src.layers[0].surface().unwrap().clone();
    let sm = SmartObject::new(SmartSource::Linked { path: path.into() }, Affine::IDENTITY, Some(cache));
    d.layers.push(Layer::new("front", LayerContent::Smart(sm.clone())));
    d.layers.push(Layer::new("back", LayerContent::Smart(sm)));
    d
}
fn roundtrip(d: &Document) -> Document {
    let out = photocraft_io::export(d, "psd", &Default::default()).unwrap();
    assert!(!out.warnings.iter().any(|w| w.contains("embedded")));
    photocraft_io::import("parent.psd", &out.bytes).unwrap().document
}
fn assert_path(d: &Document, path: &str) {
    for l in &d.layers {
        let LayerContent::Smart(sm) = &l.content else { panic!("not smart") };
        let SmartSource::Linked { path: actual } = &sm.source else { panic!("source became embedded") };
        assert_eq!(actual.replace('\\', "/"), path.replace('\\', "/"));
    }
}
#[test]
fn edit_contents_saves_real_psd_keeps_links_and_updates_other_parent() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("bron ü % #.psd");
    let path = file.to_string_lossy().into_owned();
    let original = source([1.0, 0.0, 0.0, 1.0]);
    save(&original, &file);
    let first = roundtrip(&parent(&path, &original));
    let second = roundtrip(&parent(&path, &original));
    let mut s = Session::new();
    s.add_document(first, None);
    s.add_document(second, None);
    s.execute("layer.smartObjects.editContents", json!({})).unwrap();
    s.edit("green", |d, _| {
        let bounds = d.bounds();
        d.layers[0].surface_mut().unwrap().fill_rect(bounds, &[0.0, 1.0, 0.0, 1.0]);
        Ok(())
    })
    .unwrap();
    s.execute("layer.smartObjects.saveContents", json!({})).unwrap();
    let bytes = std::fs::read(&file).unwrap();
    assert!(bytes.starts_with(b"8BPS"));
    let changed = photocraft_io::import("source.psd", &bytes).unwrap().document;
    assert!(photocraft_compose::flatten(&changed).px[0][1] > 0.99);
    for st in s.documents().iter().take(2) {
        assert_path(&roundtrip(&st.doc), &path);
    }
    // Every parent refreshes the same file, without embedding or changing its transform.
    s.execute("file.close", json!({})).unwrap();
    assert!(s.set_active(0));
    s.execute("layer.smartObjects.updateAllModifiedContent", json!({})).unwrap();
    assert!(photocraft_compose::flatten(&s.active().unwrap().doc).px[0][1] > 0.99);
}

#[test]
fn automatic_watcher_follows_imported_psd_external_source() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("source.psd");
    let path = file.to_string_lossy().into_owned();
    let red = source([1.0, 0.0, 0.0, 1.0]);
    save(&red, &file);
    let mut s = Session::new();
    s.add_document(roundtrip(&parent(&path, &red)), None);
    s.execute("layer.smartObjects.updateChanged", json!({})).unwrap();
    let mut green = source([0.0, 1.0, 0.0, 1.0]);
    green.name = "changed source".into();
    save(&green, &file);
    let result = s.execute("layer.smartObjects.updateChanged", json!({})).unwrap();
    assert!(!result["updated"].as_array().unwrap().is_empty());
    assert!(photocraft_compose::flatten(&s.active().unwrap().doc).px[0][1] > 0.99);
    assert_path(&roundtrip(&s.active().unwrap().doc), &path);
}

#[test]
fn source_modified_while_parent_closed_refreshes_on_first_scan() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("closed.psd");
    let path = file.to_string_lossy().into_owned();
    let red = source([1.0, 0.0, 0.0, 1.0]);
    save(&red, &file);
    let cached = roundtrip(&parent(&path, &red));
    std::thread::sleep(std::time::Duration::from_millis(10));
    save(&source([0.0, 1.0, 0.0, 1.0]), &file);
    let mut s = Session::new();
    s.add_document(cached, None);
    let result = s.execute("layer.smartObjects.updateChanged", json!({})).unwrap();
    assert!(!result["updated"].as_array().unwrap().is_empty());
    assert!(photocraft_compose::flatten(&s.active().unwrap().doc).px[0][1] > 0.99);
    let back = roundtrip(&s.active().unwrap().doc);
    let mut fresh = Session::new();
    fresh.add_document(back, None);
    let result = fresh.execute("layer.smartObjects.updateChanged", json!({})).unwrap();
    assert!(result["updated"].as_array().unwrap().is_empty(), "fresh cache must not re-render again");
}

#[test]
fn linked_contents_save_rechecks_write_permission_before_changing_source() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("protected.psd");
    let path = file.to_string_lossy().into_owned();
    let red = source([1.0, 0.0, 0.0, 1.0]);
    save(&red, &file);
    let original = std::fs::read(&file).unwrap();
    let mut s = Session::new();
    s.add_document(roundtrip(&parent(&path, &red)), None);
    s.execute("layer.smartObjects.editContents", json!({})).unwrap();
    s.authorize = Some(|_, params| if params.get("path").is_some() { Err(photocraft_engine::EngineError::Other("write denied".into())) } else { Ok(()) });
    assert!(s.execute("layer.smartObjects.saveContents", json!({})).is_err());
    assert_eq!(std::fs::read(file).unwrap(), original);
}
