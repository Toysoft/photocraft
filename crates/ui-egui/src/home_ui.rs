//! Home screen: recent files and virtual work folders. Folder membership is stored in
//! preferences; the original files are never moved or rewritten.

use std::collections::HashSet;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::{HashMap, VecDeque};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{self, Receiver, Sender};

use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use crate::PhotocraftApp;
use crate::theme::{self, Tokens};

const SIDE_W: f32 = 224.0;
const HEAD_H: f32 = 58.0;
const CARD_W: f32 = 174.0;
const CARD_H: f32 = 190.0;

/// File previews are decoded off the UI thread and kept only while this app is open.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub(crate) struct Thumbnails {
    requests: Option<Sender<String>>,
    results: Option<Receiver<(String, Option<egui::ColorImage>)>>,
    pending: HashSet<String>,
    ready: HashMap<String, Option<egui::TextureHandle>>,
    order: VecDeque<String>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Thumbnails {
    pub fn invalidate(&mut self, path: &str) {
        self.ready.remove(path);
        self.pending.remove(path);
        self.order.retain(|p| p != path);
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(results) = &self.results else { return };
        while let Ok((path, image)) = results.try_recv() {
            self.pending.remove(&path);
            let texture = image.map(|image| {
                ctx.load_texture(
                    format!("home-preview-{path}"),
                    image,
                    egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR },
                )
            });
            self.ready.insert(path.clone(), texture);
            self.order.push_back(path);
            while self.order.len() > 256 {
                if let Some(old) = self.order.pop_front() {
                    self.ready.remove(&old);
                }
            }
        }
    }

    fn get(&mut self, ctx: &egui::Context, path: &str, visible: bool) -> Option<&egui::TextureHandle> {
        if !self.ready.contains_key(path) && !self.pending.contains(path) && visible {
            if self.requests.is_none() {
                let (request_tx, request_rx) = mpsc::channel::<String>();
                let (result_tx, result_rx) = mpsc::channel();
                let repaint = ctx.clone();
                if std::thread::Builder::new()
                    .name("home-thumbnails".into())
                    .spawn(move || {
                        while let Ok(path) = request_rx.recv() {
                            let image = load_thumbnail(&path);
                            if result_tx.send((path, image)).is_err() {
                                break;
                            }
                            repaint.request_repaint();
                        }
                    })
                    .is_ok()
                {
                    self.requests = Some(request_tx);
                    self.results = Some(result_rx);
                }
            }
            if self.requests.as_ref().is_some_and(|tx| tx.send(path.to_owned()).is_ok()) {
                self.pending.insert(path.to_owned());
            }
        }
        self.ready.get(path).and_then(Option::as_ref)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_thumbnail(path: &str) -> Option<egui::ColorImage> {
    const MAX_FILE: u64 = 128 << 20;
    let source = std::path::Path::new(path);
    let len = source.metadata().ok()?.len();
    if len == 0 || len > MAX_FILE {
        return None;
    }
    let bytes = std::fs::read(source).ok()?;
    let ext = source.extension()?.to_str()?.to_ascii_lowercase();
    let embedded = match ext.as_str() {
        "pcraft" => photocraft_format::read_thumbnail(&bytes).ok()??,
        "psd" | "psb" => {
            let file = photocraft_psd::PsdFile::from_bytes(&bytes).ok()?;
            let resource = file.resource(photocraft_psd::resources::ids::THUMBNAIL)?;
            if resource.data.get(..4)? != 1_u32.to_be_bytes() {
                return None;
            }
            resource.data.get(28..)?.to_vec()
        }
        _ => bytes,
    };
    let options = photocraft_codecs::DecodeOptions {
        limits: photocraft_codecs::Limits { max_width: 20_000, max_height: 20_000, max_pixels: 24_000_000, max_alloc: 192 << 20 },
        ..Default::default()
    };
    let decoded = photocraft_codecs::decode_with(&embedded, &options).ok()?;
    let rgba = decoded.convert(photocraft_codecs::ChannelLayout::Rgba, photocraft_codecs::SampleType::U8);
    let image = image::RgbaImage::from_raw(rgba.width(), rgba.height(), rgba.into_data())?;
    let scale = (158.0 / image.width() as f32).min(135.0 / image.height() as f32).min(1.0);
    let width = ((image.width() as f32 * scale).round() as u32).max(1);
    let height = ((image.height() as f32 * scale).round() as u32).max(1);
    let small = image::imageops::resize(&image, width, height, image::imageops::FilterType::Triangle);
    Some(egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], small.as_raw()))
}

