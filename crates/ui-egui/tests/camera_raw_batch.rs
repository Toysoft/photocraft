//! Several raws dropped on the window at once open in one Camera Raw dialog with a filmstrip:
//! thumbnails clicked (Ctrl-click adds to the selection), a value typed into a slider's box goes
//! to every selected photo, Open opens the selected ones, Cancel none; other files dropped with
//! them open as usual. Through the real shell, as a user drives it.
use egui::{Event, Modifiers, PointerButton, Pos2, accesskit::Role, pos2, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use photocraft_ui_egui::{PhotocraftApp, Services, control, theme::ThemeKind};
use serde_json::{Value, json};

#[derive(Debug)]
struct Dropped(std::path::PathBuf);

impl egui::DroppedFile for Dropped {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        Err("read from the path".into())
    }
}

fn dng() -> Vec<u8> {
    use photocraft_raw::testgen::{DngSpec, mosaic, scene};
    let (w, h) = (96, 64);
    let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
    spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
    spec.build()
}

fn png() -> Vec<u8> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 8, "height": 6, "background": "white"})).unwrap();
    photocraft_io::export(&s.active().unwrap().doc, "p.png", &Default::default()).unwrap().bytes
}

/// `names` in a fresh directory: the `.dng`s are raws, the rest PNGs.
fn files(tag: &str, names: &[&str]) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
    let dir = std::env::temp_dir().join(format!("photocraft-raw-batch-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, other) = (dng(), png());
    let paths = names
        .iter()
        .map(|name| {
            let path = dir.join(name);
            std::fs::write(&path, if name.ends_with(".dng") { &raw } else { &other }).unwrap();
            path
        })
        .collect();
    (dir, paths)
}

fn harness() -> Harness<'static, PhotocraftApp> {
    let mut h = Harness::builder().with_step_dt(1.0 / 60.0).with_size(vec2(1632.0, 1046.0)).build_eframe(|cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::ProMedium);
        let services = Services {
            import: Some(Box::new(|name: &str, b: &[u8], opts: &photocraft_io::ImportOptions| {
                photocraft_io::import_with_options(name, b, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            ..Default::default()
        };
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
        app.set_theme(&cc.egui_ctx, ThemeKind::ProMedium);
        app
    });
    h.run_steps(4);
    h
}

fn drop_files(h: &mut Harness<'_, PhotocraftApp>, paths: &[std::path::PathBuf]) {
    for p in paths {
        h.input_mut().dropped_files.push(std::sync::Arc::new(Dropped(p.clone())));
    }
    h.run_steps(6);
}

fn camera_raw(h: &Harness<'_, PhotocraftApp>) -> Value {
    control::inspect(h.state(), &h.ctx)["cameraRaw"].clone()
}

fn names(h: &Harness<'_, PhotocraftApp>) -> Vec<String> {
    h.state().session.documents().iter().map(|d| d.doc.name.clone()).collect()
}

fn click(h: &mut Harness<'_, PhotocraftApp>, p: Pos2, modifiers: Modifiers) {
    h.event(Event::ModifiersChanged(modifiers));
    h.event(Event::PointerMoved(p));
    for pressed in [true, false] {
        h.event(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers });
    }
    h.run_steps(1);
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.run_steps(3);
}

fn thumbnail(h: &Harness<'_, PhotocraftApp>, name: &str) -> Pos2 {
    h.get_by_role_and_label(Role::Button, name).rect().center()
}

fn snapshot(h: &mut Harness<'_, PhotocraftApp>, name: &str) {
    if let Ok(dir) = std::env::var("PHOTOCRAFT_RAW_BATCH_SNAPSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        h.render().unwrap().save(std::path::Path::new(&dir).join(format!("{name}.png"))).unwrap();
    }
}

#[test]
fn dropped_raws_share_one_dialog_and_open_the_selected_photos_with_their_settings() {
    let (dir, paths) = files("open", &["a.dng", "b.dng", "c.dng", "logo.png"]);
    let mut h = harness();
    drop_files(&mut h, &paths);
    let cr = camera_raw(&h);
    let items = cr["filmstrip"]["items"].as_array().unwrap().clone();
    assert_eq!(items.iter().map(|i| i["name"].as_str().unwrap()).collect::<Vec<_>>(), ["a.dng", "b.dng", "c.dng"]);
    assert_eq!(cr["openingRaw"]["name"], "a.dng");
    assert_eq!(names(&h), ["a.dng", "logo.png"], "only the shown raw is developed; the PNG opens as usual");
    for name in ["a.dng", "b.dng", "c.dng"] {
        assert!(h.query_by_role_and_label(Role::Button, name).is_some(), "a thumbnail for {name}");
    }
    // Visit c (its thumbnail shows its edits), then back to a.
    let at = thumbnail(&h, "c.dng");
    click(&mut h, at, Modifiers::NONE);
    assert_eq!(camera_raw(&h)["openingRaw"]["name"], "c.dng");
    let at = thumbnail(&h, "a.dng");
    click(&mut h, at, Modifiers::NONE);
    assert_eq!(camera_raw(&h)["filmstrip"]["selected"], json!([0]));
    // Ctrl-click adds c to the selection and shows it.
    let at = thumbnail(&h, "c.dng");
    click(&mut h, at, Modifiers::COMMAND);
    let cr = camera_raw(&h);
    assert_eq!(cr["filmstrip"]["selected"], json!([0, 2]));
    assert_eq!(cr["openingRaw"]["name"], "c.dng");
    // A value typed into Contrast's box goes to both selected photos, not to b.
    let slider = h.get_by_role_and_label(Role::Slider, "Contrast").rect();
    click(&mut h, pos2(slider.right() - 24.0, slider.top() - 5.0), Modifiers::NONE);
    h.event(Event::Key { key: egui::Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND });
    h.event(Event::Text("35".into()));
    h.event(Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.run_steps(4);
    let cr = camera_raw(&h);
    assert_eq!(cr["params"]["contrast"], 35.0);
    assert_eq!(cr["filmstrip"]["items"][0]["settings"]["contrast"], 35.0, "{}", cr["filmstrip"]);
    assert_eq!(cr["filmstrip"]["items"][2]["settings"]["contrast"], 35.0);
    assert!(cr["filmstrip"]["items"][1]["settings"].get("contrast").is_none(), "b is not selected");
    assert_eq!(cr["filmstrip"]["selected"], json!([0, 2]), "typing kept the selection (Ctrl+A went to the field)");
    snapshot(&mut h, "filmstrip");
    // Open (2): a and c, each with Contrast 35; b is not opened.
    h.get_by_role_and_label(Role::Button, "Open (2)").click();
    h.run_steps(4);
    assert!(camera_raw(&h).is_null(), "Camera Raw closed");
    assert_eq!(names(&h), ["a.dng", "c.dng", "logo.png"], "the photos' tabs in filmstrip order");
    for st in h.state().session.documents().iter().filter(|st| st.doc.name.ends_with(".dng")) {
        assert_eq!(st.saved_revision, st.revision, "{} opens unmodified", st.doc.name);
        assert_eq!(st.history.past_len(), 1, "{}: Contrast is one Camera Raw step, as for one raw", st.doc.name);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn removing_and_cancelling_open_nothing_and_leave_the_files_alone() {
    let (dir, paths) = files("cancel", &["a.dng", "b.dng", "c.dng"]);
    let mut h = harness();
    h.state_mut().run("file.new", json!({"width": 20, "height": 20})).unwrap();
    drop_files(&mut h, &paths);
    let at = thumbnail(&h, "b.dng");
    click(&mut h, at, Modifiers::NONE);
    assert_eq!(names(&h).len(), 3, "a and b were shown");
    // b's × takes it out of the strip (its document closes); the file stays.
    let cell = h.get_by_role_and_label(Role::Button, "b.dng").rect();
    h.event(Event::PointerMoved(cell.center()));
    h.run_steps(2);
    h.get_by_role_and_label(Role::Button, "Remove from Filmstrip").click();
    h.run_steps(4);
    let cr = camera_raw(&h);
    assert_eq!(cr["filmstrip"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(cr["openingRaw"]["name"], "c.dng");
    assert!(paths[1].exists());
    assert_eq!(std::fs::read(&paths[1]).unwrap(), dng());
    // Ctrl+A, then Delete: every photo goes, which is a Cancel.
    h.key_press_modifiers(Modifiers::COMMAND, egui::Key::A);
    h.run_steps(2);
    assert_eq!(camera_raw(&h)["filmstrip"]["selected"], json!([0, 1]));
    snapshot(&mut h, "filmstrip-all");
    h.get_by_role_and_label(Role::Button, "Cancel").click();
    h.run_steps(4);
    assert!(camera_raw(&h).is_null());
    assert_eq!(names(&h), ["Untitled"], "nothing the filmstrip opened is left");
    for p in &paths {
        assert_eq!(std::fs::read(p).unwrap(), dng(), "{} untouched", p.display());
    }
    // Dropped again: Delete with everything selected empties the strip (also nothing opens).
    drop_files(&mut h, &paths[..2]);
    h.key_press_modifiers(Modifiers::COMMAND, egui::Key::A);
    h.key_press(egui::Key::Delete);
    h.run_steps(3);
    assert!(camera_raw(&h).is_null());
    assert_eq!(names(&h), ["Untitled"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A raw that can't be read stays in the strip with its error (no dialog for it, the others
/// unaffected), and Open skips it.
#[test]
fn a_broken_raw_shows_its_error_and_the_others_still_open() {
    let (dir, paths) = files("broken", &["a.dng", "c.dng"]);
    let broken = dir.join("b.dng");
    std::fs::write(&broken, b"not a raw file").unwrap();
    let mut h = harness();
    drop_files(&mut h, &[paths[0].clone(), broken.clone(), paths[1].clone()]);
    let at = thumbnail(&h, "b.dng");
    click(&mut h, at, Modifiers::NONE);
    let cr = camera_raw(&h);
    assert_eq!(cr["filmstrip"]["items"][1]["state"], "failed", "{cr}");
    assert!(cr["filmstrip"]["items"][1]["error"].is_string());
    assert!(cr["params"].is_null(), "nothing to edit for it");
    snapshot(&mut h, "filmstrip-failed");
    // Arrows still step through the strip.
    h.key_press(egui::Key::ArrowRight);
    h.run_steps(3);
    assert_eq!(camera_raw(&h)["openingRaw"]["name"], "c.dng");
    h.key_press_modifiers(Modifiers::COMMAND, egui::Key::A);
    h.run_steps(2);
    // Three selected, one of which can't open.
    h.get_by_role_and_label(Role::Button, "Open (2)").click();
    h.run_steps(4);
    assert!(camera_raw(&h).is_null());
    assert_eq!(names(&h), ["a.dng", "c.dng"]);
    let _ = std::fs::remove_dir_all(&dir);
}
