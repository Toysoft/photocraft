//! Android NativeActivity host. SAF lives behind a deliberately small Java bridge because Android
//! grants document access through activities rather than file paths.

mod bridge;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use android_activity::AndroidApp;
use photocraft_doc::Document;
use photocraft_engine::Session;
use photocraft_format::RecoveryStore;
use photocraft_ui_egui::{ExportSettings, PhotocraftApp, Recoverable, Services};

use crate::tablet_gestures::TabletGestureRouter;

const ENCODE_SHARE: f32 = 0.85;

/// NativeActivity calls this on a dedicated thread after its Java host is ready.
#[unsafe(no_mangle)]
pub fn android_main(app: AndroidApp) {
    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("."));
    if let Err(error) = std::fs::create_dir_all(&data_dir) {
        eprintln!("PhotoCraft cannot create Android data directory: {error}");
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("PhotoCraft").with_app_id("ai.storyteller.photocraft"),
        persistence_path: None,
        android_app: Some(app),
        ..Default::default()
    };
    let start = eframe::run_native(
        "PhotoCraft",
        options,
        Box::new(move |cc| {
            let services = android_services(data_dir, cc.egui_ctx.clone());
            PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
            let mut editor = PhotocraftApp::new(Session::new(), services);
            // Decoding and saving a multilayer image must keep the tablet responsive.
            editor.background_jobs = true;
            if let Some(render_state) = cc.wgpu_render_state.clone() {
                editor.set_wgpu(render_state);
            }
            Ok(Box::new(editor))
        }),
    );
    if let Err(error) = start {
        eprintln!("PhotoCraft Android stopped: {error}");
    }
}

fn android_services(data_dir: PathBuf, ctx: egui::Context) -> Services {
    let prefs = data_dir.join("preferences.json");
    let prefs_for_load = prefs.clone();
    let prefs_for_save = prefs.clone();
    let save_bridge = bridge::shared();
    let gesture_bridge = save_bridge.clone();
    let clipboard_set_bridge = save_bridge.clone();
    let clipboard_get_bridge = save_bridge.clone();
    let recovery = recovery_services(data_dir.join("Recovery"));
    Services {
        import: Some(Box::new(|name, bytes, max_svg_group_depth| {
            photocraft_io::import_with_svg_group_depth(name, bytes, max_svg_group_depth)
                .map(|result| (result.document, result.warnings))
                .map_err(|error| error.to_string())
        })),
        export: Some(Box::new(|document, path, settings| {
            photocraft_io::export(document, path, &export_options(settings)).map(|result| (result.bytes, result.warnings)).map_err(|error| error.to_string())
        })),
        save_file: Some(Arc::new({
            let bridge = save_bridge.clone();
            move |document, path, settings, ctl| save_through(document, path, settings, ctl, |target, bytes| bridge.write(target, bytes))
        })),
        file_dialog: Some(Box::new(|request, _parent, reply| bridge::shared().request(request, reply))),
        write: Some(Box::new(move |target, bytes| save_bridge.write(target, bytes))),
        load_prefs: Some(Box::new(move || std::fs::read_to_string(&prefs_for_load).ok())),
        save_prefs: Some(Box::new(move |text| write_atomic(&prefs_for_save, text.as_bytes()))),
        clipboard_set_image: Some(Box::new(move |w, h, rgba| clipboard_set_bridge.set_clipboard_image(w, h, rgba))),
        clipboard_get_image: Some(Box::new(move || clipboard_get_bridge.get_clipboard_image())),
        raw_input: Some(Box::new({
            let mut router = TabletGestureRouter::default();
            move |raw, mode| {
                router.apply(raw, gesture_bridge.take_tablet_actions(), mode);
                if router.waiting_for_chord() {
                    ctx.request_repaint_after(std::time::Duration::from_millis(80));
                }
            }
        })),
        stylus_barrel_held: Some(Box::new({
            let bridge = bridge::shared();
            move || bridge.barrel_held()
        })),
        touch_ui: true,
        ..recovery
    }
}

fn export_options(settings: &ExportSettings) -> photocraft_io::ExportOptions {
    let mut options = photocraft_io::ExportOptions::default();
    if let Some(quality) = settings.jpeg_quality {
        options.encode.jpeg_quality = quality;
    }
    options.encode.webp_lossless = settings.webp_lossless;
    if let Some(quality) = settings.webp_quality {
        options.encode.webp_quality = quality;
    }
    options.tiff_layers = settings.tiff_layers;
    options.xmp = if settings.xmp_all { photocraft_io::XmpEmbed::All } else { photocraft_io::XmpEmbed::None };
    options.tga_bits = settings.tga_bits;
    options.image_previews = settings.image_previews;
    options.psd_maximize_compatibility = settings.psd_maximize_compatibility;
    options
}

fn save_through(
    document: &Document,
    path: &str,
    settings: &ExportSettings,
    ctl: &photocraft_ui_egui::jobs_ui::SaveCtl,
    write: impl Fn(&str, &[u8]) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    ctl.progress(0.0, "Encoding");
    let cancelled = || ctl.check().is_err();
    let progress = |fraction: f32| ctl.progress(fraction * ENCODE_SHARE, "");
    let interrupt = photocraft_raster::Interrupt::new(&cancelled, &progress);
    let result = photocraft_io::export_with(document, path, &export_options(settings), &interrupt).map_err(|error| error.to_string())?;
    ctl.commit()?;
    ctl.progress(ENCODE_SHARE, "Writing");
    write(path, &result.bytes)?;
    ctl.progress(1.0, "");
    Ok(result.warnings)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    photocraft_format::atomic_write(path, bytes).map_err(|error| error.to_string())
}

fn recovery_services(dir: PathBuf) -> Services {
    use std::cell::RefCell;
    use std::rc::Rc;

    type Store = Rc<RefCell<Option<RecoveryStore>>>;
    fn with_store<T>(store: &Store, action: impl FnOnce(&mut RecoveryStore) -> T) -> Result<T, String> {
        let mut slot = store.try_borrow_mut().map_err(|_| "crash recovery is busy".to_string())?;
        Ok(action(slot.as_mut().ok_or_else(|| "no Android data directory".to_string())?))
    }

    let store: Store = Rc::new(RefCell::new(Some(RecoveryStore::new(dir.clone()))));
    let autosave = store.clone();
    let results = store.clone();
    let discard = store.clone();
    let adopt = store.clone();
    Services {
        autosave: Some(Box::new(move |document, revision, path| {
            with_store(&autosave, |recovery| recovery.autosave_checked(document, revision, path.map(str::to_string)))?
        })),
        autosave_results: Some(Box::new(move || with_store(&results, |recovery| recovery.take_completed()).unwrap_or_default())),
        discard_autosave: Some(Box::new(move |id| {
            let _ = with_store(&discard, |recovery| recovery.discard(id));
        })),
        recover: Some(Box::new(move || {
            photocraft_format::list_recovery(&dir)
                .into_iter()
                .map(|entry| Recoverable {
                    key: entry.info.key.clone(),
                    name: entry.info.document_name.clone(),
                    path: entry.info.original_path.clone(),
                    load: Box::new(move || photocraft_format::recover(&entry).map_err(|error| error.to_string())),
                })
                .collect()
        })),
        adopt_autosave: Some(Box::new(move |id, key| {
            let _ = with_store(&adopt, |recovery| recovery.adopt(id, key));
        })),
        ..Default::default()
    }
}
