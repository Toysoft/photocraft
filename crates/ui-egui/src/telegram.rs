//! File › Send to Telegram: post the saved or exported file to a chat the user configured.
//!
//! The bot token never goes through preferences or the control channel. It lives in a separate
//! settings file the desktop app reads and writes (`Services::load_telegram` /
//! `save_telegram`). Copy and `prefs.get` therefore cannot see it. Sending is a shell command:
//! the engine has no network, and plug-ins cannot open one.

use serde_json::{Value, json};

use crate::PhotocraftApp;

/// Telegram Bot API `sendDocument` limit for a bot.
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// Which write just finished, so each automatic send can be switched on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    Save,
    SaveAs,
    /// File › Save a Copy. The document's own path stays unchanged.
    SaveCopy,
    Export,
}

/// What the user stored for sending. Empty token or chat means "not set up yet".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub bot_token: String,
    pub chat_id: String,
    /// After File › Save. Off until the user opts in.
    pub send_on_save: bool,
    /// After File › Save As. Off until the user opts in.
    pub send_on_save_as: bool,
    /// After an export. Off until the user opts in.
    pub send_on_export: bool,
    /// After File › Save a Copy. Off until the user opts in.
    pub send_on_save_copy: bool,
    /// When the file sent is a PSD, also send a PNG of the same document.
    pub also_png: bool,
    /// Pin the sent message. Off until the user opts in. Needs the bot to be an admin.
    pub pin_message: bool,
    /// Files this app sent, newest first, so they can be opened again. The bot token is not here.
    pub sent_files: Vec<Value>,
}

impl Settings {
    fn wants(&self, kind: WriteKind) -> bool {
        match kind {
            WriteKind::Save => self.send_on_save,
            WriteKind::SaveAs => self.send_on_save_as,
            WriteKind::SaveCopy => self.send_on_save_copy,
            WriteKind::Export => self.send_on_export,
        }
    }
}

impl Settings {
    pub fn ready(&self) -> bool {
        validate_token(&self.bot_token).is_ok() && parse_chats(&self.chat_id).is_ok()
    }
}

/// Parse the settings file. A missing or broken file is "not set up", never an error.
pub fn settings_from_json(text: &str) -> Settings {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Settings::default() };
    Settings {
        bot_token: v.get("botToken").and_then(Value::as_str).unwrap_or("").trim().to_string(),
        chat_id: v.get("chatId").and_then(Value::as_str).unwrap_or("").trim().to_string(),
        send_on_save: v.get("sendOnSave").and_then(Value::as_bool).unwrap_or(false),
        // Older files had one switch for Save As and Export together.
        send_on_save_as: v.get("sendOnSaveAs").and_then(Value::as_bool).unwrap_or_else(|| v.get("sendAfterExport").and_then(Value::as_bool).unwrap_or(false)),
        send_on_export: v.get("sendOnExport").and_then(Value::as_bool).unwrap_or_else(|| v.get("sendAfterExport").and_then(Value::as_bool).unwrap_or(false)),
        send_on_save_copy: v.get("sendOnSaveCopy").and_then(Value::as_bool).unwrap_or(false),
        also_png: v.get("alsoPng").and_then(Value::as_bool).unwrap_or(false),
        pin_message: v.get("pinMessage").and_then(Value::as_bool).unwrap_or(false),
        sent_files: v.get("sentFiles").and_then(Value::as_array).cloned().unwrap_or_default(),
    }
}