fn tr(source: &str) -> &str {
    crate::i18n::tr(crate::i18n::current(), source)
}

#[derive(Debug)]
enum FileAction {
    Open(String),
    RemoveRecent(String),
    ClearRecent,
    AddToFolder(String, String),
    RemoveFromFolder(String, String),
}

/// Preserve File › Open Recent as the source for the Home view. A folder keeps independent
/// references, so an older work remains in its group after it falls off the recent-file cap.
fn visible_files(app: &PhotocraftApp) -> Vec<String> {
    if cfg!(target_arch = "wasm32") {
        return Vec::new();
    }
    let groups = &app.session.prefs().file_handling.home_folders;
    let selected = app.ui.chrome.home_folder.as_deref().and_then(|name| groups.iter().find(|f| f.name == name));
    let query = app.ui.chrome.home_search.trim().to_lowercase();
    let mut paths = if query.is_empty() {
        selected.map_or_else(
            || if app.session.prefs().interface.show_home_recent_files { app.ui.recent_files.clone() } else { Vec::new() },
            |folder| folder.paths.clone(),
        )
    } else {
        let mut all = app.ui.recent_files.clone();
        all.extend(groups.iter().flat_map(|f| f.paths.iter().cloned()));
        let mut seen = HashSet::new();
        all.retain(|p| seen.insert(p.clone()) && p.to_lowercase().contains(&query));
        all
    };
    if app.ui.chrome.home_sort_by_name {
        paths.sort_by_key(|path| crate::file_open::display_name(path).to_lowercase());
    }
    paths
}

