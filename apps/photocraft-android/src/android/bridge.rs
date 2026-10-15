//! Thread-safe bridge between Rust services and the Java Storage Access Framework host.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use jni::EnvUnowned;
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JClass, JString};
use photocraft_ui_egui::{FileDialogAnswer, FileDialogReply, FileDialogRequest};

use crate::tablet_gestures::TabletAction;

const WRITE_TIMEOUT: Duration = Duration::from_secs(120);
const VIRTUAL_DOCUMENT_PREFIX: &str = "android://document/";
const MAX_TABLET_ACTIONS: usize = 32;

#[derive(Clone, Debug)]
enum PickerRequest {
    Open { multiple: bool },
    Save { suggested: String },
}

#[derive(Debug)]
struct SaveRequest {
    uri: String,
    bytes: Vec<u8>,
    dispatched: bool,
    result: Option<Result<(), String>>,
}

#[derive(Default)]
struct State {
    picker: Option<PickerRequest>,
    reply: Option<FileDialogReply>,
    save_suggested: Option<String>,
    save_targets: HashMap<String, String>,
    write: Option<SaveRequest>,
    tablet_actions: VecDeque<TabletAction>,
    barrel_held: bool,
    clipboard_incoming: Option<(u32, u32, Vec<u8>)>,
    clipboard_outgoing: Option<Vec<u8>>,
}

/// One pending picker or write at a time. PhotoCraft itself serializes file dialogs, and the
/// bridge rejects an unexpected second write rather than risking writing bytes to the wrong URI.
pub(crate) struct Bridge {
    state: Mutex<State>,
    write_ready: Condvar,
    next_document: AtomicU64,
}

impl Default for Bridge {
    fn default() -> Self {
        Self { state: Mutex::new(State::default()), write_ready: Condvar::new(), next_document: AtomicU64::new(1) }
    }
}

static BRIDGE: OnceLock<Arc<Bridge>> = OnceLock::new();