fn settings_json(s: &Settings) -> String {
    serde_json::to_string(&json!({
        "botToken": s.bot_token,
        "chatId": s.chat_id,
        "sendOnSave": s.send_on_save,
        "sendOnSaveAs": s.send_on_save_as,
        "sendOnExport": s.send_on_export,
        "sendOnSaveCopy": s.send_on_save_copy,
        "alsoPng": s.also_png,
        "pinMessage": s.pin_message,
        "sentFiles": s.sent_files,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

/// Official BotFather walkthrough. Chat ids come from `getUpdates` (the Find chats button).
pub const BOT_TUTORIAL: &str = "https://core.telegram.org/bots/tutorial";

/// File › Send to Telegram…. Opens the setup dialog when the token or chat is missing.
/// A document that has not been saved is flattened to a temporary PNG and sent from a
/// background thread, so the window stays usable while Telegram answers.
pub fn command(app: &mut PhotocraftApp) -> Result<Value, String> {
    let pending = app.session.active().and_then(|d| d.path.clone());
    Ok(json!({"dialog": open_dialog(app, pending)}))
}

/// Save, Save As and Export call this after the file is on disk. Each kind sends only when
/// its own switch is on. A missing setup opens the dialog and leaves the saved file alone.
pub fn after_written(app: &mut PhotocraftApp, path: &str, kind: WriteKind) {
    if !app.telegram.wants(kind) {
        return;
    }
    if !app.telegram.ready() {
        if !dialog_open(app) {
            open_dialog(app, Some(path.to_string()));
        }
        return;
    }
    let chats = match parse_chats(&app.telegram.chat_id) {
        Ok(chats) => chats,
        Err(error) => {
            app.ui.status = error;
            app.ui.status_error = true;
            return;
        }
    };
    let preview = match png_preview(app, path) {
        Ok(preview) => preview,
        Err(error) => {
            app.ui.status = error;
            app.ui.status_error = true;
            return;
        }
    };
    if let Err(error) = start_send(app, path.to_string(), false, chats, preview) {
        app.ui.status = error;
        app.ui.status_error = true;
    }
}

/// Apply a finished background send. Keeps requesting frames until Telegram answers.
pub fn poll(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some(job) = app.telegram_job.as_ref() else { return };
    let finished = match job.rx.try_recv() {
        Ok(result) => Some(result),
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("The Telegram send stopped before it finished.".to_string())),
    };
    let Some(job) = app.telegram_job.take() else { return };
    if job.temporary {
        let _ = std::fs::remove_file(&job.path);
    }
    if let Some(preview) = &job.preview {
        let _ = std::fs::remove_file(preview);
    }
    match finished {
        Some(Ok(body)) => {
            if let Some(path) = body.strip_prefix("OPEN\t") {
                let opened = std::fs::read(path).ok();
                let name = file_name(path);
                let _ = std::fs::remove_file(path);
                if let Some(bytes) = opened {
                    if let Err(error) = app.open_bytes(&name, &bytes) {
                        app.ui.status = error;
                        app.ui.status_error = true;
                    } else {
                        app.ui.status = format!("Opened {name}.");
                        app.ui.status_error = false;
                    }
                } else {
                    app.ui.status = format!("Could not read {name}.");
                    app.ui.status_error = true;
                }
                return;
            }
            let report = send_report(&body);
            let url = report.urls.first().cloned();
            let links = if report.urls.is_empty() {
                tl!("Telegram did not return a link for this chat.").to_string()
            } else {
                report.urls.join("\n")
            };
            let mut status = crate::i18n::fmt(tl!("Sent {name}. {url}"), &[("name", &job.name), ("url", &links)]);
            if report.pin_failed {
                let note = tl!("Could not pin the message. The file was still sent.").to_string();
                status = format!("{status}\n{note}");
            }
            remember_sent_files(app, &report.files);
            app.ui.status = status.clone();
            app.ui.status_error = false;
            show_result(app, status, url);
        }
        Some(Err(error)) => {
            let message = redact(&error, &job.token);
            app.ui.status = message.clone();
            app.ui.status_error = true;
            show_result(app, message, None);
        }
        None => {}
    }
}

pub fn owns(fields: &serde_json::Map<String, Value>) -> bool {
    fields.get("__telegram").and_then(Value::as_bool) == Some(true)
}

pub fn result_owns(fields: &serde_json::Map<String, Value>) -> bool {
    fields.get("__telegram_result").and_then(Value::as_bool) == Some(true)
}

pub fn result_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &serde_json::Map<String, Value>) {
    ui.set_min_width(420.0);
    if let Some(message) = f.get("message").and_then(Value::as_str) {
        ui.label(message);
        ui.add_space(8.0);
    }
    let Some(url) = f.get("url").and_then(Value::as_str) else { return };
    ui.hyperlink(url);
    ui.add_space(4.0);
    if crate::widgets::secondary_button(ui, tl!("Open the Telegram message"), 220.0).clicked() {
        crate::links::open(app, ui.ctx(), url);
    }
}

fn show_result(app: &mut PhotocraftApp, message: String, url: Option<String>) {
    let mut fields = serde_json::Map::new();
    fields.insert("__telegram_result".into(), json!(true));
    fields.insert("__label".into(), json!("Telegram"));
    fields.insert("message".into(), json!(message));
    if let Some(url) = url {
        fields.insert("url".into(), json!(url));
    }
    app.ui.open_dialog(crate::state::DialogKind::Command, fields);
}

fn dialog_open(app: &PhotocraftApp) -> bool {
    app.ui.dialogs.iter().any(|d| owns(&d.fields))
}

fn open_dialog(app: &mut PhotocraftApp, pending: Option<String>) -> u64 {
    let mut fields = serde_json::Map::new();
    fields.insert("__telegram".into(), json!(true));
    fields.insert("__label".into(), json!("Send to Telegram"));
    fields.insert("token".into(), json!(app.telegram.bot_token));
    fields.insert("chat".into(), json!(app.telegram.chat_id));
    fields.insert("onSave".into(), json!(app.telegram.send_on_save));
    fields.insert("onSaveAs".into(), json!(app.telegram.send_on_save_as));
    fields.insert("onExport".into(), json!(app.telegram.send_on_export));
    fields.insert("onSaveCopy".into(), json!(app.telegram.send_on_save_copy));
    fields.insert("alsoPng".into(), json!(app.telegram.also_png));
    fields.insert("pin".into(), json!(app.telegram.pin_message));
    let saved = pending.as_ref().is_some_and(|path| std::fs::metadata(path).is_ok_and(|meta| meta.is_file()))
        || app.session.active().and_then(|d| d.path.as_deref()).is_some_and(|path| std::fs::metadata(path).is_ok_and(|meta| meta.is_file()));
    fields.insert("format".into(), json!(if saved { "file" } else { "psd" }));
    if let Some(path) = pending {
        fields.insert("pending".into(), json!(path));
    }
    app.ui.open_dialog(crate::state::DialogKind::Command, fields)
}

pub fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut serde_json::Map<String, Value>) {
    let t = crate::theme::Tokens::get(ui.ctx());
    ui.set_min_width(440.0);
    ui.label(egui::RichText::new(tl!("Sending needs an internet connection.")).color(t.text_dim));
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(tl!(
            "Create a bot with BotFather, then send that bot any message. Use Find chats to fill the id, or paste a number or @channelname."
        ))
        .color(t.text_dim),
    );
    ui.add_space(4.0);
    if crate::widgets::secondary_button(ui, tl!("Open the official bot guide"), 220.0).clicked() {
        crate::links::open(app, ui.ctx(), BOT_TUTORIAL);
    }
    ui.add_space(8.0);
    ui.label(tl!("Bot token"));
    let mut token = f.get("token").and_then(Value::as_str).unwrap_or("").to_string();
    if ui.add(egui::TextEdit::singleline(&mut token).password(true).desired_width(ui.available_width())).changed() {
        f.insert("token".into(), json!(token.clone()));
    }
    ui.label(egui::RichText::new(tl!("The bot token is kept in the system keychain, not in PhotoCraft's settings file.")).color(t.text_dim));
    ui.add_space(4.0);
    if crate::widgets::secondary_button(ui, tl!("Find chats for this token"), 220.0).clicked() {
        match fetch_chats(app, &token) {
            Ok(chats) => {
                f.insert("found".into(), json!(chats));
                f.remove("findError");
            }
            Err(error) => {
                f.insert("findError".into(), json!(error));
            }
        }
    }
    if let Some(error) = f.get("findError").and_then(Value::as_str) {
        ui.label(egui::RichText::new(error).color(t.warning));
    }
    if let Some(chats) = f.get("found").and_then(Value::as_array).cloned() {
        if chats.is_empty() {
            ui.label(tl!("No chats yet. Send your bot a message, then try again."));
        }
        for chat in chats {
            let id = chat.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let label = chat.get("label").and_then(Value::as_str).unwrap_or(&id).to_string();
            if id.is_empty() {
                continue;
            }
            if ui.selectable_label(chat_selected(f.get("chat").and_then(Value::as_str).unwrap_or(""), &id), label).clicked() {
                f.insert("chat".into(), json!(append_chat(f.get("chat").and_then(Value::as_str).unwrap_or(""), &id)));
                ui.ctx().copy_text(id);
                app.ui.status = "Copied the chat id.".to_string();
                app.ui.status_error = false;
            }
        }
    }
    ui.add_space(4.0);
    ui.label(egui::RichText::new(tl!("Files sent from PhotoCraft, and the ten newest files sent to this bot. Click one to open it in PhotoCraft.")).color(t.text_dim));
    ui.add_space(4.0);
    if crate::widgets::secondary_button(ui, tl!("Get files"), 160.0).clicked() {
        match fetch_inbox(app, &token) {
            Ok(files) => {
                f.insert("inbox".into(), json!(files));
                f.remove("findError");
            }
            Err(error) => {
                f.insert("findError".into(), json!(error));
            }
        }
    }
    if let Some(files) = f.get("inbox").and_then(Value::as_array).cloned() {
        if files.is_empty() {
            ui.label(tl!("No files yet. Send one from here, or send the bot a file, then try again."));
        }
        for file in files {
            let id = file.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let name = file.get("name").and_then(Value::as_str).unwrap_or("file").to_string();
            let label = file.get("label").and_then(Value::as_str).unwrap_or(&name).to_string();
            if id.is_empty() {
                continue;
            }
            if ui.selectable_label(false, label).clicked()
                && let Err(error) = start_download(app, &token, &id, &name)
            {
                app.ui.status = error;
                app.ui.status_error = true;
            }
        }
    }
    ui.add_space(8.0);
    ui.label(tl!("Chats or channels. Several are allowed, separated by commas."));
    let mut chat = f.get("chat").and_then(Value::as_str).unwrap_or("").to_string();
    if ui.add(egui::TextEdit::singleline(&mut chat).desired_width(ui.available_width())).changed() {
        f.insert("chat".into(), json!(chat));
    }
    ui.add_space(8.0);
    ui.label(tl!("Send as"));
    let format = f.get("format").and_then(Value::as_str).unwrap_or("psd").to_string();
    ui.horizontal(|ui| {
        for (id, label) in [("file", tl!("Saved file")), ("psd", "PSD"), ("png", "PNG")] {
            if ui.selectable_label(format == id, label).clicked() {
                f.insert("format".into(), json!(id));
            }
        }
    });
    ui.label(
        egui::RichText::new(tl!("Saved file sends the copy on disk. PSD and PNG are a new copy of the open document, including changes you have not saved."))
            .color(t.text_dim),
    );
    let mut also_png = f.get("alsoPng").and_then(Value::as_bool).unwrap_or(false);
    if crate::widgets::checkbox(ui, &mut also_png, tl!("Also send a PNG preview with a PSD")).changed() {
        f.insert("alsoPng".into(), json!(also_png));
    }
    let mut pin = f.get("pin").and_then(Value::as_bool).unwrap_or(false);
    if crate::widgets::checkbox(ui, &mut pin, tl!("Pin the message")).changed() {
        f.insert("pin".into(), json!(pin));
    }
    ui.label(egui::RichText::new(tl!("Pinning needs the bot to be an admin. If it is not, the file is still sent.")).color(t.text_dim));
    ui.add_space(8.0);
    crate::widgets::section_label(ui, "Send automatically");
    ui.add_space(4.0);
    for (key, label) in [
        ("onSave", tl!("Also send when I Save")),
        ("onSaveAs", tl!("Also send when I Save As")),
        ("onSaveCopy", tl!("Also send when I Save a Copy")),
        ("onExport", tl!("Also send when I Export")),
    ] {
        let mut on = f.get(key).and_then(Value::as_bool).unwrap_or(false);
        if crate::widgets::checkbox(ui, &mut on, label).changed() {
            f.insert(key.into(), json!(on));
        }
    }
}