pub fn show(app: &mut PhotocraftApp, ui: &mut Ui) {
    #[cfg(not(target_arch = "wasm32"))]
    app.home_thumbnails.poll(ui.ctx());
    let t = Tokens::get(ui.ctx());
    let area = ui.available_rect_before_wrap();
    ui.painter().rect_filled(area, 0.0, t.canvas);
    let header = Rect::from_min_size(area.min, vec2(area.width(), HEAD_H));
    crate::canvas::open_on_double_click(app, ui, Rect::from_min_max(pos2(area.left(), header.bottom()), area.max));
    let sidebar = Rect::from_min_max(pos2(area.left(), header.bottom()), pos2((area.left() + SIDE_W).min(area.right()), area.bottom()));
    let main = Rect::from_min_max(pos2(sidebar.right(), header.bottom()), area.max);
    ui.painter().rect_filled(header, 0.0, t.chrome);
    ui.painter().rect_filled(sidebar, 0.0, t.canvas);
    ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, t.separator));
    ui.painter().line_segment([sidebar.right_top(), sidebar.right_bottom()], Stroke::new(1.0, t.separator));

    let mut header_ui = ui.new_child(egui::UiBuilder::new().id_salt("home-header").max_rect(header.shrink2(vec2(20.0, 9.0))));
    {
        let ui = &mut header_ui;
        ui.horizontal_centered(|ui| {
            let has_project = !app.session.documents().is_empty();
            let can_close = has_project || (app.ui.chrome.home.is_some() && !app.session.prefs().general.auto_show_home_screen);
            if has_project {
                let (back, response) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
                if response.hovered() {
                    ui.painter().rect_filled(back, t.radius_sm, t.hover);
                }
                let center = back.center();
                let stroke = Stroke::new(2.0, t.text);
                ui.painter().line_segment([center + vec2(-9.0, 0.0), center + vec2(8.0, 0.0)], stroke);
                ui.painter().line_segment([center + vec2(-9.0, 0.0), center + vec2(-2.0, -7.0)], stroke);
                ui.painter().line_segment([center + vec2(-9.0, 0.0), center + vec2(-2.0, 7.0)], stroke);
                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tr("Back to project")));
                if response.on_hover_text(tr("Back to project")).clicked() {
                    app.ui.chrome.home = None;
                }
            } else {
                let (logo, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                crate::brand::paint_mark(ui, logo);
            }
            ui.add_space(9.0);
            ui.label(RichText::new("PhotoCraft").font(theme::semibold(19.0)).color(t.text));
            if !has_project && can_close && ui.button("×").on_hover_text(tr("Close Home")).clicked() {
                app.ui.chrome.home = None;
            }
            let search_w = ui.available_width().clamp(0.0, 310.0);
            ui.add_space((ui.available_width() - search_w).max(0.0));
            let (search_rect, _) = ui.allocate_exact_size(vec2(search_w, 32.0), Sense::hover());
            ui.painter().rect_filled(search_rect, t.radius_sm, t.field);
            ui.painter().rect_stroke(search_rect, t.radius_sm, Stroke::new(1.0, t.card_border), egui::StrokeKind::Inside);
            let icon_rect = Rect::from_center_size(pos2(search_rect.left() + 17.0, search_rect.center().y), vec2(18.0, 18.0));
            crate::icons::paint(ui, icon_rect, "search", 17.0, t.text_dim);
            let edit_rect =
                Rect::from_min_max(pos2(search_rect.left() + 34.0, search_rect.top() + 2.0), pos2(search_rect.right() - 6.0, search_rect.bottom() - 2.0));
            ui.put(
                edit_rect,
                egui::TextEdit::singleline(&mut app.ui.chrome.home_search)
                    .frame(egui::Frame::NONE)
                    .vertical_align(egui::Align::Center)
                    .hint_text(tr("Search files…")),
            );
        });
    }

    let groups = app.session.prefs().file_handling.home_folders.clone();
    if app.ui.chrome.home_folder.as_ref().is_some_and(|name| !groups.iter().any(|f| &f.name == name)) {
        app.ui.chrome.home_folder = None;
    }
    let mut rename = None;
    let mut delete = None;
    let mut sidebar_ui = ui.new_child(egui::UiBuilder::new().id_salt("home-sidebar").max_rect(sidebar.shrink2(vec2(15.0, 18.0))));
    {
        let ui = &mut sidebar_ui;
        let (home_rect, home_response) = ui.allocate_exact_size(vec2(SIDE_W - 30.0, 34.0), Sense::click());
        ui.painter().rect_filled(
            home_rect,
            t.radius_sm,
            if app.ui.chrome.home_folder.is_none() {
                t.card
            } else if home_response.hovered() {
                t.hover
            } else {
                t.canvas
            },
        );
        let home_icon = Rect::from_center_size(pos2(home_rect.left() + 17.0, home_rect.center().y), vec2(17.0, 17.0));
        crate::icons::paint(ui, home_icon, "house", 16.0, t.text);
        ui.painter().text(pos2(home_rect.left() + 35.0, home_rect.center().y), Align2::LEFT_CENTER, tr("Home page"), FontId::proportional(14.0), t.text);
        if home_response.clicked() {
            app.ui.chrome.home_folder = None;
        }
        ui.add_space(20.0);
        if crate::widgets::secondary_button(ui, tr("Create"), 100.0).clicked() {
            let fields = app.new_document_fields();
            app.ui.open_dialog(crate::state::DialogKind::NewDocument, fields);
        }
        ui.add_space(8.0);
        if crate::widgets::secondary_button(ui, tr("Open"), 100.0).clicked() {
            let _ = app.open_dialog_file();
        }
        ui.add_space(26.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("Work folders")).font(theme::semibold(13.0)).color(t.text_dim));
            ui.add_space((ui.available_width() - 28.0).max(0.0));
            if ui.add_sized([24.0, 24.0], egui::Button::new("+")).on_hover_text(tr("New folder")).clicked() {
                app.ui.chrome.home_folder_draft.clear();
                app.ui.chrome.home_rename_folder = None;
                app.ui.chrome.home_folder_dialog = true;
            }
        });
        ui.add_space(6.0);
        let list_h = ui.available_height().max(40.0);
        egui::ScrollArea::vertical().id_salt("home-folders").max_height(list_h).show(ui, |ui| {
            for folder in &groups {
                let selected = app.ui.chrome.home_folder.as_deref() == Some(folder.name.as_str());
                let (row, response) = ui.allocate_exact_size(vec2(SIDE_W - 30.0, 31.0), Sense::click());
                ui.painter().rect_filled(
                    row,
                    t.radius_sm,
                    if selected {
                        t.card
                    } else if response.hovered() {
                        t.hover
                    } else {
                        t.canvas
                    },
                );
                let icon = Rect::from_center_size(pos2(row.left() + 16.0, row.center().y), vec2(17.0, 17.0));
                crate::icons::paint(ui, icon, "folder", 16.0, t.text_dim);
                let label = crate::tab_strip::elided(ui, &folder.name, FontId::proportional(13.0), t.text, row.width() - 65.0);
                ui.painter().galley(pos2(row.left() + 34.0, row.center().y - label.size().y / 2.0), label, t.text);
                ui.painter().text(
                    pos2(row.right() - 9.0, row.center().y),
                    Align2::RIGHT_CENTER,
                    folder.paths.len().to_string(),
                    FontId::proportional(11.0),
                    t.text_faint,
                );
                let response = response.on_hover_text(&folder.name);
                if response.clicked() {
                    app.ui.chrome.home_folder = Some(folder.name.clone());
                }
                response.context_menu(|ui| {
                    if ui.button(tr("Rename folder")).clicked() {
                        rename = Some(folder.name.clone());
                        ui.close();
                    }
                    if ui.button(tr("Delete folder")).on_hover_text(tr("Files stay on disk")).clicked() {
                        delete = Some(folder.name.clone());
                        ui.close();
                    }
                });
            }
        });
    }
    if let Some(name) = rename {
        app.ui.chrome.home_folder_draft = name.clone();
        app.ui.chrome.home_rename_folder = Some(name);
        app.ui.chrome.home_folder_dialog = true;
    }
    if let Some(name) = delete {
        app.session.prefs.edit(|p| {
            p.file_handling.delete_home_folder(&name);
        });
        if app.ui.chrome.home_folder.as_deref() == Some(&name) {
            app.ui.chrome.home_folder = None;
        }
    }

    let paths = visible_files(app);
    let mut add_files = false;
    let mut main_ui = ui.new_child(egui::UiBuilder::new().id_salt("home-main").max_rect(main.shrink2(vec2(28.0, 20.0))));
    {
        let ui = &mut main_ui;
        ui.horizontal(|ui| {
            let title =
                if app.ui.chrome.home_search.trim().is_empty() { app.ui.chrome.home_folder.as_deref().unwrap_or(tr("Recent")) } else { tr("Search results") };
            ui.label(RichText::new(title).font(theme::semibold(21.0)).color(t.text));
            ui.add_space(8.0);
            ui.label(RichText::new(paths.len().to_string()).color(t.text_faint));
            if app.ui.chrome.home_folder.is_some() && !cfg!(target_arch = "wasm32") {
                ui.add_space(16.0);
                if ui.button(tr("Add files…")).clicked() {
                    add_files = true;
                }
            }
            ui.add_space((ui.available_width() - 236.0).max(0.0));
            egui::ComboBox::from_id_salt("home-sort")
                .selected_text(if app.ui.chrome.home_sort_by_name { tr("Name") } else { tr("Recent") })
                .width(130.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut app.ui.chrome.home_sort_by_name, false, tr("Recent"));
                    ui.selectable_value(&mut app.ui.chrome.home_sort_by_name, true, tr("Name"));
                });
            if crate::icons::button(ui, "align-justify", 30.0, !app.ui.chrome.home_grid, tr("List view")).clicked() {
                app.ui.chrome.home_grid = false;
            }
            if crate::icons::button(ui, "grid-2x2", 30.0, app.ui.chrome.home_grid, tr("Grid view")).clicked() {
                app.ui.chrome.home_grid = true;
            }
        });
        ui.add_space(5.0);
        ui.separator();
        ui.add_space(18.0);
        if paths.is_empty() {
            ui.add_space(36.0);
            let empty = if app.ui.chrome.home_folder.is_some() { tr("This folder is empty. Use Add files above.") } else { tr("No recent files") };
            ui.label(RichText::new(empty).color(t.text_dim).size(14.0));
            if app.ui.chrome.home_folder.is_none() {
                ui.add_space(8.0);
                ui.label(RichText::new(crate::canvas::start_screen_drop_hint(app)).color(t.text_faint));
            }
        } else {
            let mut actions = Vec::new();
            egui::ScrollArea::vertical().id_salt("home-files").show(ui, |ui| {
                if app.ui.chrome.home_grid {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(14.0, 18.0);
                        for path in &paths {
                            file_card(ui, path, app, &groups, &t, &mut actions, true);
                        }
                    });
                } else {
                    for path in &paths {
                        file_card(ui, path, app, &groups, &t, &mut actions, false);
                    }
                }
            });
            for action in actions {
                match action {
                    FileAction::Open(path) => {
                        if let Err(e) = app.open_path(&path) {
                            app.open_failed(&crate::file_open::display_name(&path), &e);
                        }
                    }
                    FileAction::RemoveRecent(path) => app.remove_recent(&path),
                    FileAction::ClearRecent => app.clear_recent(),
                    FileAction::AddToFolder(folder, path) => {
                        app.session.prefs.edit(|p| {
                            p.file_handling.add_home_file(&folder, &path);
                        });
                    }
                    FileAction::RemoveFromFolder(folder, path) => {
                        app.session.prefs.edit(|p| {
                            p.file_handling.remove_home_file(&folder, &path);
                        });
                    }
                }
            }
        }
    }
    if add_files && let Some(folder) = app.ui.chrome.home_folder.clone() {
        let initial_dir = crate::file_dialog::last_used_dir(&app.ui.recent_files);
        let request = crate::FileDialogRequest::Open { multiple: true, initial_dir, extensions: None };
        if let Err(e) = app.ask_file(request, move |app, answer| match answer {
            crate::FileDialogAnswer::Paths(paths) => {
                app.session.prefs.edit(|p| {
                    for path in paths {
                        p.file_handling.add_home_file(&folder, &path);
                    }
                });
                Ok(serde_json::Value::Null)
            }
            _ => Err("Expected file paths".into()),
        }) {
            app.ui.status = e;
        }
    }
    folder_dialog(app, ui);
}

