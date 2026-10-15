//! Save identity belongs to the editable document, after a successful write only.

use crate::{ExportSettings, PhotocraftApp, Services, menus};
use photocraft_engine::Session;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn app() -> PhotocraftApp {
    let mut app = PhotocraftApp::new(
        Session::new(),
        Services {
            import: Some(Box::new(|name, bytes, max_svg_group_depth| {
                photocraft_io::import_with_svg_group_depth(name, bytes, max_svg_group_depth).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            export: Some(Box::new(|doc, path, _| {
                photocraft_io::export(doc, path, &Default::default()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
            })),
            write: Some(Box::new(|_, _| Ok(()))),
            automation_write: Some(Box::new(|_, _| Ok(()))),
            ..Default::default()
        },
    );
    app.run("file.new", json!({"width": 4, "height": 4, "name": "Untitled-1"})).unwrap();
    app.run("layer.new.layer", json!({})).unwrap();
    app
}

fn identity(app: &PhotocraftApp) -> (String, Option<String>, u64, u64) {
    let st = app.session.active().unwrap();
    (st.doc.name.clone(), st.path.clone(), st.revision, st.saved_revision)
}

#[test]
fn new_document_psd_and_subsequent_save_as_adopt_the_written_file_name() {
    for depth in [8, 16, 32] {
        let mut app = app();
        app.run("file.new", json!({"width": 4, "height": 4, "depth": depth, "name": "Untitled-1"})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        let bytes = Rc::new(RefCell::new(Vec::new()));
        let written = bytes.clone();
        app.services.write = Some(Box::new(move |_, data| {
            *written.borrow_mut() = data.to_vec();
            Ok(())
        }));
        let id = app.session.active().unwrap().doc.id;
        let revision = app.session.active().unwrap().revision;
        let ctx = egui::Context::default();
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": "作品/My image.PSD"})).unwrap();
        assert_eq!(identity(&app), ("My image.PSD".into(), Some("作品/My image.PSD".into()), revision, revision));
        assert_eq!(app.session.active().unwrap().doc.id, id);
        let reopened = photocraft_io::import("My image.PSD", &bytes.borrow()).unwrap();
        assert_eq!(reopened.document.size, app.session.active().unwrap().doc.size);
        assert_eq!(reopened.document.depth.bits(), depth);
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": "作品/次の画像.psd"})).unwrap();
        assert_eq!(app.session.active().unwrap().doc.name, "次の画像.psd");
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        assert_eq!(app.session.active().unwrap().path.as_deref(), Some("作品/次の画像.psd"));
        app.open_bytes("existing.psd", &bytes.borrow()).unwrap();
        let id = app.active_doc_id().unwrap();
        assert_eq!(app.session.active().unwrap().doc.name, "existing.psd");
        app.save_as(Some("renamed existing.psd".into())).unwrap();
        assert_eq!(app.active_doc_id().unwrap(), id);
        assert_eq!(app.session.active().unwrap().doc.name, "renamed existing.psd");
    }
}

#[test]
fn failed_encode_or_write_keeps_the_previous_name_path_and_dirty_state() {
    for saved_before in [false, true] {
        let mut app = app();
        if saved_before {
            app.save_as(Some("before.psd".into())).unwrap();
            app.run("layer.new.layer", json!({})).unwrap();
        }
        let before = identity(&app);
        app.services.write = Some(Box::new(|_, _| Err("disk full".into())));
        assert!(app.save_as(Some("after.psd".into())).is_err());
        assert_eq!(identity(&app), before);
        assert!(app.save_automation(Some("after.psd".into())).is_ok());
        app.run("layer.new.layer", json!({})).unwrap();
        let before = identity(&app);
        app.services.automation_write = Some(Box::new(|_, _| Err("denied".into())));
        assert!(app.save_automation(Some("another.psd".into())).is_err());
        assert_eq!(identity(&app), before);
        app.services.export = Some(Box::new(|_, _, _| Err("cannot encode".into())));
        assert!(app.save_as(Some("after.psd".into())).is_err());
        assert_eq!(identity(&app), before);
    }
}

#[test]
fn save_copy_and_export_keep_the_editable_document_identity() {
    let mut app = app();
    app.save_as(Some("original.psd".into())).unwrap();
    app.run("layer.new.layer", json!({})).unwrap();
    let before = identity(&app);
    app.write_document("copy.psd".into(), &ExportSettings::default(), true).unwrap();
    assert_eq!(identity(&app), before);
    let (dialog, _) = crate::file_dialog::fake(vec![Some(crate::file_dialog::FileDialogAnswer::SaveTo("export.png".into()))]);
    app.services.file_dialog = Some(dialog);
    let ctx = egui::Context::default();
    crate::export_dialog::confirm(&mut app, json!({"format": "png", "scale": 100}).as_object().unwrap()).unwrap();
    app.poll_file_dialog(&ctx, None);
    assert_eq!(identity(&app), before);
    assert!(app.ui.status.starts_with("Exported export.png"), "{}", app.ui.status);
}

#[test]
fn web_download_and_automation_saves_use_the_same_identity_completion() {
    let mut app = app();
    let (dialog, _) = crate::file_dialog::fake(vec![Some(crate::file_dialog::FileDialogAnswer::SaveTo("Untitled-1.psd".into()))]);
    app.services.file_dialog = Some(dialog);
    app.save_as(None).unwrap();
    app.poll_file_dialog(&egui::Context::default(), None);
    assert_eq!(app.session.active().unwrap().doc.name, "Untitled-1.psd");
    app.save_automation(Some("保存/agent save.psd".into())).unwrap();
    assert_eq!(app.session.active().unwrap().doc.name, "agent save.psd");
}

#[test]
fn automation_save_keeps_an_absolute_desktop_identity_and_scoped_write_requests() {
    use crate::AutomationPathKind;
    let mut app = app();
    // Background mode without an automation worker must use the same identity resolution.
    app.background_jobs = true;
    app.services.automation_path = Some(Box::new(|kind, path| match kind {
        AutomationPathKind::Read | AutomationPathKind::Write => Ok(format!("/workspace/{path}")),
        AutomationPathKind::WriteBack => path.strip_prefix("/workspace/").map(str::to_string).ok_or("outside write root".into()),
    }));
    let written = Rc::new(RefCell::new(Vec::new()));
    let capture = written.clone();
    app.services.automation_write = Some(Box::new(move |path, _| {
        capture.borrow_mut().push(path.to_string());
        Ok(())
    }));
    app.save_automation(Some("poster.pcraft".into())).unwrap();
    assert_eq!(app.session.active().unwrap().path.as_deref(), Some("/workspace/poster.pcraft"));
    app.save_automation(None).unwrap();
    assert_eq!(*written.borrow(), ["poster.pcraft", "poster.pcraft"]);
    let capture = written.clone();
    app.services.write = Some(Box::new(move |path, _| {
        capture.borrow_mut().push(path.to_string());
        Ok(())
    }));
    menus::invoke(&mut app, &egui::Context::default(), "file.save", json!({})).unwrap();
    assert_eq!(written.borrow().last().unwrap(), "/workspace/poster.pcraft");
    app.session.active_mut().unwrap().path = Some("/read-only/poster.pcraft".into());
    assert!(app.save_automation(None).is_err());
    assert_eq!(written.borrow().len(), 3);
}

#[test]
fn automation_flat_copy_preserves_the_canonical_layered_save_destination() {
    use crate::AutomationPathKind;
    let mut app = app();
    app.services.automation_path = Some(Box::new(|kind, path| match kind {
        AutomationPathKind::Read | AutomationPathKind::Write => Ok(format!("/workspace/{path}")),
        AutomationPathKind::WriteBack => path.strip_prefix("/workspace/").map(str::to_string).ok_or("outside write root".into()),
    }));
    let written = Rc::new(RefCell::new(Vec::new()));
    let capture = written.clone();
    app.services.automation_write = Some(Box::new(move |path, _| {
        capture.borrow_mut().push(path.to_string());
        Ok(())
    }));
    app.save_automation(Some("poster.pcraft".into())).unwrap();
    app.run("edit.fill", json!({"color": "#ff0000"})).unwrap();
    let before = identity(&app);
    app.save_automation(Some("copy.png".into())).unwrap();
    assert_eq!(identity(&app), before, "a flat copy must leave the layered document dirty");
    app.save_automation(None).unwrap();
    assert_eq!(*written.borrow(), ["poster.pcraft", "copy.png", "poster.pcraft"]);
    let state = app.session.active().unwrap();
    assert_eq!(state.path.as_deref(), Some("/workspace/poster.pcraft"));
    assert!(!state.is_dirty());
}

/// Save As to a flat format writes a copy when the flat file can't hold the document: its layers,
/// or the layered file it already lives in. The document keeps its file, so Save still writes the
/// layered original and the edits stay unsaved (#2550).
#[test]
fn save_as_flat_keeps_the_layered_original() {
    let ctx = egui::Context::default();
    // A layered document, and a one-layer one (the issue's case: the dialog offered a JPEG).
    for layered in [true, false] {
        let mut app = app();
        if !layered {
            app.run("file.new", json!({"width": 4, "height": 4, "name": "Untitled-2"})).unwrap();
        }
        let written = Rc::new(RefCell::new(Vec::new()));
        let w = written.clone();
        app.services.write = Some(Box::new(move |path, _| {
            w.borrow_mut().push(path.to_string());
            Ok(())
        }));
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": "art.psd"})).unwrap();
        app.run("edit.fill", json!({"color": "#000000"})).unwrap();
        let before = identity(&app);
        for flat in ["art.jpg", "art.png"] {
            menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": flat})).unwrap();
            assert_eq!(identity(&app), before, "{flat}, layered: {layered}");
        }
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        assert_eq!(*written.borrow(), ["art.psd", "art.jpg", "art.png", "art.psd"]);
        let st = app.session.active().unwrap();
        assert_eq!(st.saved_revision, st.revision);
    }
    // An untitled layered document saved as a JPEG is still untitled and unsaved.
    let mut app = app();
    let before = identity(&app);
    app.save_as(Some("flat.jpg".into())).unwrap();
    assert_eq!(identity(&app), before);
}

/// A flat document that isn't in a layered file becomes the flat file it is saved as, as before.
#[test]
fn save_as_flat_retargets_a_flat_document() {
    let mut app = app();
    app.run("file.new", json!({"width": 4, "height": 4, "name": "Untitled-2"})).unwrap();
    app.save_as(Some("shot.png".into())).unwrap();
    assert_eq!(app.session.active().unwrap().path.as_deref(), Some("shot.png"));
    app.save_as(Some("shot.jpg".into())).unwrap();
    let st = app.session.active().unwrap();
    assert_eq!((st.doc.name.as_str(), st.path.as_deref()), ("shot.jpg", Some("shot.jpg")));
    assert_eq!(st.saved_revision, st.revision);
}

/// `app.save` (control channel, MCP bridge) to a flat format is a copy, as in the headless server
/// (#1547); only a layered format becomes the document's file (#2579).
#[test]
fn automation_flat_save_is_a_copy() {
    let mut app = app();
    let before = identity(&app);
    for flat in ["flat.png", "flat.jpg", "flat.tif"] {
        app.save_automation(Some(flat.into())).unwrap();
        assert_eq!(identity(&app), before, "{flat}");
    }
    app.save_automation(Some("layers.psd".into())).unwrap();
    let st = app.session.active().unwrap();
    assert_eq!((st.doc.name.as_str(), st.path.as_deref()), ("layers.psd", Some("layers.psd")));
    assert_eq!(st.saved_revision, st.revision);
}

/// Preferences › File Handling › Image Previews reaches the exporter of Save As, Save and
/// automation saves: Always and Ask When Saving embed the PSD thumbnail, Never doesn't.
#[test]
fn image_previews_preference_reaches_the_exporter() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut app = app();
    let log = seen.clone();
    app.services.export = Some(Box::new(move |_, _, settings: &ExportSettings| {
        log.borrow_mut().push(settings.image_previews);
        Ok((b"out".to_vec(), Vec::new()))
    }));
    let ctx = egui::Context::default();
    for (value, expect) in [("always", true), ("never", false), ("ask", true)] {
        app.run("prefs.set", json!({"path": "fileHandling.imagePreviews", "value": value})).unwrap();
        seen.borrow_mut().clear();
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": format!("{value}.psd")})).unwrap();
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        app.save_automation(Some(format!("{value} agent.psd"))).unwrap();
        assert_eq!(*seen.borrow(), [expect; 3], "{value}");
    }
}

/// Preferences › File Handling › Maximize PSD and PSB File Compatibility reaches the exporter of
/// Save As, Save and automation saves: Always and Ask When Saving keep the PSD's real merged
/// image, Never doesn't.
#[test]
fn maximize_compatibility_preference_reaches_the_exporter() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut app = app();
    let log = seen.clone();
    app.services.export = Some(Box::new(move |_, _, settings: &ExportSettings| {
        log.borrow_mut().push(settings.psd_maximize_compatibility);
        Ok((b"out".to_vec(), Vec::new()))
    }));
    let ctx = egui::Context::default();
    for (value, expect) in [("always", true), ("never", false), ("ask", true)] {
        app.run("prefs.set", json!({"path": "fileHandling.maximizePsdCompatibility", "value": value})).unwrap();
        seen.borrow_mut().clear();
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": format!("{value}.psd")})).unwrap();
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        app.save_automation(Some(format!("{value} agent.psd"))).unwrap();
        assert_eq!(*seen.borrow(), [expect; 3], "{value}");
    }
}

/// #2399: Save and Save As start at the opened file's own encoding: a JPEG's estimated quality,
/// a WebP lossy (at its quality) or lossless; a new document keeps the codec defaults.
#[test]
fn save_settings_start_at_the_opened_files_encoding() {
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
    let px: Vec<u8> = (0..32u32 * 24).flat_map(|i| [(i * 3) as u8, (i / 32 * 9) as u8, (i % 97) as u8]).collect();
    let img = Image::from_raw(32, 24, ChannelLayout::Rgb, SampleType::U8, px).unwrap();
    let mut app = app();
    assert_eq!(app.save_settings().jpeg_quality, None);
    assert!(app.save_settings().webp_lossless);

    let jpeg = photocraft_codecs::encode(&img, Format::Jpeg, &EncodeOptions { jpeg_quality: 65, ..Default::default() }).unwrap();
    app.open_bytes("photo.jpg", &jpeg).unwrap();
    let q = app.save_settings().jpeg_quality.unwrap();
    assert!(q.abs_diff(65) <= 2, "estimated {q}");

    let lossy = photocraft_codecs::encode(&img, Format::WebP, &EncodeOptions { webp_lossless: false, webp_quality: 80, ..Default::default() }).unwrap();
    app.open_bytes("photo.webp", &lossy).unwrap();
    let s = app.save_settings();
    assert!(!s.webp_lossless);
    assert!(s.webp_quality.unwrap().abs_diff(80) <= 2, "{:?}", s.webp_quality);

    let lossless = photocraft_codecs::encode(&img, Format::WebP, &EncodeOptions::default()).unwrap();
    app.open_bytes("art.webp", &lossless).unwrap();
    assert!(app.save_settings().webp_lossless);
}

#[test]
fn background_automation_save_records_identity_and_keeps_requests_scoped() {
    use crate::AutomationPathKind;
    use std::sync::{Arc, Mutex};
    let mut app = app();
    app.background_jobs = true;
    app.services.automation_path = Some(Box::new(|kind, path| match kind {
        AutomationPathKind::Read | AutomationPathKind::Write => Ok(format!("/workspace/{path}")),
        AutomationPathKind::WriteBack => path.strip_prefix("/workspace/").map(str::to_string).ok_or("outside write root".into()),
    }));
    let written: Arc<Mutex<Vec<String>>> = Arc::default();
    let capture = Arc::clone(&written);
    app.services.automation_save = Some(Arc::new(move |_, path, _, ctl| {
        ctl.commit()?;
        capture.lock().unwrap().push(path.to_owned());
        Ok(vec![])
    }));
    // No synchronous exporter or ambient writer is available to hide a broken worker path.
    app.services.export = None;
    app.services.write = None;
    app.services.automation_write = None;
    let original = identity(&app);
    let revision = app.session.active().unwrap().revision;
    let doc = app.active_doc_id().unwrap();
    let started = app.save_automation(Some("poster.pcraft".into())).unwrap();
    assert_eq!(started["pending"], true);
    assert_eq!(started["path"], "poster.pcraft");
    assert_eq!(identity(&app), original, "completion alone adopts the saved identity");
    let finish = |app: &mut PhotocraftApp| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.saving() {
            assert!(std::time::Instant::now() < deadline, "background save timed out");
            crate::jobs_ui::tick(app, &egui::Context::default());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    // Completion must update the saved document even when another tab became active.
    app.run("file.new", json!({"width": 4, "height": 4, "name": "Other"})).unwrap();
    let other = app.active_doc_id().unwrap();
    finish(&mut app);
    assert_eq!(app.active_doc_id().unwrap(), other);
    let saved = app.session.documents().iter().find(|st| st.doc.id == doc).unwrap();
    assert_eq!(saved.path.as_deref(), Some("/workspace/poster.pcraft"));
    assert_eq!(saved.doc.name, "poster.pcraft");
    assert_eq!(saved.saved_revision, revision);
    assert!(!saved.is_dirty());
    assert!(app.ui.recent_files.is_empty(), "automation saves do not change the recent list");
    app.session.set_active(0);
    app.save_automation(None).unwrap();
    finish(&mut app);
    assert_eq!(*written.lock().unwrap(), ["poster.pcraft", "poster.pcraft"]);
    app.run("layer.new.layer", json!({})).unwrap();
    let before = identity(&app);
    app.save_automation(Some("copy.png".into())).unwrap();
    finish(&mut app);
    assert_eq!(identity(&app), before, "flat export preserves the layered destination and dirty revision");
    app.session.active_mut().unwrap().path = Some("/read-only/poster.pcraft".into());
    assert!(app.save_automation(None).is_err());
    assert_eq!(written.lock().unwrap().len(), 3, "read-only identities never reach the worker");
    assert!(!app.saving());
}

#[test]
fn failed_background_automation_save_preserves_the_previous_identity() {
    use std::sync::Arc;
    let mut app = app();
    app.services.automation_path = Some(Box::new(|_, path| Ok(format!("/workspace/{path}"))));
    app.background_jobs = true;
    app.services.automation_save = Some(Arc::new(|_, _, _, _| Err("disk full".into())));
    let before = identity(&app);
    app.save_automation(Some("failed.pcraft".into())).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.saving() {
        assert!(std::time::Instant::now() < deadline);
        crate::jobs_ui::tick(&mut app, &egui::Context::default());
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(identity(&app), before);
    assert!(app.ui.status.contains("disk full"), "{}", app.ui.status);
}