pub fn confirm(app: &mut PhotocraftApp, f: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let token = f.get("token").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let chat = f.get("chat").and_then(Value::as_str).unwrap_or("").trim().to_string();
    validate_token(&token)?;
    let chats = parse_chats(&chat)?;
    let format = f.get("format").and_then(Value::as_str).unwrap_or("psd").to_string();
    let next = Settings {
        bot_token: token,
        chat_id: chat,
        send_on_save: f.get("onSave").and_then(Value::as_bool).unwrap_or(false),
        send_on_save_as: f.get("onSaveAs").and_then(Value::as_bool).unwrap_or(false),
        send_on_export: f.get("onExport").and_then(Value::as_bool).unwrap_or(false),
        send_on_save_copy: f.get("onSaveCopy").and_then(Value::as_bool).unwrap_or(false),
        also_png: f.get("alsoPng").and_then(Value::as_bool).unwrap_or(false),
        pin_message: f.get("pin").and_then(Value::as_bool).unwrap_or(false),
        sent_files: app.telegram.sent_files.clone(),
    };
    let text = settings_json(&next);
    let pending = f.get("pending").and_then(Value::as_str).map(str::to_string);
    let Some(save) = app.services.save_telegram.as_mut() else {
        return Err("Telegram sending needs the desktop app.".to_string());
    };
    save(&text)?;
    app.telegram = next;
    if app.session.active().is_none() && pending.is_none() {
        app.ui.status = "Telegram is set up. Save or export a file, then send it.".to_string();
        app.ui.status_error = false;
        return Ok(Value::Null);
    }
    let (path, temporary) = resolve_payload(app, &format, pending)?;
    let preview = png_preview(app, &path)?;
    let name = file_name(&path);
    start_send(app, path, temporary, chats, preview)?;
    Ok(json!({"sending": true, "name": name}))
}

/// A send whose request is still on the network. Not UI state: it holds the token only so an
/// error can be redacted.
pub struct Job {
    rx: std::sync::mpsc::Receiver<Result<String, String>>,
    path: String,
    temporary: bool,
    /// A PNG preview written only for this send. Always removed when the send ends.
    preview: Option<String>,
    name: String,
    token: String,
}

/// Arm Telegram for a save that may still be writing in the background. A finished write sends
/// now; a pending one sends when the worker reports the file is on disk.
pub fn arm_after_save(app: &mut PhotocraftApp, path: &str, kind: WriteKind, result: &Value) {
    if result.get("pending").and_then(Value::as_bool) == Some(true) {
        if let Some(save) = app.jobs.saves.iter_mut().rev().find(|save| save.path == path && !save.ended) {
            save.notify_telegram = Some(kind);
        }
        return;
    }
    after_written(app, path, kind);
}

fn forget_unsent(temporary: bool, path: &str, preview: &Option<String>) {
    if temporary {
        let _ = std::fs::remove_file(path);
    }
    if let Some(preview) = preview {
        let _ = std::fs::remove_file(preview);
    }
}