fn file_card(
    ui: &mut Ui,
    path: &str,
    app: &mut PhotocraftApp,
    groups: &[photocraft_engine::prefs::HomeFolder],
    t: &Tokens,
    actions: &mut Vec<FileAction>,
    grid: bool,
) {
    let name = crate::file_open::display_name(path);
    let size = if grid { vec2(CARD_W, CARD_H) } else { vec2(ui.available_width(), 70.0) };
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    // Keep the parent card active while the pointer is over its overlaid remove button.
    let hovered = ui.rect_contains_pointer(rect);
    ui.painter().rect_filled(rect, t.radius_sm, if hovered { t.hover } else { t.card });
    ui.painter().rect_stroke(rect, t.radius_sm, Stroke::new(1.0, t.card_border), egui::StrokeKind::Inside);
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_uppercase();
    if grid {
        let preview = Rect::from_min_max(rect.min + vec2(8.0, 8.0), pos2(rect.right() - 8.0, rect.bottom() - 43.0));
        paint_preview(ui, app, path, preview, &ext, t, true);
        let galley = crate::tab_strip::elided(ui, &name, FontId::proportional(12.5), t.text, CARD_W - 20.0);
        ui.painter().galley(pos2(rect.left() + 10.0, rect.bottom() - 32.0), galley, t.text);
    } else {
        let preview = Rect::from_min_size(rect.min + vec2(6.0, 6.0), vec2(58.0, 58.0));
        paint_preview(ui, app, path, preview, &ext, t, false);
        let name_g = crate::tab_strip::elided(ui, &name, FontId::proportional(13.0), t.text, (rect.width() * 0.45).max(50.0));
        ui.painter().galley(pos2(rect.left() + 76.0, rect.center().y - name_g.size().y / 2.0), name_g, t.text);
        let folder = std::path::Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let folder_g = crate::tab_strip::elided(ui, &folder, FontId::proportional(11.5), t.text_faint, (rect.width() * 0.37).max(0.0));
        ui.painter().galley(pos2(rect.right() - folder_g.size().x - 34.0, rect.center().y - folder_g.size().y / 2.0), folder_g, t.text_faint);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
    let recent = app.ui.recent_files.iter().any(|p| p == path);
    let mut removed = false;
    if hovered && recent {
        let close = Rect::from_center_size(pos2(rect.right() - 16.0, rect.top() + 16.0), vec2(23.0, 23.0));
        let x = ui.interact(close, resp.id.with("remove"), Sense::click());
        let label = crate::i18n::fmt(tl!("Remove {name} from Recent"), &[("name", &name)]);
        x.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
        let tint = crate::icons::button_chrome(ui, close, false, x.hovered());
        crate::icons::paint(ui, close, "x", 12.0, tint);
        if x.on_hover_text(tr("Remove from Recent")).clicked() {
            actions.push(FileAction::RemoveRecent(path.to_owned()));
            removed = true;
        }
    }
    if resp.clicked() && !removed {
        actions.push(FileAction::Open(path.to_owned()));
    }
    resp.on_hover_text(path).context_menu(|ui| {
        if !groups.is_empty() {
            ui.menu_button(tr("Add to folder"), |ui| {
                for folder in groups {
                    if ui.add_enabled(!folder.paths.iter().any(|p| p == path), egui::Button::new(&folder.name)).clicked() {
                        actions.push(FileAction::AddToFolder(folder.name.clone(), path.to_owned()));
                        ui.close();
                    }
                }
            });
        }
        if let Some(folder) = app.ui.chrome.home_folder.as_deref()
            && groups.iter().any(|f| f.name == folder && f.paths.iter().any(|p| p == path))
            && ui.button(tr("Remove from folder")).clicked()
        {
            actions.push(FileAction::RemoveFromFolder(folder.to_owned(), path.to_owned()));
            ui.close();
        }
        if recent && ui.button(tr("Remove from Recent")).clicked() {
            actions.push(FileAction::RemoveRecent(path.to_owned()));
            ui.close();
        }
        if recent && ui.button(tr("Clear Recent Files")).clicked() {
            actions.push(FileAction::ClearRecent);
            ui.close();
        }
    });
}

fn paint_preview(ui: &Ui, app: &mut PhotocraftApp, path: &str, rect: Rect, ext: &str, t: &Tokens, large: bool) {
    ui.painter().rect_filled(rect, t.radius_sm, t.field);
    #[cfg(not(target_arch = "wasm32"))]
    let thumb = app.home_thumbnails.get(ui.ctx(), path, rect.intersects(ui.clip_rect()));
    #[cfg(target_arch = "wasm32")]
    let thumb: Option<&egui::TextureHandle> = None;
    if let Some(thumb) = thumb {
        let size = thumb.size_vec2();
        let scale = (rect.width() / size.x).min(rect.height() / size.y);
        let image_rect = Rect::from_center_size(rect.center(), size * scale);
        ui.painter().image(thumb.id(), image_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    } else {
        let glyph = if matches!(ext, "PSD" | "PSB" | "PCRAFT" | "ORA") { "layers" } else { "image" };
        crate::icons::paint(ui, rect, glyph, if large { 45.0 } else { 24.0 }, t.text_dim);
        if large {
            ui.painter().text(
                rect.center() + vec2(0.0, 39.0),
                Align2::CENTER_CENTER,
                ext.chars().take(8).collect::<String>(),
                FontId::proportional(11.0),
                t.text_faint,
            );
        }
    }
}

fn folder_dialog(app: &mut PhotocraftApp, ui: &mut Ui) {
    if !app.ui.chrome.home_folder_dialog {
        return;
    }
    let editing = app.ui.chrome.home_rename_folder.clone();
    let mut submit = false;
    let mut cancel = false;
    egui::Window::new(if editing.is_some() { tr("Rename folder") } else { tr("New folder") })
        .id(egui::Id::new("home-folder-dialog"))
        .collapsible(false)
        .resizable(false)
        .show(ui.ctx(), |ui| {
            ui.label(tr("Folder name"));
            let edit = ui.add_sized([300.0, 28.0], egui::TextEdit::singleline(&mut app.ui.chrome.home_folder_draft).char_limit(64));
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(tr("Cancel")).clicked() {
                    cancel = true;
                }
                if ui.button(if editing.is_some() { tr("Rename") } else { tr("Create") }).clicked() {
                    submit = true;
                }
            });
        });
    if submit {
        let name = app.ui.chrome.home_folder_draft.trim().to_owned();
        let ok = app.session.prefs.edit(|p| match editing.as_deref() {
            Some(old) => p.file_handling.rename_home_folder(old, &name),
            None => p.file_handling.create_home_folder(&name),
        });
        if ok {
            app.ui.chrome.home_folder = Some(name);
            app.ui.chrome.home_folder_dialog = false;
            app.ui.chrome.home_rename_folder = None;
            app.ui.chrome.home_folder_draft.clear();
        } else {
            app.ui.status = tr("Choose a different folder name").to_owned();
        }
    }
    if cancel {
        app.ui.chrome.home_folder_dialog = false;
        app.ui.chrome.home_rename_folder = None;
        app.ui.chrome.home_folder_draft.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_recent_files_feed_home_without_migration() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        app.push_recent("/work/old.psd");
        app.push_recent("/work/new.png");
        assert_eq!(visible_files(&app), ["/work/new.png", "/work/old.psd"]);
        app.session.prefs.edit(|p| {
            assert!(p.file_handling.create_home_folder("Client"));
            assert!(p.file_handling.add_home_file("Client", "/work/old.psd"));
        });
        app.ui.chrome.home_folder = Some("Client".into());
        assert_eq!(visible_files(&app), ["/work/old.psd"]);
        app.remove_recent("/work/old.psd");
        assert_eq!(visible_files(&app), ["/work/old.psd"], "groups retain old files independently of Recent");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_and_psd_saves_have_readable_home_previews() {
        use photocraft_doc::{ColorMode, Document, SampleType, Size};

        let doc = Document::new("Preview", Size::new(24, 16), ColorMode::Rgb, SampleType::U8);
        let dir = std::env::temp_dir().join(format!("photocraft-home-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for extension in ["pcraft", "psd"] {
            let name = format!("preview.{extension}");
            let path = dir.join(&name);
            let saved = photocraft_io::export(&doc, &name, &Default::default()).unwrap();
            assert!(saved.warnings.is_empty(), "{extension}: {:?}", saved.warnings);
            std::fs::write(&path, saved.bytes).unwrap();
            let thumbnail = load_thumbnail(path.to_str().unwrap()).unwrap_or_else(|| panic!("no {extension} preview"));
            assert!(thumbnail.size[0] > 0 && thumbnail.size[1] > 0, "{extension} preview is empty");
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
    }
}