pub(crate) fn shared() -> Arc<Bridge> {
    BRIDGE.get_or_init(|| Arc::new(Bridge::default())).clone()
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Bridge {
    pub(crate) fn request(&self, request: FileDialogRequest, reply: FileDialogReply) {
        if matches!(request, FileDialogRequest::Folder { .. }) {
            reply.send(Some(FileDialogAnswer::Unavailable("Android does not grant folders to apps. Select individual documents instead.".into())));
            return;
        }
        let mut state = lock(&self.state);
        if state.reply.is_some() || state.picker.is_some() {
            drop(state);
            reply.send(Some(FileDialogAnswer::Unavailable("another Android document picker is already open".into())));
            return;
        }
        let picker = match request {
            FileDialogRequest::Open { multiple, .. } => PickerRequest::Open { multiple },
            FileDialogRequest::Save { suggested } => {
                state.save_suggested = Some(suggested.clone());
                PickerRequest::Save { suggested }
            }
            FileDialogRequest::Folder { .. } => return,
        };
        state.picker = Some(picker);
        state.reply = Some(reply);
    }

    fn take_picker_request(&self) -> Option<String> {
        let mut state = lock(&self.state);
        match state.picker.take()? {
            PickerRequest::Open { multiple } => Some(if multiple { "open:multiple".to_string() } else { "open:single".to_string() }),
            PickerRequest::Save { suggested } => Some(format!("save:{}", safe_file_name(&suggested))),
        }
    }

    fn picked_paths(&self, packed_paths: &str) {
        let paths: Vec<String> = packed_paths.split('\n').filter(|path| !path.is_empty()).map(ToOwned::to_owned).collect();
        let reply = {
            let mut state = lock(&self.state);
            state.save_suggested = None;
            state.reply.take()
        };
        if let Some(reply) = reply {
            if paths.is_empty() {
                reply.send(Some(FileDialogAnswer::Unavailable("Android did not return a readable document.".into())));
            } else {
                reply.send(Some(FileDialogAnswer::Paths(paths)));
            }
        }
    }

    fn picked_save_target(&self, uri: &str) {
        let reply_and_target = {
            let mut state = lock(&self.state);
            let suggested = state.save_suggested.take().unwrap_or_else(|| "PhotoCraft.pcraft".to_string());
            let document = self.next_document.fetch_add(1, Ordering::Relaxed);
            let target = format!("{VIRTUAL_DOCUMENT_PREFIX}{document}/{}", safe_file_name(&suggested));
            state.save_targets.insert(target.clone(), uri.to_string());
            state.reply.take().map(|reply| (reply, target))
        };
        if let Some((reply, target)) = reply_and_target {
            reply.send(Some(FileDialogAnswer::SaveTo(target)));
        }
    }

    fn picker_cancelled(&self) {
        let reply = {
            let mut state = lock(&self.state);
            state.picker = None;
            state.save_suggested = None;
            state.reply.take()
        };
        if let Some(reply) = reply {
            reply.send(None);
        }
    }

    fn picker_failed(&self, error: String) {
        let reply = {
            let mut state = lock(&self.state);
            state.picker = None;
            state.save_suggested = None;
            state.reply.take()
        };
        if let Some(reply) = reply {
            reply.send(Some(FileDialogAnswer::Unavailable(error)));
        }
    }

    pub(crate) fn write(&self, target: &str, bytes: &[u8]) -> Result<(), String> {
        let uri = {
            let state = lock(&self.state);
            state.save_targets.get(target).cloned().ok_or_else(|| "Choose a destination with Save As before writing on Android.".to_string())?
        };
        let mut state = lock(&self.state);
        if state.write.is_some() {
            return Err("another Android document write is already in progress".into());
        }
        state.write = Some(SaveRequest { uri, bytes: bytes.to_vec(), dispatched: false, result: None });
        let deadline = Instant::now() + WRITE_TIMEOUT;
        loop {
            if let Some(result) = state.write.as_mut().and_then(|write| write.result.take()) {
                state.write = None;
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                state.write = None;
                return Err("Android did not finish writing the document within two minutes.".into());
            }
            let (next, _) = match self.write_ready.wait_timeout(state, deadline.saturating_duration_since(now)) {
                Ok(value) => value,
                Err(error) => error.into_inner(),
            };
            state = next;
        }
    }

    fn take_save_uri(&self) -> Option<String> {
        let mut state = lock(&self.state);
        let write = state.write.as_mut()?;
        if write.dispatched {
            return None;
        }
        write.dispatched = true;
        Some(write.uri.clone())
    }

    fn take_save_bytes(&self) -> Option<Vec<u8>> {
        let state = lock(&self.state);
        state.write.as_ref().filter(|write| write.dispatched).map(|write| write.bytes.clone())
    }

    fn save_finished(&self, success: bool, error: String) {
        let mut state = lock(&self.state);
        if let Some(write) = state.write.as_mut() {
            write.result = Some(if success { Ok(()) } else { Err(if error.is_empty() { "Android could not write the document.".into() } else { error }) });
            self.write_ready.notify_all();
        }
    }

    pub(crate) fn take_tablet_actions(&self) -> Vec<TabletAction> {
        lock(&self.state).tablet_actions.drain(..).collect()
    }

    pub(crate) fn barrel_held(&self) -> bool {
        lock(&self.state).barrel_held
    }

    fn tablet_action(&self, action: TabletAction) {
        let mut state = lock(&self.state);
        if let TabletAction::Barrel { pressed, .. } = action {
            state.barrel_held = pressed;
        }
        if state.tablet_actions.len() >= MAX_TABLET_ACTIONS {
            state.tablet_actions.pop_front();
        }
        state.tablet_actions.push_back(action);
    }

    pub(crate) fn set_clipboard_incoming_bytes(&self, bytes: &[u8]) {
        match photocraft_codecs::decode(bytes) {
            Ok(image) => {
                let w = image.width();
                let h = image.height();
                let rgba8 = image.to_rgba8();
                let mut state = lock(&self.state);
                state.clipboard_incoming = Some((w, h, rgba8));
            }
            Err(error) => {
                eprintln!("PhotoCraft cannot decode incoming clipboard image: {error}");
            }
        }
    }

    pub(crate) fn clear_clipboard_incoming(&self) {
        let mut state = lock(&self.state);
        state.clipboard_incoming = None;
    }

    pub(crate) fn get_clipboard_image(&self) -> Option<(u32, u32, Vec<u8>)> {
        let state = lock(&self.state);
        state.clipboard_incoming.clone()
    }

    pub(crate) fn set_clipboard_image(&self, w: u32, h: u32, rgba8: &[u8]) -> Result<(), String> {
        let image = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgba, rgba8.to_vec())
            .map_err(|e| format!("cannot create image: {e}"))?;
        let png = photocraft_codecs::encode(&image, photocraft_codecs::Format::Png, &photocraft_codecs::EncodeOptions::default())
            .map_err(|e| format!("cannot encode clipboard png: {e}"))?;
        let mut state = lock(&self.state);
        state.clipboard_incoming = Some((w, h, rgba8.to_vec()));
        state.clipboard_outgoing = Some(png);
        Ok(())
    }

    pub(crate) fn take_clipboard_outgoing(&self) -> Option<Vec<u8>> {
        let mut state = lock(&self.state);
        state.clipboard_outgoing.take()
    }
}