fn start_send(app: &mut PhotocraftApp, path: String, temporary: bool, chats: Vec<String>, preview: Option<String>) -> Result<(), String> {
    if app.telegram_job.is_some() {
        if temporary {
            let _ = std::fs::remove_file(&path);
        }
        if let Some(preview) = &preview {
            let _ = std::fs::remove_file(preview);
        }
        return Err("A Telegram send is already running.".to_string());
    }
    let Some(send) = app.services.telegram_send.clone() else {
        forget_unsent(temporary, &path, &preview);
        return Err("Telegram sending needs the desktop app.".to_string());
    };
    let (name, bytes) = match read_file(&path) {
        Ok(read) => read,
        Err(error) => {
            forget_unsent(temporary, &path, &preview);
            return Err(error);
        }
    };
    let preview_file = match preview.as_deref() {
        Some(preview_path) => {
            let (preview_name, preview_bytes) = match read_file(preview_path) {
                Ok(read) => read,
                Err(error) => {
                    forget_unsent(temporary, &path, &preview);
                    return Err(error);
                }
            };
            let preview_caption = caption(preview_path, &preview_name, preview_bytes.len() as u64, file_secs(preview_path));
            Some((preview_name, preview_bytes, preview_caption))
        }
        None => None,
    };
    let caption = caption(&path, &name, bytes.len() as u64, file_secs(&path));
    let token = app.telegram.bot_token.clone();
    let pin = app.telegram.pin_message;
    let (tx, rx) = std::sync::mpsc::channel();
    let report_token = token.clone();
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (send, bytes, caption, chats, report_token, tx, preview_file, pin);
        forget_unsent(temporary, &path, &preview);
        return Err("Telegram sending needs the desktop app.".to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::Builder::new()
        .name("pc-telegram".into())
        .spawn({
            let name = name.clone();
            move || {
                let result = send_to_chats(&send, &report_token, &chats, &caption, &name, &bytes, preview_file.as_ref(), pin);
                let _ = tx.send(result);
            }
        })
        .map_err(|error| {
            forget_unsent(temporary, &path, &preview);
            error.to_string()
        })?;
    app.ui.status = format!("Sending {name} to Telegram…");
    app.ui.status_error = false;
    app.telegram_job = Some(Job { rx, path, temporary, preview, name, token });
    Ok(())
}

fn resolve_payload(app: &mut PhotocraftApp, format: &str, pending: Option<String>) -> Result<(String, bool), String> {
    match format {
        "png" => export_temp(app, "png").map(|path| (path, true)),
        "psd" => export_temp(app, "psd").map(|path| (path, true)),
        _ => saved_file(app, pending).ok_or_else(|| "There is no saved file to send. Choose PSD or PNG.".to_string()),
    }
}

fn saved_file(app: &PhotocraftApp, pending: Option<String>) -> Option<(String, bool)> {
    let path = pending
        .filter(|path| std::fs::metadata(path).is_ok_and(|meta| meta.is_file()))
        .or_else(|| app.session.active().and_then(|doc| doc.path.clone()).filter(|path| std::fs::metadata(path).is_ok_and(|meta| meta.is_file())))?;
    Some((path, false))
}

fn export_temp(app: &mut PhotocraftApp, extension: &str) -> Result<String, String> {
    let doc = app.session.active().ok_or_else(|| "no document".to_string())?.doc.clone();
    let export = app.services.export.as_ref().ok_or_else(|| "Telegram sending needs the desktop app.".to_string())?;
    let stem = file_name(&doc.name);
    let path = std::env::temp_dir().join(format!("photocraft-telegram-{}-{stem}.{extension}", std::process::id()));
    let path_str = path.display().to_string();
    let settings = crate::ExportSettings { xmp_all: false, tiff_layers: false, ..crate::ExportSettings::default() };
    let (bytes, _) =
        export(&doc, &path_str, &settings).map_err(|error| format!("Could not prepare a file to send: {error}"))?;
    if bytes.len() as u64 > MAX_BYTES {
        let _ = std::fs::remove_file(&path);
        return Err("That file is larger than Telegram accepts (50 MB).".to_string());
    }
    std::fs::write(&path, &bytes).map_err(|error| read_error(&path_str, &error.to_string()))?;
    Ok(path_str)
}

fn fetch_inbox(app: &PhotocraftApp, token: &str) -> Result<Vec<Value>, String> {
    validate_token(token)?;
    let Some(get) = app.services.telegram_get.as_ref() else {
        return Err("Telegram sending needs the desktop app.".to_string());
    };
    let url = format!("https://api.telegram.org/bot{token}/getUpdates");
    let body = get(&url).map_err(|error| redact(&error, token))?;
    let mut rows = app.telegram.sent_files.clone();
    for row in files_from_updates(&body) {
        let id = row.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || rows.iter().any(|have| have.get("id").and_then(Value::as_str) == Some(id)) {
            continue;
        }
        rows.push(row);
        if rows.len() == 10 {
            break;
        }
    }
    Ok(rows)
}

fn start_download(app: &mut PhotocraftApp, token: &str, file_id: &str, name: &str) -> Result<(), String> {
    if app.telegram_job.is_some() {
        return Err("A Telegram send is already running.".to_string());
    }
    let Some(download) = app.services.telegram_download.clone() else {
        return Err("Telegram sending needs the desktop app.".to_string());
    };
    let token = token.to_string();
    let file_id = file_id.to_string();
    let name = name.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let token_for_job = token.clone();
    let name_for_status = name.clone();
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (download, token, file_id, name, tx);
        return Err("Telegram sending needs the desktop app.".to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::Builder::new()
        .name("pc-tg-get".into())
        .spawn(move || {
            let result = match download(&token, &file_id) {
                Ok(bytes) => {
                    let path = std::env::temp_dir().join(format!("photocraft-telegram-in-{}-{name}", std::process::id()));
                    match std::fs::write(&path, &bytes) {
                        Ok(()) => Ok(format!("OPEN\t{}", path.display())),
                        Err(error) => Err(error.to_string()),
                    }
                }
                Err(error) => Err(redact(&error, &token)),
            };
            let _ = tx.send(result);
        })
        .map_err(|error| error.to_string())?;
    app.ui.status = format!("Opening {name_for_status}…");
    app.ui.status_error = false;
    app.telegram_job = Some(Job { rx, path: String::new(), temporary: false, preview: None, name: name_for_status, token: token_for_job });
    Ok(())
}

fn fetch_chats(app: &PhotocraftApp, token: &str) -> Result<Vec<Value>, String> {
    validate_token(token)?;
    let Some(get) = app.services.telegram_get.as_ref() else {
        return Err("Telegram sending needs the desktop app.".to_string());
    };
    let url = format!("https://api.telegram.org/bot{token}/getUpdates");
    let body = get(&url).map_err(|error| redact(&error, token))?;
    Ok(chats_from_updates(&body))
}

/// Chats that have talked to the bot, newest last, one row per id.
pub fn chats_from_updates(text: &str) -> Vec<Value> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<Value>(text) else { return out };
    let Some(items) = v.get("result").and_then(Value::as_array) else { return out };
    for item in items {
        let chat = item.get("message").or_else(|| item.get("edited_message")).or_else(|| item.get("channel_post")).and_then(|m| m.get("chat"));
        let Some(chat) = chat else { continue };
        let Some(id) = chat.get("id").and_then(Value::as_i64) else { continue };
        let id = id.to_string();
        if !seen.insert(id.clone()) {
            continue;
        }
        let label = chat.get("title").or_else(|| chat.get("username")).or_else(|| chat.get("first_name")).and_then(Value::as_str).unwrap_or("").to_string();
        let label = if label.is_empty() { id.clone() } else { format!("{label} ({id})") };
        out.push(json!({"id": id, "label": label}));
    }
    out
}

/// Link to the sent message. A public chat gets `https://t.me/…`. A private chat has no web
/// page; Telegram's own app link still opens the message.
pub fn message_link(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let result = v.get("result")?;
    let message = result.as_array().and_then(|items| items.first()).unwrap_or(result);
    let message_id = message.get("message_id")?.as_i64()?;
    let chat = message.get("chat")?;
    if let Some(username) = chat.get("username").and_then(Value::as_str).filter(|name| !name.is_empty()) {
        return Some(format!("https://t.me/{username}/{message_id}"));
    }
    let id = chat.get("id")?.as_i64()?;
    if let Some(internal) = id.to_string().strip_prefix("-100").filter(|rest| !rest.is_empty()) {
        return Some(format!("https://t.me/c/{internal}/{message_id}"));
    }
    if id > 0 {
        Some(format!("tg://openmessage?user_id={id}&message_id={message_id}"))
    } else {
        Some(format!("tg://openmessage?chat_id={id}&message_id={message_id}"))
    }
}

/// Documents and photos in a successful send reply, newest first.
pub fn sent_files_from_reply(body: &str) -> Vec<Value> {
    let Ok(value) = serde_json::from_str::<Value>(body) else { return Vec::new() };
    let Some(result) = value.get("result") else { return Vec::new() };
    let messages: Vec<&Value> = result.as_array().map(|items| items.iter().collect()).unwrap_or_else(|| vec![result]);
    let mut out = Vec::new();
    for message in messages {
        push_message_file(message, &mut out);
    }
    out
}

fn push_message_file(message: &Value, out: &mut Vec<Value>) {
    let message_id = message.get("message_id").and_then(Value::as_i64).unwrap_or(0);
    if let Some(document) = message.get("document") {
        let file_id = document.get("file_id").and_then(Value::as_str).unwrap_or("");
        if file_id.is_empty() {
            return;
        }
        let name = document.get("file_name").and_then(Value::as_str).unwrap_or("file");
        out.push(json!({"id": file_id, "name": name, "label": name}));
        return;
    }
    let Some(photos) = message.get("photo").and_then(Value::as_array) else { return };
    let Some(photo) = photos.iter().max_by_key(|p| p.get("file_size").and_then(Value::as_u64).unwrap_or(0)) else { return };
    let file_id = photo.get("file_id").and_then(Value::as_str).unwrap_or("");
    if file_id.is_empty() {
        return;
    }
    let name = format!("telegram-{message_id}.jpg");
    out.push(json!({"id": file_id, "name": name, "label": name}));
}

struct SendReport {
    urls: Vec<String>,
    files: Vec<Value>,
    pin_failed: bool,
}

/// The send thread returns either the new JSON report or, from an older build, a bare link.
fn send_report(body: &str) -> SendReport {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(urls) = value.get("urls").and_then(Value::as_array) {
            let urls = urls.iter().filter_map(|v| v.as_str().map(str::to_string)).filter(|url| !url.is_empty()).collect();
            let files = value.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
            let pin_failed = value.get("pinFailed").and_then(Value::as_bool).unwrap_or(false);
            return SendReport { urls, files, pin_failed };
        }
    }
    let (text, pin_failed) = body.split_once("\nPINFAIL").map(|(ok, _)| (ok.to_string(), true)).unwrap_or((body.to_string(), false));
    let urls = if text.starts_with("http") || text.starts_with("tg:") {
        text.lines().map(str::to_string).filter(|line| !line.is_empty()).collect()
    } else {
        Vec::new()
    };
    SendReport { urls, files: Vec::new(), pin_failed }
}

fn remember_sent_files(app: &mut PhotocraftApp, fresh: &[Value]) {
    if fresh.is_empty() {
        return;
    }
    let mut kept = fresh.to_vec();
    for old in app.telegram.sent_files.drain(..) {
        let id = old.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || kept.iter().any(|row| row.get("id").and_then(Value::as_str) == Some(id)) {
            continue;
        }
        kept.push(old);
        if kept.len() == 10 {
            break;
        }
    }
    app.telegram.sent_files = kept;
    let text = settings_json(&app.telegram);
    if let Some(save) = app.services.save_telegram.as_mut() {
        let _ = save(&text);
    }
}

fn read_file(path: &str) -> Result<(String, Vec<u8>), String> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        return Err("Telegram sending needs the desktop app.".to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let meta = std::fs::metadata(path).map_err(|e| read_error(path, &e.to_string()))?;
        if !meta.is_file() {
            return Err(read_error(path, "not a file"));
        }
        if meta.len() > MAX_BYTES {
            return Err("That file is larger than Telegram accepts (50 MB).".to_string());
        }
        let bytes = std::fs::read(path).map_err(|e| read_error(path, &e.to_string()))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("That file is larger than Telegram accepts (50 MB).".to_string());
        }
        Ok((file_name(path), bytes))
    }
}

fn read_error(path: &str, error: &str) -> String {
    let name = file_name(path);
    format!("Could not read {name}: {}", redact(error, ""))
}

fn file_secs(path: &str) -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        0
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
    }
}

fn file_name(path: &str) -> String {
    let raw = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let name: String = raw.chars().filter(|c| !matches!(c, '"' | '\\' | '\n' | '\r')).take(180).collect();
    if name.is_empty() { "file".to_string() } else { name }
}

const CAPTION_LIMIT: usize = 1024;

fn caption(path: &str, name: &str, bytes: u64, secs: u64) -> String {
    let format = std::path::Path::new(path).extension().and_then(|ext| ext.to_str()).unwrap_or("").to_ascii_uppercase();
    let format = if format.is_empty() { "—".to_string() } else { format };
    let mut text = [
        crate::i18n::fmt(tl!("*File:* `{name}`"), &[("name", &markdown_code(name))]),
        crate::i18n::fmt(tl!("*Format:* {format}"), &[("format", &markdown_text(&format))]),
        crate::i18n::fmt(tl!("*Size:* {size}"), &[("size", &crate::sizing::human_bytes(bytes as f64))]),
        crate::i18n::fmt(tl!("*Time:* {time}"), &[("time", &utc_stamp(secs))]),
        crate::i18n::fmt(tl!("*Path:* `{path}`"), &[("path", &markdown_code(path))]),
    ]
    .join("\n");
    if text.chars().count() > CAPTION_LIMIT {
        text = text.chars().take(CAPTION_LIMIT).collect();
    }
    text
}

/// Text inside a Markdown code span. Backticks would end the span.
fn markdown_code(text: &str) -> String {
    text.replace('`', "'")
}