fn safe_file_name(suggested: &str) -> String {
    let base = suggested.rsplit(['/', '\\']).next().unwrap_or(suggested);
    let mut clean: String =
        base.chars().map(|character| if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') { character } else { '_' }).collect();
    if clean.is_empty() || clean.chars().all(|character| character == '.') {
        clean = "PhotoCraft.pcraft".into();
    }
    clean.truncate(120);
    clean
}

fn java_string(value: JString<'_>) -> String {
    value.to_string()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeTakePickerRequest<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| JString::from_str(env, shared().take_picker_request().unwrap_or_default())).resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativePickerOpened<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    paths: JString<'local>,
) {
    env.with_env(|_| {
        shared().picked_paths(&java_string(paths));
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativePickerSaveTarget<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    uri: JString<'local>,
) {
    env.with_env(|_| {
        shared().picked_save_target(&java_string(uri));
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativePickerCancelled<'local>(mut env: EnvUnowned<'local>, _class: JClass<'local>) {
    env.with_env(|_| {
        shared().picker_cancelled();
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativePickerFailed<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    error: JString<'local>,
) {
    env.with_env(|_| {
        shared().picker_failed(java_string(error));
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeTakeSaveUri<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| JString::from_str(env, shared().take_save_uri().unwrap_or_default())).resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeTakeSaveBytes<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jni::objects::JByteArray<'local> {
    env.with_env(|env| env.byte_array_from_slice(&shared().take_save_bytes().unwrap_or_default())).resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeSaveFinished<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    success: bool,
    error: JString<'local>,
) {
    env.with_env(|_| {
        shared().save_finished(success, java_string(error));
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeTwoFingerTap<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    x: f32,
    y: f32,
) {
    env.with_env(|_| {
        shared().tablet_action(TabletAction::SecondaryTap { x, y });
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeStylusBarrel<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    x: f32,
    y: f32,
    pressed: bool,
) {
    env.with_env(|_| {
        shared().tablet_action(TabletAction::Barrel { x, y, pressed });
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeClipboardImageChanged<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    bytes: jni::objects::JByteArray<'local>,
) {
    env.with_env(|env| {
        let data = env.convert_byte_array(&bytes)?;
        shared().set_clipboard_incoming_bytes(&data);
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeClipboardImageCleared<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) {
    env.with_env(|_| {
        shared().clear_clipboard_incoming();
        Ok::<(), jni::errors::Error>(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_storyteller_photocraft_PhotoCraftActivity_nativeTakeClipboardSetBytes<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jni::objects::JByteArray<'local> {
    env.with_env(|env| {
        let bytes = shared().take_clipboard_outgoing().unwrap_or_default();
        env.byte_array_from_slice(&bytes)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::safe_file_name;

    #[test]
    fn safe_file_names_keep_extensions_and_remove_separators() {
        assert_eq!(safe_file_name("/tmp/portrait.final.psd"), "portrait.final.psd");
        assert_eq!(safe_file_name("../name with / separators.png"), "_separators.png");
        assert_eq!(safe_file_name("..."), "PhotoCraft.pcraft");
    }
}