/// Markdown text outside a code span.
fn markdown_text(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if matches!(c, '_' | '*' | '`' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `multipart/form-data` body and its `Content-Type` value.
pub fn multipart(chat_id: &str, caption: &str, filename: &str, bytes: &[u8], reply_to: Option<i64>) -> (String, Vec<u8>) {
    let boundary = boundary_for(bytes);
    let mut body = Vec::new();
    part(&mut body, &boundary, "chat_id", chat_id);
    part(&mut body, &boundary, "caption", caption);
    part(&mut body, &boundary, "parse_mode", "Markdown");
    if let Some(message_id) = reply_to {
        // The preview answers the document message, so the chat shows them as one pair.
        let reply = format!("{{\"message_id\":{message_id},\"allow_sending_without_reply\":true}}");
        part(&mut body, &boundary, "reply_parameters", &reply);
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn part(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
}

fn boundary_for(bytes: &[u8]) -> String {
    for n in 0..8u32 {
        let boundary = format!("PcTelegram{n:08x}");
        if !bytes.windows(boundary.len()).any(|w| w == boundary.as_bytes()) {
            return boundary;
        }
    }
    format!("PcTelegram{}", bytes.len())
}

pub fn validate_token(token: &str) -> Result<(), String> {
    let Some((id, secret)) = token.split_once(':') else {
        return Err("The bot token should look like 123456789:ABC…".to_string());
    };
    if (6..=12).contains(&id.len())
        && id.chars().all(|c| c.is_ascii_digit())
        && (20..=80).contains(&secret.len())
        && secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !token.contains('/')
    {
        return Ok(());
    }
    Err("The bot token should look like 123456789:ABC…".to_string())
}

/// One id, or several separated by commas, semicolons or spaces.
pub fn parse_chats(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for part in text.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        validate_chat(part)?;
        if !out.iter().any(|have: &String| have == part) {
            out.push(part.to_string());
        }
    }
    if out.is_empty() { Err("Enter a chat id (a number) or a public @channelname.".to_string()) } else { Ok(out) }
}

fn chat_selected(current: &str, id: &str) -> bool {
    current.split(|c: char| c == ',' || c == ';' || c.is_whitespace()).any(|part| part.trim() == id)
}

fn append_chat(current: &str, id: &str) -> String {
    if chat_selected(current, id) {
        current.to_string()
    } else if current.trim().is_empty() {
        id.to_string()
    } else {
        format!("{}, {id}", current.trim().trim_end_matches([',', ';']))
    }
}

fn png_preview(app: &mut PhotocraftApp, path: &str) -> Result<Option<String>, String> {
    if !app.telegram.also_png {
        return Ok(None);
    }
    let ext = std::path::Path::new(path).extension().and_then(|ext| ext.to_str()).unwrap_or("").to_ascii_lowercase();
    if !matches!(ext.as_str(), "psd" | "psb") {
        return Ok(None);
    }
    export_temp(app, "png").map(Some)
}

#[allow(clippy::too_many_arguments)]
fn send_to_chats(
    send: &crate::TelegramSendFn,
    token: &str,
    chats: &[String],
    caption: &str,
    name: &str,
    bytes: &[u8],
    preview: Option<&(String, Vec<u8>, String)>,
    pin: bool,
) -> Result<String, String> {
    let mut links = Vec::new();
    let mut files = Vec::new();
    let mut pin_failed = false;
    for chat in chats {
        let (method, content_type, body) = if let Some((preview_name, preview_bytes, _)) = preview {
            let (content_type, body) = media_group(chat, caption, &[(name, bytes), (preview_name.as_str(), preview_bytes.as_slice())]);
            ("sendMediaGroup", content_type, body)
        } else if is_image_name(name) {
            let (content_type, body) = photo(chat, caption, name, bytes);
            ("sendPhoto", content_type, body)
        } else {
            let (content_type, body) = multipart(chat, caption, name, bytes, None);
            ("sendDocument", content_type, body)
        };
        let url = format!("https://api.telegram.org/bot{token}/{method}");
        let text = match send(&url, &content_type, &body) {
            Ok(text) => text,
            Err(error) => {
                let error = redact(&error, token);
                if links.is_empty() {
                    return Err(error);
                }
                return Err(format!("Sent to the earlier chats. The next chat failed: {error}"));
            }
        };
        files.extend(sent_files_from_reply(&text));
        if let Some(link) = message_link(&text) {
            links.push(link);
        }
        if pin {
            if let Some(message_id) = first_message_id(&text) {
                let pin_url = format!("https://api.telegram.org/bot{token}/pinChatMessage");
                let pin_body = format!("chat_id={chat}&message_id={message_id}");
                if send(&pin_url, "application/x-www-form-urlencoded", pin_body.as_bytes()).is_err() {
                    pin_failed = true;
                }
            } else {
                pin_failed = true;
            }
        }
    }
    Ok(json!({"urls": links, "files": files, "pinFailed": pin_failed}).to_string())
}

fn is_image_name(name: &str) -> bool {
    let ext = std::path::Path::new(name).extension().and_then(|ext| ext.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
}

/// Album of two documents. Telegram will not mix a document with a photo.
pub fn media_group(chat_id: &str, caption: &str, files: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = format!("PcTelegramAlbum{}", files.iter().map(|(_, bytes)| bytes.len()).sum::<usize>());
    let mut media = String::from("[");
    for (index, _) in files.iter().enumerate() {
        if index > 0 {
            media.push(',');
        }
        if index == 0 {
            media.push_str(&format!(
                "{{\"type\":\"document\",\"media\":\"attach://f{index}\",\"caption\":{},\"parse_mode\":\"Markdown\"}}",
                serde_json::to_string(caption).unwrap_or_else(|_| "\"\"".to_string())
            ));
        } else {
            media.push_str(&format!("{{\"type\":\"document\",\"media\":\"attach://f{index}\"}}"));
        }
    }
    media.push(']');
    let mut body = Vec::new();
    part(&mut body, &boundary, "chat_id", chat_id);
    part(&mut body, &boundary, "media", &media);
    for (index, (name, bytes)) in files.iter().enumerate() {
        let safe = file_name(name);
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"f{index}\"; filename=\"{safe}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

pub fn photo(chat_id: &str, caption: &str, filename: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = format!("PcTelegramPhoto{}", bytes.len());
    let mut body = Vec::new();
    part(&mut body, &boundary, "chat_id", chat_id);
    part(&mut body, &boundary, "caption", caption);
    part(&mut body, &boundary, "parse_mode", "Markdown");
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"photo\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// The 10 newest photos and documents a bot was sent. Newest first.
pub fn files_from_updates(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(value) = serde_json::from_str::<Value>(text) else { return out };
    let Some(items) = value.get("result").and_then(Value::as_array) else { return out };
    for item in items.iter().rev() {
        let Some(message) = item.get("message").or_else(|| item.get("channel_post")) else { continue };
        let message_id = message.get("message_id").and_then(Value::as_i64).unwrap_or(0);
        if let Some(document) = message.get("document") {
            let file_id = document.get("file_id").and_then(Value::as_str).unwrap_or("");
            let size = document.get("file_size").and_then(Value::as_u64).unwrap_or(0);
            if file_id.is_empty() || size > 20 * 1024 * 1024 {
                continue;
            }
            let name = document.get("file_name").and_then(Value::as_str).unwrap_or("file");
            out.push(json!({"id": file_id, "name": name, "label": format!("{name} ({message_id})")}));
        } else if let Some(photos) = message.get("photo").and_then(Value::as_array) {
            let photo = photos.iter().max_by_key(|p| p.get("file_size").and_then(Value::as_u64).unwrap_or(0));
            let Some(photo) = photo else { continue };
            let file_id = photo.get("file_id").and_then(Value::as_str).unwrap_or("");
            let size = photo.get("file_size").and_then(Value::as_u64).unwrap_or(0);
            if file_id.is_empty() || size > 20 * 1024 * 1024 {
                continue;
            }
            let name = format!("telegram-{message_id}.jpg");
            out.push(json!({"id": file_id, "name": name, "label": name}));
        }
        if out.len() == 10 {
            break;
        }
    }
    out
}

pub fn first_message_id(body: &str) -> Option<i64> {
    let value: Value = serde_json::from_str(body).ok()?;
    let result = value.get("result")?;
    result
        .get("message_id")
        .and_then(Value::as_i64)
        .or_else(|| result.as_array().and_then(|items| items.first()).and_then(|item| item.get("message_id")).and_then(Value::as_i64))
}

/// `message_id` from a successful `sendDocument` reply. Absent when the body is not that shape.
pub fn sent_message_id(body: &str) -> Option<i64> {
    let value: Value = serde_json::from_str(body).ok()?;
    value.get("result")?.get("message_id")?.as_i64()
}

/// Split a settings document into the part safe to store on disk and the bot token.
pub fn without_token(text: &str) -> Result<(String, String), String> {
    let mut value: Value = serde_json::from_str(text).unwrap_or_else(|_| json!({}));
    let token = value.get("botToken").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(object) = value.as_object_mut() {
        object.remove("botToken");
    }
    Ok((serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()), token))
}

/// Put `token` back into a settings document that was stored without it.
pub fn with_token(text: &str, token: &str) -> String {
    let mut value: Value = serde_json::from_str(text).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        object.insert("botToken".into(), json!(token));
    }
    serde_json::to_string(&value).unwrap_or_else(|_| text.to_string())
}

pub fn validate_chat(chat: &str) -> Result<(), String> {
    let ok = if let Some(name) = chat.strip_prefix('@') {
        (5..=32).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else {
        let digits = chat.strip_prefix('-').unwrap_or(chat);
        (1..=20).contains(&digits.len()) && digits.chars().all(|c| c.is_ascii_digit()) && chat != "-"
    };
    if ok { Ok(()) } else { Err("Enter a chat id (a number) or a public @channelname.".to_string()) }
}

/// Drop the bot token if a transport error repeated the request URL.
pub fn redact(text: &str, token: &str) -> String {
    if token.len() < 8 { text.to_string() } else { text.replace(token, "…") }
}

/// UTC `YYYY-MM-DD HH:MM` from seconds since the Unix epoch.
pub fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", tod / 3600, (tod % 3600) / 60)
}

/// Howard Hinnant's civil-from-days, days counted from 1970-01-01.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn token() -> String {
        format!("123456789:{}", "A".repeat(35))
    }

    #[test]
    fn broken_settings_file_is_not_set_up() {
        let s = settings_from_json("not json");
        assert!(!s.ready());
        assert!(!s.send_on_save && !s.send_on_save_as && !s.send_on_export);
        let s = settings_from_json(r#"{"botToken":"  ","chatId":"12","sendAfterExport":true}"#);
        assert!(!s.ready());
        assert!(!s.send_on_save && s.send_on_save_as && s.send_on_export);
    }

    #[test]
    fn token_and_chat_rules() {
        assert!(validate_token(&token()).is_ok());
        assert!(validate_token("123:short").is_err());
        assert!(validate_token(&format!("123456789:{}", "A".repeat(35))).is_ok());
        assert!(validate_token("123456789/secret").is_err());
        assert!(validate_chat("123456789").is_ok());
        assert!(validate_chat("-100123").is_ok());
        assert!(validate_chat("@my_channel").is_ok());
        assert!(validate_chat("@no").is_err());
        assert!(validate_chat("hello").is_err());
        assert!(validate_chat("-").is_err());
    }

    #[test]
    fn utc_stamp_is_the_civil_date() {
        assert_eq!(utc_stamp(1_577_836_800), "2020-01-01 00:00 UTC");
    }

    #[test]
    fn caption_names_the_file_without_the_token() {
        let text = caption("/tmp/photo_1.psd", "photo_1.psd", 2048, 1_577_836_800);
        assert!(text.contains("photo_1.psd"), "{text}");
        assert!(text.contains("PSD"), "{text}");
        assert!(text.contains("2020-01-01"), "{text}");
        assert!(text.contains("/tmp/photo_1.psd"), "{text}");
        assert!(!text.contains('#'), "{text}");
        let (_, body) = multipart("1", &text, "photo_1.psd", b"x", None);
        let raw = String::from_utf8_lossy(&body);
        assert!(raw.contains("name=\"parse_mode\""));
        assert!(raw.contains("Markdown"));
    }

    #[test]
    fn multipart_carries_the_file_and_not_the_token() {
        let (ctype, body) = multipart("99", "caption", "photo.png", b"PNGDATA", None);
        assert!(ctype.starts_with("multipart/form-data; boundary="));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"chat_id\""));
        assert!(text.contains("99"));
        assert!(text.contains("filename=\"photo.png\""));
        assert!(text.contains("PNGDATA"));
        assert!(!text.contains("reply_parameters"));
        assert!(!text.contains("123456789"));
    }

    #[test]
    fn album_sends_two_documents_and_a_photo_is_a_photo() {
        let (_, body) = media_group("99", "cap", &[("a.psd", b"PSD"), ("a.png", b"PNG")]);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("sendMediaGroup") || text.contains("attach://f0"));
        assert!(text.contains("attach://f1"));
        assert!(text.contains("\"type\":\"document\""));
        assert!(!text.contains("reply_parameters"));
        let (_, photo_body) = photo("99", "cap", "a.png", b"PNG");
        let photo_text = String::from_utf8_lossy(&photo_body);
        assert!(photo_text.contains("name=\"photo\""));
        assert!(!photo_text.contains("name=\"document\""));
        assert_eq!(first_message_id(r#"{"ok":true,"result":[{"message_id":7}]}"#), Some(7));
    }

    #[test]
    fn inbox_keeps_the_ten_newest_files() {
        let mut messages = Vec::new();
        for n in 0..12 {
            messages.push(format!(r#"{{"message":{{"message_id":{n},"document":{{"file_id":"id{n}","file_name":"f{n}.psd","file_size":10}}}}}}"#));
        }
        let text = format!(r#"{{"ok":true,"result":[{}]}}"#, messages.join(","));
        let files = files_from_updates(&text);
        assert_eq!(files.len(), 10);
        assert_eq!(files[0].get("name").and_then(|v| v.as_str()), Some("f11.psd"));
    }

    #[test]
    fn command_without_setup_opens_the_dialog_and_does_not_send() {
        let sent = Arc::new(Mutex::new(0u32));
        let count = sent.clone();
        let services = crate::Services {
            telegram_send: Some(Arc::new(move |_, _, _| {
                *count.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                Ok("{}".to_string())
            })),
            save_telegram: Some(Box::new(|_| Ok(()))),
            ..Default::default()
        };
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
        let result = crate::menus::invoke(&mut app, &egui::Context::default(), "file.sendTelegram", json!({})).unwrap();
        assert!(result.get("dialog").is_some());
        assert_eq!(*sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner), 0);
    }

    /// A generic command dialog for this id used to show only the raw id and OK/Cancel.
    #[test]
    fn generic_command_opener_shows_the_telegram_form() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        let id = crate::dialogs::open_command_dialog(&mut app, "file.sendTelegram", "file.sendTelegram");
        let dialog = app.ui.dialogs.iter().find(|d| d.id == id).expect("dialog");
        assert!(owns(&dialog.fields));
        assert_eq!(dialog.fields.get("__label").and_then(Value::as_str), Some("Send to Telegram"));
        assert_ne!(crate::dialogs::title(dialog), "file.sendTelegram");
    }

    #[test]
    fn command_sends_the_document_file_and_hides_the_token() {
        let dir = std::env::temp_dir().join(format!("photocraft-telegram-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("photo.png");
        std::fs::write(&path, b"PNGDATA").unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let slot = seen.clone();
        let services = crate::Services {
            telegram_send: Some(Arc::new(move |url, _, body| {
                let text = String::from_utf8_lossy(body).into_owned();
                *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = format!("{url}\n{text}");
                Err(format!("refused {url}"))
            })),
            save_telegram: Some(Box::new(|_| Ok(()))),
            ..Default::default()
        };
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
        app.session.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
        app.session.active_mut().unwrap().path = Some(path.display().to_string());
        let mut fields = serde_json::Map::new();
        fields.insert("token".into(), json!(token()));
        fields.insert("chat".into(), json!("42, 42"));
        fields.insert("format".into(), json!("file"));
        let started = confirm(&mut app, &fields).unwrap();
        assert_eq!(started["sending"], true);
        assert_eq!(parse_chats(&app.telegram.chat_id).unwrap(), vec!["42".to_string()]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while app.telegram_job.is_some() && std::time::Instant::now() < deadline {
            poll(&mut app, &egui::Context::default());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.ui.status_error, "{}", app.ui.status);
        assert!(!app.ui.status.contains(&token()), "{}", app.ui.status);
        let dumped = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert!(dumped.contains("PNGDATA"), "{dumped}");
        assert!(dumped.contains("name=\"chat_id\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn after_export_stays_quiet_until_the_option_is_on() {
        let sent = Arc::new(Mutex::new(0u32));
        let count = sent.clone();
        let mut app = PhotocraftApp::new(
            photocraft_engine::Session::new(),
            crate::Services {
                telegram_send: Some(Arc::new(move |_, _, _| {
                    *count.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                    Ok("{}".to_string())
                })),
                ..Default::default()
            },
        );
        app.telegram = Settings { bot_token: token(), chat_id: "42".into(), ..Settings::default() };
        after_written(&mut app, "/tmp/does-not-matter.png", WriteKind::Save);
        after_written(&mut app, "/tmp/does-not-matter.png", WriteKind::Export);
        assert_eq!(*sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner), 0);
        app.telegram.bot_token.clear();
        app.telegram.send_on_export = true;
        after_written(&mut app, "/tmp/out.png", WriteKind::Export);
        assert!(dialog_open(&app));
        assert_eq!(*sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner), 0);
    }

    #[test]
    fn message_links_and_chat_list() {
        let channel = r#"{"ok":true,"result":{"message_id":9,"chat":{"id":-100555,"type":"channel"}}}"#;
        assert_eq!(message_link(channel).as_deref(), Some("https://t.me/c/555/9"));
        let public = r#"{"ok":true,"result":{"message_id":4,"chat":{"id":1,"username":"mychan"}}}"#;
        assert_eq!(message_link(public).as_deref(), Some("https://t.me/mychan/4"));
        assert_eq!(
            message_link(r#"{"ok":true,"result":{"message_id":1,"chat":{"id":42}}}"#).as_deref(),
            Some("tg://openmessage?user_id=42&message_id=1")
        );
        let sent = r#"{"ok":true,"result":{"message_id":2,"document":{"file_id":"abc","file_name":"hill.psd"}}}"#;
        assert_eq!(sent_files_from_reply(sent)[0].get("id").and_then(Value::as_str), Some("abc"));
        let updates = r#"{"ok":true,"result":[{"message":{"chat":{"id":42,"first_name":"Sam"}}},{"message":{"chat":{"id":42,"first_name":"Sam"}}},{"channel_post":{"chat":{"id":-1007,"title":"Desk"}}}]}"#;
        let chats = chats_from_updates(updates);
        assert_eq!(chats.len(), 2);
        assert_eq!(chats[0]["id"], "42");
        assert_eq!(chats[1]["label"], "Desk (-1007)");
    }

    #[test]
    fn send_returns_before_telegram_answers_and_then_reports_the_link() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let wait = Arc::new(Mutex::new(wait));
        let held = wait.clone();
        let mut app = PhotocraftApp::new(
            photocraft_engine::Session::new(),
            crate::Services {
                export: Some(Box::new(|_, _, _| Ok((b"PNGDATA".to_vec(), Vec::new())))),
                telegram_send: Some(Arc::new(move |_, _, body| {
                    assert!(body.windows(7).any(|w| w == b"PNGDATA"));
                    let _ = held.lock().unwrap_or_else(std::sync::PoisonError::into_inner).recv();
                    Ok(r#"{"ok":true,"result":{"message_id":4,"chat":{"id":-100999,"username":"mychan"}}}"#.to_string())
                })),
                save_telegram: Some(Box::new(|_| Ok(()))),
                ..Default::default()
            },
        );
        app.session.execute("file.new", json!({"width": 8, "height": 8, "name": "Sketch"})).unwrap();
        let mut fields = serde_json::Map::new();
        fields.insert("token".into(), json!(token()));
        fields.insert("chat".into(), json!("42"));
        fields.insert("format".into(), json!("png"));
        let started = confirm(&mut app, &fields).unwrap();
        assert_eq!(started["sending"], true);
        let temp = app.telegram_job.as_ref().map(|job| job.path.clone()).unwrap();
        assert!(std::path::Path::new(&temp).exists(), "{temp}");
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while app.telegram_job.is_some() && std::time::Instant::now() < deadline {
            poll(&mut app, &egui::Context::default());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!app.ui.status_error, "{}", app.ui.status);
        assert!(app.ui.status.contains("https://t.me/mychan/4"), "{}", app.ui.status);
        assert!(!std::path::Path::new(&temp).exists(), "{temp}");
    }

    #[test]
    fn telegram_copy_is_translated_in_every_complete_catalog() {
        const STRINGS: &[&str] = &[
            "Send to Telegram…",
            "Send to Telegram",
            "Bot token",
            "Send as",
            "Saved file",
            "Saved file sends the copy on disk. PSD and PNG are a new copy of the open document, including changes you have not saved.",
            "Chats or channels. Several are allowed, separated by commas.",
            "Create a bot with BotFather, then send that bot any message. Use Find chats to fill the id, or paste a number or @channelname.",
            "Open the official bot guide",
            "Find chats for this token",
            "No chats yet. Send your bot a message, then try again.",
            "Also send when I Save",
            "Also send when I Save As",
            "Also send when I Export",
            "Also send when I Save a Copy",
            "Also send a PNG preview with a PSD",
            "Send automatically",
            "The bot token is kept in the system keychain, not in PhotoCraft's settings file.",
            "Sending needs an internet connection.",
            "Pin the message",
            "Pinning needs the bot to be an admin. If it is not, the file is still sent.",
            "Get from Telegram",
            "No files yet. Send your bot a file or a photo, then try again.",
            "*File:* `{name}`",
            "*Size:* {size}",
            "*Time:* {time}",
            "*Path:* `{path}`",
        ];
        for lang in crate::i18n::Lang::all().filter(|l| l.complete_menus()) {
            for source in STRINGS {
                assert_ne!(crate::i18n::tr(lang, source), *source, "{}: {source}", lang.code());
            }
        }
    }
}
