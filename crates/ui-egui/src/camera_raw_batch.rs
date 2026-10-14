//! Several raw files opened together (dropped at once, File › Open with several chosen, the
//! system opening a set): one open-time Camera Raw dialog with a filmstrip of them all under the
//! preview, as Photoshop's Camera Raw shows several images.
//!
//! - Only the photo in the dialog is developed (as a raw opening alone is). The others show their
//!   camera's embedded preview, read off the UI thread without touching the sensor data, until
//!   they are shown; so twenty files cost one document while the dialog is up.
//! - Each photo keeps its own settings (and demosaic method); switching photos keeps them.
//! - A click shows that photo alone selected; Ctrl/Cmd-click adds it to the selection or takes it
//!   out, Shift-click selects the range from the last click, Ctrl/Cmd+A selects all, ← / → step
//!   through the strip. A setting changed with several selected applies that setting (its new
//!   value) to every selected photo; their other settings stay their own.
//! - Remove from Filmstrip (a thumbnail's ×, its context menu, Delete for the selection) takes
//!   photos out of the batch: they are not opened. Files on disk are never touched.
//! - **Open** opens the selected photos, each developed with its settings, one at a time (in the
//!   background in the desktop app); the others are not opened and their documents, if shown,
//!   are closed. **Cancel** opens none.
//!
//! Control channel: `filter.cameraRaw` `{"ui": {"filmstrip": {…}}}` and
//! `ui.inspect.cameraRaw.filmstrip` (see `docs/control-protocol.md`).

use std::sync::Arc;

use egui::{Color32, FontId, Rect as ERect, Sense, Stroke, TextureHandle, pos2, vec2};
use photocraft_algo::camera_raw::CameraRaw;
use photocraft_doc::DocId;
use photocraft_engine::jobs::{JobEvent, JobId, JobOutcome, OpenSource, Started};
use photocraft_io::raw::{Demosaic, RawPreview};
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::camera_raw_style::Acr;
use crate::camera_raw_ui::{RawOpen, raw_defaults};

/// A thumbnail's long edge (pixels): sharp at 2× in the strip's cells.
const THUMB_PX: u32 = 160;
/// The filmstrip's height under the preview (points).
pub(crate) const FILMSTRIP_H: f32 = 112.0;
/// Developed documents kept for photos shown before (besides the one shown): stepping through a
/// long strip would otherwise keep a full-size document per photo. Older ones close and develop
/// again when shown (their settings, curve and thumbnail stay).
const KEEP_DEVELOPED: usize = 3;
/// A thumbnail cell: the picture's box and the file name under it.
const CELL_W: f32 = 100.0;
const CELL_H: f32 = 104.0;
const THUMB_BOX: f32 = 76.0;

/// Where a filmstrip photo is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Stage {
    /// Not opened: only its thumbnail is shown.
    Waiting,
    /// Developing with the defaults, to be shown (a background open).
    Opening(JobId),
    /// Developed with the defaults: the document the dialog edits while it is shown.
    Ready(DocId),
    /// It could not be opened; why.
    Failed(String),
}

enum Thumb {
    /// The embedded preview is being read.
    Pending,
    /// No preview to show (a placeholder until the photo has been shown).
    None,
    Shown([usize; 2], TextureHandle),
}

/// One raw file in the filmstrip.
pub(crate) struct Photo {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) path: Option<String>,
    /// The file, when it came as bytes (no path to read it again from).
    pub(crate) bytes: Option<Arc<Vec<u8>>>,
    pub(crate) selected: bool,
    pub(crate) stage: Stage,
    /// Its own settings.
    pub(crate) params: CameraRaw,
    /// Its demosaic method (Open re-develops when it differs from `developed_with`).
    pub(crate) demosaic: Demosaic,
    pub(crate) developed_with: Demosaic,
    /// How the default develop rendered it (tone curve, noise), kept for its next showing.
    curve: Option<Arc<RawPreview>>,
    thumb: Thumb,
    /// The dialog's preview revision the thumbnail was last made from (a shown photo's thumbnail
    /// shows its edits).
    thumb_revision: Option<u64>,
}

impl Photo {
    fn new(id: u64, name: String, path: Option<String>, bytes: Option<Arc<Vec<u8>>>, method: Demosaic) -> Self {
        Photo {
            id,
            name,
            path,
            bytes,
            selected: false,
            stage: Stage::Waiting,
            params: raw_defaults(),
            demosaic: method,
            developed_with: method,
            curve: None,
            thumb: Thumb::Pending,
            thumb_revision: None,
        }
    }

    /// A raw already open (developed with the defaults), with `params`.
    fn adopted(id: u64, raw: RawOpen, params: CameraRaw) -> Self {
        Photo {
            stage: Stage::Ready(raw.document),
            params,
            demosaic: raw.demosaic,
            developed_with: raw.developed_with,
            ..Photo::new(id, raw.name, raw.path, raw.bytes, raw.developed_with)
        }
    }

    fn raw_open(&self, document: DocId) -> RawOpen {
        RawOpen {
            document,
            name: self.name.clone(),
            path: self.path.clone(),
            bytes: self.bytes.clone(),
            developed_with: self.developed_with,
            demosaic: self.demosaic,
        }
    }

    /// Settings other than Camera Raw's raw defaults (the strip marks it).
    fn edited(&self) -> bool {
        let mut p = self.params.clone();
        let blank = CameraRaw::default();
        (p.encoding_gamma, p.pixel_scale) = (blank.encoding_gamma, blank.pixel_scale);
        p != raw_defaults()
    }
}

/// How a thumbnail click selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pick {
    /// A plain click: that photo alone, shown.
    Only,
    /// Ctrl/Cmd-click: in or out of the selection (shown when it comes in).
    Toggle,
    /// Shift-click: the range from the last click, the clicked photo shown.
    Range,
}

/// What the strip, its keys or the control channel ask for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Pick(usize, Pick),
    SelectAll,
    /// The previous (−1) or next (+1) photo, alone.
    Step(isize),
    Remove(Vec<usize>),
    RemoveSelected,
}

/// The filmstrip of an open-time Camera Raw dialog showing several raws.
pub struct RawBatch {
    pub(crate) photos: Vec<Photo>,
    /// The photo shown (always selected).
    pub(crate) current: usize,
    /// Where a Shift-click range starts.
    anchor: usize,
    next_id: u64,
    /// The shown photo's settings when last shared with the other selected photos.
    shared: Option<(CameraRaw, Demosaic)>,
    /// The photo last made current, and the photos shown so far, the most recent last.
    shown: Option<u64>,
    recent: Vec<u64>,
    scroll_to_current: bool,
    #[cfg(not(target_arch = "wasm32"))]
    thumbs: (std::sync::mpsc::Sender<ThumbResult>, std::sync::mpsc::Receiver<ThumbResult>),
}

/// A photo's thumbnail read on the worker: its id and the picture (none without a preview).
#[cfg(not(target_arch = "wasm32"))]
type ThumbResult = (u64, Option<egui::ColorImage>);

impl RawBatch {
    fn new() -> Self {
        RawBatch {
            photos: Vec::new(),
            current: 0,
            anchor: 0,
            next_id: 1,
            shared: None,
            shown: None,
            recent: Vec::new(),
            scroll_to_current: true,
            #[cfg(not(target_arch = "wasm32"))]
            thumbs: std::sync::mpsc::channel(),
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn position(&self, id: u64) -> Option<usize> {
        self.photos.iter().position(|p| p.id == id)
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut Photo> {
        self.photos.iter_mut().find(|p| p.id == id)
    }

    /// The shown photo's file name.
    pub(crate) fn current_name(&self) -> &str {
        self.photos.get(self.current).map_or("", |p| p.name.as_str())
    }

    fn selected(&self) -> Vec<usize> {
        self.photos.iter().enumerate().filter(|(_, p)| p.selected).map(|(i, _)| i).collect()
    }

    /// A click on thumbnail `i`.
    pub(crate) fn pick(&mut self, i: usize, how: Pick) {
        if i >= self.photos.len() {
            return;
        }
        match how {
            Pick::Only => {
                for (k, p) in self.photos.iter_mut().enumerate() {
                    p.selected = k == i;
                }
                (self.current, self.anchor) = (i, i);
            }
            Pick::Toggle => {
                let in_selection = self.photos.get(i).is_some_and(|p| p.selected);
                if !in_selection {
                    if let Some(p) = self.photos.get_mut(i) {
                        p.selected = true;
                    }
                    (self.current, self.anchor) = (i, i);
                } else if self.selected().len() > 1 {
                    // At least one photo stays selected, the one shown among them.
                    if let Some(p) = self.photos.get_mut(i) {
                        p.selected = false;
                    }
                    if self.current == i {
                        self.current = self.nearest_selected(i);
                    }
                    self.anchor = self.current;
                }
            }
            Pick::Range => {
                let from = self.anchor.min(self.photos.len() - 1);
                let (a, b) = (from.min(i), from.max(i));
                for (k, p) in self.photos.iter_mut().enumerate() {
                    p.selected = (a..=b).contains(&k);
                }
                self.current = i;
            }
        }
        self.scroll_to_current = true;
    }

    /// The selected photo nearest after `i` (else before it), or `i`.
    fn nearest_selected(&self, i: usize) -> usize {
        (i + 1..self.photos.len()).chain((0..i).rev()).find(|k| self.photos.get(*k).is_some_and(|p| p.selected)).unwrap_or(i)
    }

    pub(crate) fn select_all(&mut self) {
        for p in &mut self.photos {
            p.selected = true;
        }
    }

    /// The previous or next photo, alone.
    pub(crate) fn step(&mut self, by: isize) {
        let last = self.photos.len().saturating_sub(1) as isize;
        let i = (self.current as isize).saturating_add(by).clamp(0, last);
        self.pick(i as usize, Pick::Only);
    }

    /// Takes photos `indices` out of the strip and returns them. The photo shown stays shown; when
    /// it went, the next selected photo from its place is (else the one before it, else the photo
    /// now at its place, selected).
    fn take_out(&mut self, indices: &[usize]) -> Vec<Photo> {
        let mut gone: Vec<usize> = indices.iter().copied().filter(|i| *i < self.photos.len()).collect();
        gone.sort_unstable();
        gone.dedup();
        let shown = self.photos.get(self.current).map(|p| p.id);
        let before = gone.iter().filter(|i| **i < self.current).count();
        let removed: Vec<Photo> = gone.iter().rev().map(|i| self.photos.remove(*i)).collect();
        let n = self.photos.len();
        if n == 0 {
            (self.current, self.anchor) = (0, 0);
            return removed;
        }
        self.current = match shown.and_then(|id| self.position(id)) {
            Some(i) => i,
            None => {
                let at = self.current.saturating_sub(before).min(n - 1);
                (at..n).chain((0..at).rev()).find(|k| self.photos.get(*k).is_some_and(|p| p.selected)).unwrap_or(at)
            }
        };
        if let Some(p) = self.photos.get_mut(self.current) {
            p.selected = true;
        }
        self.anchor = self.current;
        self.scroll_to_current = true;
        removed
    }

    /// Reads the embedded previews of photos `ids` for their thumbnails, off the UI thread (the
    /// web build reads one per frame in [`tick`]).
    fn thumbnails(&mut self, ids: &[u64]) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            type Work = (u64, Option<String>, Option<Arc<Vec<u8>>>);
            let work: Vec<Work> = self.photos.iter().filter(|p| ids.contains(&p.id)).map(|p| (p.id, p.path.clone(), p.bytes.clone())).collect();
            if work.is_empty() {
                return;
            }
            let tx = self.thumbs.0.clone();
            let spawned = std::thread::Builder::new().name("camera-raw-thumbnails".into()).spawn(move || {
                for (id, path, bytes) in work {
                    let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| thumbnail_of_file(path.as_deref(), bytes.as_deref()))).ok().flatten();
                    // The strip closed: stop.
                    if tx.send((id, img)).is_err() {
                        return;
                    }
                }
            });
            if spawned.is_err() {
                for p in self.photos.iter_mut().filter(|p| ids.contains(&p.id)) {
                    p.thumb = Thumb::None;
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = ids;
    }

    /// `ui.inspect.cameraRaw.filmstrip`.
    pub(crate) fn describe(&self, session: &photocraft_engine::Session) -> Value {
        let items: Vec<Value> = self
            .photos
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (state, error, document) = match &p.stage {
                    Stage::Waiting => ("waiting", None, None),
                    Stage::Opening(_) => ("opening", None, None),
                    Stage::Ready(doc) => ("ready", None, session.documents().iter().position(|st| st.doc.id == *doc)),
                    Stage::Failed(e) => ("failed", Some(e.as_str()), None),
                };
                let mut settings = crate::camera_raw_ui::non_defaults(&p.params);
                if let Value::Object(m) = &mut settings {
                    m.remove("encodingGamma");
                }
                json!({
                    "name": p.name,
                    "path": p.path,
                    "selected": p.selected,
                    "current": i == self.current,
                    "state": state,
                    "error": error,
                    "document": document,
                    "edited": p.edited(),
                    "settings": settings,
                    "demosaic": p.demosaic.id(),
                    "thumbnail": match &p.thumb { Thumb::Shown(size, _) => json!(size), _ => Value::Null },
                })
            })
            .collect();
        json!({"current": self.current, "selected": self.selected(), "items": items})
    }
}

/// `path`'s (or `bytes`') embedded preview as a thumbnail.
fn thumbnail_of_file(path: Option<&str>, bytes: Option<&Vec<u8>>) -> Option<egui::ColorImage> {
    #[cfg(not(target_arch = "wasm32"))]
    let read;
    let bytes: &[u8] = match (bytes, path) {
        (Some(b), _) => b,
        #[cfg(not(target_arch = "wasm32"))]
        (None, Some(p)) => {
            read = photocraft_format::read_file(std::path::Path::new(p)).ok()?;
            &read
        }
        _ => return None,
    };
    let (w, h, px) = photocraft_io::raw::preview_thumbnail(bytes, THUMB_PX)?;
    Some(egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px))
}

/// The photos Open is developing (one at a time), and the tab order they end up in.
#[derive(Default)]
pub struct RawQueue {
    waiting: std::collections::VecDeque<Queued>,
    /// The photos of Open in filmstrip order (path, name): once all have landed their tabs are
    /// put in that order, from the first one's place.
    order: Vec<(Option<String>, String)>,
}

impl RawQueue {
    /// No photo waits for its turn (the last ones may still be developing).
    pub(crate) fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

/// A selected photo waiting for its turn to develop after Open.
#[derive(Clone, Debug)]
pub struct Queued {
    /// Its document when it was shown (developed with the defaults); a placeholder id otherwise.
    raw: RawOpen,
    shown: bool,
    params: CameraRaw,
}

/// A raw file to add to the filmstrip.
pub(crate) enum Source {
    Path(String),
    /// A file read without a path (the web, the inbox).
    Bytes(String, Arc<Vec<u8>>),
}

impl Source {
    fn name(&self) -> String {
        match self {
            Source::Path(p) => crate::file_open::display_name(p),
            Source::Bytes(name, _) => name.clone(),
        }
    }
}

/// Is `name` a camera raw file by its extension (the formats Camera Raw develops)?
pub fn is_raw_name(name: &str) -> bool {
    std::path::Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(|e| photocraft_io::raw::EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Do `n` raws opened together go to the filmstrip? When Camera Raw opens raws (the preference),
/// and they are two or more or a filmstrip is already showing (they join it).
pub(crate) fn takes(app: &PhotocraftApp, n: usize) -> bool {
    n > 0 && app.session.prefs().raw_defaults.open_in_camera_raw && (n >= 2 || app.raw_batch.is_some())
}

/// Is the open-time dialog the filmstrip's (shown, or waiting for its photo)?
pub(crate) fn showing(app: &PhotocraftApp) -> bool {
    app.raw_batch.is_some() && app.camera_raw.as_ref().is_none_or(|d| d.batch_photo.is_some())
}

/// The raw files of `paths` (opened together) go to the filmstrip when it [`takes`] them; the
/// other paths are returned, to open as usual.
pub(crate) fn take_paths(app: &mut PhotocraftApp, paths: Vec<String>) -> Vec<String> {
    if !takes(app, paths.iter().filter(|p| is_raw_name(p)).count()) {
        return paths;
    }
    let (raws, rest): (Vec<String>, Vec<String>) = paths.into_iter().partition(|p| is_raw_name(p));
    add(app, raws.into_iter().map(Source::Path).collect());
    rest
}

/// [`take_paths`] for files read without a path: `(name, bytes)`.
pub(crate) fn take_contents(app: &mut PhotocraftApp, files: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    if !takes(app, files.iter().filter(|(name, _)| is_raw_name(name)).count()) {
        return files;
    }
    let (raws, rest): (Vec<_>, Vec<_>) = files.into_iter().partition(|(name, _)| is_raw_name(name));
    add(app, raws.into_iter().map(|(name, bytes)| Source::Bytes(name, Arc::new(bytes))).collect());
    rest
}

/// Starts the filmstrip when none is showing: a raw waiting for its dialog, or the one whose
/// dialog shows, becomes its first photo.
fn ensure_batch(app: &mut PhotocraftApp) {
    if app.raw_batch.is_some() {
        return;
    }
    let mut b = RawBatch::new();
    let id = b.next_id();
    let single = app.camera_raw.as_mut().filter(|d| d.opens_single_raw());
    if let Some(d) = single
        && let Some(raw) = d.raw_open.clone()
    {
        let demosaic = raw.demosaic;
        let mut p = Photo::adopted(id, raw, d.params.clone());
        p.curve = d.raw_curve.clone();
        p.selected = true;
        d.batch_photo = Some(id);
        b.shared = Some((d.params.clone(), demosaic));
        b.photos.push(p);
    } else if let Some(raw) = app.pending_raw_open.take() {
        let mut p = Photo::adopted(id, raw, raw_defaults());
        p.selected = true;
        b.photos.push(p);
    }
    let first: Vec<u64> = b.photos.iter().map(|p| p.id).collect();
    b.thumbnails(&first);
    app.raw_batch = Some(b);
}

/// Adds raw files to the filmstrip (in name order, each file once), starting it with the first of
/// them shown when none is showing.
pub(crate) fn add(app: &mut PhotocraftApp, mut sources: Vec<Source>) {
    if sources.is_empty() {
        return;
    }
    sources.sort_by(|a, b| crate::file_open::natural_cmp(&a.name(), &b.name()));
    ensure_batch(app);
    let method = app.session.prefs().raw_defaults.demosaic.method();
    let Some(b) = app.raw_batch.as_mut() else { return };
    let mut added = Vec::new();
    for s in sources {
        let name = s.name();
        let (path, bytes) = match s {
            Source::Path(p) => (Some(p), None),
            Source::Bytes(_, bytes) => (None, Some(bytes)),
        };
        if path.is_some() && b.photos.iter().any(|p| p.path == path) {
            continue;
        }
        let id = b.next_id();
        b.photos.push(Photo::new(id, name, path, bytes, method));
        added.push(id);
    }
    if !b.photos.iter().any(|p| p.selected) {
        b.current = 0;
        if let Some(p) = b.photos.first_mut() {
            p.selected = true;
        }
    }
    b.thumbnails(&added);
    // The shown photo starts developing now (in the background in the desktop app).
    let current = b.current;
    start_open(app, current);
}

/// A raw that opened on its own while another waits for (or shows) the open-time dialog, or while
/// a filmstrip shows: it joins them in one filmstrip. Returns `raw` when it opens on its own.
pub(crate) fn adopt(app: &mut PhotocraftApp, raw: RawOpen) -> Option<RawOpen> {
    let single = app.camera_raw.as_ref().is_some_and(|d| d.opens_single_raw());
    if app.raw_batch.is_none() && app.pending_raw_open.is_none() && !single {
        return Some(raw);
    }
    ensure_batch(app);
    let Some(b) = app.raw_batch.as_mut() else { return Some(raw) };
    if b.photos.iter().any(|p| p.stage == Stage::Ready(raw.document)) {
        return None;
    }
    let id = b.next_id();
    let mut p = Photo::adopted(id, raw, raw_defaults());
    if b.photos.is_empty() {
        p.selected = true;
    }
    b.photos.push(p);
    b.thumbnails(&[id]);
    None
}

/// Starts developing photo `index` with the defaults, to show it (when it is waiting). Not while
/// another Camera Raw dialog (the filter's) is up: opening a document would change what it edits.
fn start_open(app: &mut PhotocraftApp, index: usize) {
    if app.camera_raw.as_ref().is_some_and(|d| d.batch_photo.is_none()) {
        return;
    }
    let Some(p) = app.raw_batch.as_ref().and_then(|b| b.photos.get(index)).filter(|p| p.stage == Stage::Waiting) else { return };
    let (id, name, path, bytes) = (p.id, p.name.clone(), p.path.clone(), p.bytes.clone());
    if let Some(b) = app.raw_batch.as_mut().filter(|b| b.current == index) {
        b.shown = Some(id);
    }
    if app.background_jobs {
        let source = match (bytes, path) {
            (Some(b), _) => OpenSource::Bytes(b),
            (None, Some(p)) => OpenSource::Path(p),
            (None, None) => return failed(app, id, "nothing to read"),
        };
        match app.session.start_open(&name, source) {
            Ok(Started::Job(job)) => {
                if let Some(p) = app.raw_batch.as_mut().and_then(|b| b.find_mut(id)) {
                    p.stage = Stage::Opening(job);
                }
            }
            Ok(Started::Done(v)) => opened(app, id, &v),
            Err(e) => failed(app, id, &e.to_string()),
        }
        return;
    }
    let result = (|| -> Result<Value, String> {
        let bytes = match (bytes, path) {
            (Some(b), _) => b,
            #[cfg(not(target_arch = "wasm32"))]
            (None, Some(p)) => Arc::new(photocraft_format::read_file(std::path::Path::new(&p)).map_err(|e| format!("{p}: {e}"))?),
            _ => return Err("the file can't be read here".into()),
        };
        let import = app.services.import.as_ref().ok_or("no importer configured")?;
        let opts = photocraft_engine::jobs::import_options(app.session.prefs());
        let (doc, warnings) = import(&name, &bytes, &opts)?;
        let (index, _) = app.session.open_document(doc, None);
        Ok(json!({"document": index, "warnings": warnings}))
    })();
    match result {
        Ok(v) => opened(app, id, &v),
        Err(e) => failed(app, id, &e),
    }
}

/// Photo `id` opened (`{document, warnings}`): it can be shown. One that did not develop from its
/// sensor data (the camera's preview stands in) is an ordinary document: it leaves the strip and
/// stays open like any file.
fn opened(app: &mut PhotocraftApp, id: u64, v: &Value) {
    let index = v.get("document").and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok());
    let Some(doc) = index.and_then(|i| app.session.documents().get(i)).map(|st| st.doc.id) else {
        return failed(app, id, "the open made no document");
    };
    let warnings: Vec<String> =
        v.get("warnings").and_then(Value::as_array).map(|a| a.iter().filter_map(|w| w.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let developed = warnings.iter().any(|w| w.contains(photocraft_io::raw::DEVELOPED_NOTE));
    let method = app.session.prefs().raw_defaults.demosaic.method();
    let Some((at, name, path)) = app.raw_batch.as_ref().and_then(|b| b.position(id).and_then(|i| b.photos.get(i).map(|p| (i, p.name.clone(), p.path.clone()))))
    else {
        // Taken out (or the strip closed) meanwhile: not opened after all.
        close_document(app, doc);
        return;
    };
    // File › Save goes back to the file, as for any open.
    if let Some(i) = document_index(app, doc) {
        app.session.set_active(i);
        if let Some(st) = app.session.active_mut() {
            st.path = path.clone();
        }
    }
    app.sync_views();
    let opened_name = crate::i18n::fmt(tl!("Opened {name}"), &[("name", &name)]);
    if developed {
        if let Some(p) = app.raw_batch.as_mut().and_then(|b| b.photos.get_mut(at)) {
            p.stage = Stage::Ready(doc);
            p.developed_with = method;
        }
        crate::notices::io_warnings(app, &opened_name, &crate::camera_raw_ui::without_develop_note(&warnings));
        return;
    }
    if let Some(b) = app.raw_batch.as_mut() {
        b.take_out(&[at]);
        if b.photos.is_empty() {
            app.raw_batch = None;
        }
    }
    if let Some(p) = &path {
        app.push_recent(p);
    }
    crate::notices::io_warnings(app, &opened_name, &warnings);
    // Script events bound to "Open Document".
    photocraft_engine::automate_cmds::document_opened(&mut app.session);
    app.sync_views();
}

fn failed(app: &mut PhotocraftApp, id: u64, err: &str) {
    let Some(p) = app.raw_batch.as_mut().and_then(|b| b.find_mut(id)) else { return };
    p.stage = Stage::Failed(err.to_string());
    let name = p.name.clone();
    app.open_failed(&name, err);
}

/// A finished background open of a filmstrip photo: `true` when the event was one (handled here).
pub(crate) fn on_job_event(app: &mut PhotocraftApp, e: &JobEvent) -> bool {
    let Some(id) = app.raw_batch.as_ref().and_then(|b| b.photos.iter().find(|p| p.stage == Stage::Opening(e.id))).map(|p| p.id) else {
        return false;
    };
    match &e.outcome {
        JobOutcome::Done(v) => opened(app, id, v),
        JobOutcome::Failed(err) => failed(app, id, err),
        JobOutcome::Cancelled => {
            if let Some(p) = app.raw_batch.as_mut().and_then(|b| b.find_mut(id)) {
                p.stage = Stage::Waiting;
            }
        }
    }
    true
}

fn document_index(app: &PhotocraftApp, id: DocId) -> Option<usize> {
    app.session.documents().iter().position(|st| st.doc.id == id)
}

fn close_document(app: &mut PhotocraftApp, id: DocId) {
    if let Some(i) = document_index(app, id) {
        app.session.close(i);
        app.sync_views();
    }
}

/// Every frame (from `camera_raw_ui::show`): photos still to develop after Open, thumbnails that
/// arrived, the shown photo's dialog once it is developed.
pub(crate) fn tick(app: &mut PhotocraftApp, ctx: &egui::Context) {
    pump(app);
    let Some(b) = app.raw_batch.as_mut() else { return };
    #[cfg(not(target_arch = "wasm32"))]
    while let Ok((id, img)) = b.thumbs.1.try_recv() {
        if let Some(p) = b.find_mut(id).filter(|p| matches!(p.thumb, Thumb::Pending)) {
            p.thumb = shown_thumb(ctx, id, img);
        }
    }
    #[cfg(target_arch = "wasm32")]
    if let Some(p) = b.photos.iter_mut().find(|p| matches!(p.thumb, Thumb::Pending)) {
        let img = thumbnail_of_file(None, p.bytes.as_deref());
        p.thumb = shown_thumb(ctx, p.id, img);
    }
    if b.photos.iter().any(|p| matches!(p.thumb, Thumb::Pending)) {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    ensure_dialog(app, ctx);
    refresh_thumbnail(app, ctx);
}

fn shown_thumb(ctx: &egui::Context, id: u64, img: Option<egui::ColorImage>) -> Thumb {
    match img {
        Some(img) => {
            let size = img.size;
            Thumb::Shown(size, ctx.load_texture(format!("camera-raw-thumb-{id}"), img, egui::TextureOptions::LINEAR))
        }
        None => Thumb::None,
    }
}

/// The shown photo's dialog: built once the photo is developed (started when it waits); a dialog
/// left from the photo shown before keeps its settings in that photo and closes.
fn ensure_dialog(app: &mut PhotocraftApp, ctx: &egui::Context) {
    // A photo that opens as an ordinary document leaves the strip and another is shown: again.
    let tries = app.raw_batch.as_ref().map_or(0, |b| b.photos.len());
    for _ in 0..=tries {
        if !show_current(app, ctx) {
            break;
        }
    }
}

/// [`ensure_dialog`] once: `true` when the shown photo changed meanwhile.
fn show_current(app: &mut PhotocraftApp, ctx: &egui::Context) -> bool {
    let Some(id) = app.raw_batch.as_ref().and_then(|b| b.photos.get(b.current)).map(|p| p.id) else { return false };
    if let Some(d) = app.camera_raw.as_ref() {
        match d.batch_photo {
            // Another Camera Raw dialog is up: the filmstrip waits for it.
            None => return false,
            Some(shown) if shown == id => {
                // The canvas behind shows (and the dialog previews through) that photo's document.
                if let Some(doc) = d.raw_open.as_ref().map(|r| r.document)
                    && app.session.active().is_some_and(|st| st.doc.id != doc)
                    && let Some(i) = document_index(app, doc)
                {
                    app.session.set_active(i);
                }
                return false;
            }
            Some(_) => {
                share(app);
                refresh_thumbnail(app, ctx);
                app.camera_raw = None;
            }
        }
    }
    leave_previous(app, id);
    let Some(index) = app.raw_batch.as_ref().map(|b| b.current) else { return false };
    start_open(app, index);
    let Some(p) = app.raw_batch.as_ref().and_then(|b| b.photos.get(index)).filter(|p| p.id == id) else {
        return app.raw_batch.as_ref().is_some_and(|b| !b.photos.is_empty());
    };
    let Stage::Ready(doc) = p.stage else { return false };
    let (raw, params, curve) = (p.raw_open(doc), p.params.clone(), p.curve.clone());
    if document_index(app, doc).is_none() {
        // Its document was closed behind the dialog: develop it again.
        if let Some(p) = app.raw_batch.as_mut().and_then(|b| b.find_mut(id)) {
            p.stage = Stage::Waiting;
        }
        return true;
    }
    let demosaic = raw.demosaic;
    match crate::camera_raw_ui::open_raw_with(app, ctx, raw, Some(params), curve) {
        Ok(curve) => {
            let shown = app.camera_raw.as_mut().map(|d| {
                d.batch_photo = Some(id);
                d.params.clone()
            });
            if let Some(b) = app.raw_batch.as_mut() {
                b.shared = shown.map(|params| (params, demosaic));
                if let Some(p) = b.find_mut(id) {
                    p.curve = curve;
                }
                b.recent.retain(|r| *r != id);
                b.recent.push(id);
            }
            unload_old(app);
        }
        Err(e) => failed(app, id, &e),
    }
    false
}

/// Photo `id` is becoming current: one still developing to be shown stops (one develop at a time
/// while stepping through the strip; it develops again when shown).
fn leave_previous(app: &mut PhotocraftApp, id: u64) {
    let Some(b) = app.raw_batch.as_mut() else { return };
    let Some(previous) = b.shown.replace(id).filter(|p| *p != id) else { return };
    if let Some(p) = b.find_mut(previous)
        && let Stage::Opening(job) = p.stage
    {
        p.stage = Stage::Waiting;
        app.session.cancel_job(job);
    }
}

/// Closes the documents of photos shown longest ago beyond [`KEEP_DEVELOPED`] (never the shown
/// one, nor one whose file can't be read again): they wait, and develop again when shown.
fn unload_old(app: &mut PhotocraftApp) {
    let Some(b) = app.raw_batch.as_mut() else { return };
    let current = b.photos.get(b.current).map(|p| p.id);
    let age = |id: u64| b.recent.iter().position(|r| *r == id);
    let mut kept: Vec<(Option<usize>, u64, DocId)> = b
        .photos
        .iter()
        .filter(|p| Some(p.id) != current && (p.bytes.is_some() || p.path.is_some()))
        .filter_map(|p| match p.stage {
            Stage::Ready(doc) => Some((age(p.id), p.id, doc)),
            _ => None,
        })
        .collect();
    if kept.len() <= KEEP_DEVELOPED {
        return;
    }
    // Never shown (joined already open) first, then the oldest shown.
    kept.sort_by_key(|(age, _, _)| *age);
    let unload: Vec<(u64, DocId)> = kept.iter().take(kept.len() - KEEP_DEVELOPED).map(|(_, id, doc)| (*id, *doc)).collect();
    for (id, doc) in unload {
        if let Some(p) = app.raw_batch.as_mut().and_then(|b| b.find_mut(id)) {
            p.stage = Stage::Waiting;
        }
        close_document(app, doc);
    }
}

/// The shown photo's thumbnail from the dialog's developed preview (its edits), once the preview
/// changed and no drag is going on.
fn refresh_thumbnail(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let (Some(b), Some(d)) = (app.raw_batch.as_mut(), app.camera_raw.as_ref()) else { return };
    let Some(p) = d.batch_photo.and_then(|id| b.photos.iter_mut().find(|p| p.id == id)) else { return };
    if p.thumb_revision == Some(d.preview_revision) || d.dirty || ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    if let Some(img) = d.thumbnail(THUMB_PX as usize) {
        p.thumb = shown_thumb(ctx, p.id, Some(img));
    }
    p.thumb_revision = Some(d.preview_revision);
}

/// The settings of `after` that differ from `before` (not the per-image pixel scale and encoding).
fn changed_settings(before: &CameraRaw, after: &CameraRaw) -> Map<String, Value> {
    let (Ok(Value::Object(old)), Ok(Value::Object(new))) = (serde_json::to_value(before), serde_json::to_value(after)) else { return Map::new() };
    new.into_iter().filter(|(k, v)| !matches!(k.as_str(), "pixelScale" | "encodingGamma") && old.get(k) != Some(v)).collect()
}

/// `p` with `changed` settings in place of its own.
fn with_settings(p: &CameraRaw, changed: &Map<String, Value>) -> CameraRaw {
    let Ok(Value::Object(mut m)) = serde_json::to_value(p) else { return p.clone() };
    m.extend(changed.iter().map(|(k, v)| (k.clone(), v.clone())));
    serde_json::from_value(Value::Object(m)).unwrap_or_else(|_| p.clone())
}

/// Keeps the shown photo's settings in its photo; with several selected, a setting changed since
/// the last call gets that new value in every selected photo (their other settings stay).
pub(crate) fn share(app: &mut PhotocraftApp) {
    let (Some(b), Some(d)) = (app.raw_batch.as_mut(), app.camera_raw.as_ref()) else { return };
    let Some(id) = d.batch_photo else { return };
    let demosaic = d.raw_open.as_ref().map(|r| r.demosaic);
    let Some((before, before_demosaic)) = b.shared.take() else {
        b.shared = Some((d.params.clone(), demosaic.unwrap_or_default()));
        return;
    };
    let new_demosaic = demosaic.unwrap_or(before_demosaic);
    if before != d.params || new_demosaic != before_demosaic {
        let changed = changed_settings(&before, &d.params);
        for p in b.photos.iter_mut().filter(|p| p.selected && p.id != id) {
            if !changed.is_empty() {
                p.params = with_settings(&p.params, &changed);
            }
            if new_demosaic != before_demosaic {
                p.demosaic = new_demosaic;
            }
        }
    }
    if let Some(p) = b.find_mut(id) {
        p.params = d.params.clone();
        p.demosaic = new_demosaic;
    }
    b.shared = Some((d.params.clone(), new_demosaic));
}

/// Does what the strip asked for, then shows the photo that is current.
pub(crate) fn act(app: &mut PhotocraftApp, ctx: &egui::Context, action: Action) {
    share(app);
    let Some(b) = app.raw_batch.as_mut() else { return };
    match action {
        Action::Pick(i, how) => b.pick(i, how),
        Action::SelectAll => b.select_all(),
        Action::Step(by) => b.step(by),
        Action::Remove(list) => remove(app, &list),
        Action::RemoveSelected => {
            let list = b.selected();
            remove(app, &list);
        }
    }
    ensure_dialog(app, ctx);
}

/// Takes photos out of the strip: nothing of them is opened (a shown one's document closes). The
/// files are not touched. With none left, Camera Raw closes.
fn remove(app: &mut PhotocraftApp, list: &[usize]) {
    let shown = app.camera_raw.as_ref().and_then(|d| d.batch_photo);
    let Some(b) = app.raw_batch.as_mut() else { return };
    let removed = b.take_out(list);
    let empty = b.photos.is_empty();
    if shown.is_some_and(|id| removed.iter().any(|p| p.id == id)) {
        app.camera_raw = None;
    }
    release(app, &removed);
    if empty {
        app.raw_batch = None;
        status_cancelled(app, &removed);
    }
}

/// Closes the documents photos were shown with and stops their opens.
fn release(app: &mut PhotocraftApp, photos: &[Photo]) {
    for p in photos {
        match p.stage {
            Stage::Ready(doc) => {
                if let Some(i) = document_index(app, doc) {
                    app.session.close(i);
                }
            }
            Stage::Opening(job) => {
                app.session.cancel_job(job);
            }
            Stage::Waiting | Stage::Failed(_) => {}
        }
    }
    app.sync_views();
}

fn status_cancelled(app: &mut PhotocraftApp, photos: &[Photo]) {
    let mut names: Vec<&str> = photos.iter().take(3).map(|p| p.name.as_str()).collect();
    if photos.len() > 3 {
        names.push("…");
    }
    app.ui.status = crate::i18n::fmt(tl!("Cancelled opening {name}"), &[("name", &names.join(", "))]);
    app.ui.status_error = false;
}

/// Cancel: none of the photos opens; documents shown in the dialog close.
pub(crate) fn cancel(app: &mut PhotocraftApp) {
    let Some(b) = app.raw_batch.take() else { return };
    if app.camera_raw.as_ref().is_some_and(|d| d.batch_photo.is_some()) {
        app.camera_raw = None;
    }
    release(app, &b.photos);
    status_cancelled(app, &b.photos);
}

/// Open: the selected photos, each developed with its own settings; the others are not opened.
/// They develop one at a time ([`pump`]); the reply lists those done (`photos`) and whether more
/// are on their way (`pending`).
pub(crate) fn commit(app: &mut PhotocraftApp) -> Result<Value, String> {
    share(app);
    let b = app.raw_batch.take().ok_or("no filmstrip is showing")?;
    if app.camera_raw.as_ref().is_some_and(|d| d.batch_photo.is_some()) {
        app.camera_raw = None;
    }
    let mut queue = Vec::new();
    let mut unopened = Vec::new();
    for p in b.photos {
        if !p.selected {
            unopened.push(p);
            continue;
        }
        let document = match p.stage {
            Stage::Ready(doc) => Some(doc),
            Stage::Opening(job) => {
                // Developed with its settings instead.
                app.session.cancel_job(job);
                None
            }
            Stage::Waiting => None,
            // Already reported.
            Stage::Failed(_) => continue,
        };
        app.raw_queue.order.push((p.path.clone(), p.name.clone()));
        queue.push(Queued { raw: p.raw_open(document.unwrap_or_else(DocId::fresh)), shown: document.is_some(), params: p.params });
    }
    release(app, &unopened);
    let queued = queue.len();
    app.raw_queue.waiting.extend(queue);
    let mut photos = pump(app);
    // Indexes after the tabs were put in filmstrip order.
    for v in photos.iter_mut().filter(|v| v.get("document").is_some()) {
        let path = v.get("path").and_then(Value::as_str).map(str::to_string);
        let name = v.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        v["document"] = json!(app.session.documents().iter().position(|st| holds(st, &path, &name)));
    }
    Ok(json!({"photos": photos, "queued": queued, "pending": !app.raw_queue.is_empty() || !app.raw_redevelop.is_empty()}))
}

/// Develops the photos queued by Open, one at a time: the next starts once the open-time develops
/// and Camera Raw steps of the one before have landed (several full-size develops at once would
/// multiply the memory). Returns what each one started now replied.
pub(crate) fn pump(app: &mut PhotocraftApp) -> Vec<Value> {
    let mut out = Vec::new();
    while app.raw_redevelop.is_empty() {
        let Some(q) = app.raw_queue.waiting.pop_front() else { break };
        let shown = q.shown.then(|| document_index(app, q.raw.document)).flatten();
        let r = match shown {
            Some(i) => {
                app.session.set_active(i);
                if let Some(p) = &q.raw.path {
                    app.push_recent(p);
                }
                // Script events bound to "Open Document".
                photocraft_engine::automate_cmds::document_opened(&mut app.session);
                crate::camera_raw_ui::commit_open_raw(app, &q.params, &q.raw)
            }
            None => crate::camera_raw_ui::develop_raw(app, &q.params, &q.raw),
        };
        match r {
            Ok(mut v) => {
                v["name"] = json!(q.raw.name);
                v["path"] = json!(q.raw.path);
                out.push(v);
            }
            Err(e) => {
                app.open_failed(&q.raw.name, &e);
                out.push(json!({"name": q.raw.name, "error": e}));
            }
        }
    }
    if app.raw_queue.waiting.is_empty() && app.raw_redevelop.is_empty() && !app.raw_queue.order.is_empty() {
        arrange(app);
    }
    out
}

/// Is `st` the document of the photo read from `path` (or, without one, named `name`)?
fn holds(st: &photocraft_engine::DocState, path: &Option<String>, name: &str) -> bool {
    match path {
        Some(p) => st.path.as_deref() == Some(p.as_str()),
        None => st.path.is_none() && st.doc.name == name,
    }
}

/// Puts the tabs of the photos Open developed in filmstrip order (shown photos were developed in
/// the order they were shown, the others as their turn came), from the first one's place.
fn arrange(app: &mut PhotocraftApp) {
    let order = std::mem::take(&mut app.raw_queue.order);
    let mut placed: Vec<usize> = Vec::new();
    for (path, name) in &order {
        let found = app.session.documents().iter().enumerate().find(|(i, st)| !placed.contains(i) && holds(st, path, name));
        if let Some((i, _)) = found {
            placed.push(i);
        }
    }
    let Some(start) = placed.iter().min().copied() else { return };
    let ids: Vec<DocId> = placed.iter().filter_map(|i| app.session.documents().get(*i).map(|st| st.doc.id)).collect();
    for (k, id) in ids.iter().enumerate() {
        if let Some(from) = document_index(app, *id).filter(|from| *from != start + k) {
            app.session.move_document(from, start + k);
        }
    }
    app.sync_views();
}

/// The footer's Open: with several photos selected it says how many open (not those that failed).
pub(crate) fn open_label(b: &RawBatch) -> String {
    let n = b.photos.iter().filter(|p| p.selected && !matches!(p.stage, Stage::Failed(_))).count();
    if n > 1 { crate::i18n::fmt(tl!("Open ({n})"), &[("n", &n.to_string())]) } else { tl!("Open").to_owned() }
}

/// The filmstrip's keys (not while a text field or another control has the keyboard):
/// Ctrl/Cmd+A selects all, ← / → step, Delete takes the selection out.
pub(crate) fn shortcuts(app: &PhotocraftApp, ctx: &egui::Context) -> Option<Action> {
    let b = app.raw_batch.as_ref()?;
    if b.photos.is_empty() || ctx.text_edit_focused() {
        return None;
    }
    use egui::{Key, Modifiers};
    let free = ctx.memory(|m| m.focused().is_none());
    ctx.input_mut(|i| {
        if i.consume_key(Modifiers::COMMAND, Key::A) {
            Some(Action::SelectAll)
        } else if free && i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
            Some(Action::Step(-1))
        } else if free && i.consume_key(Modifiers::NONE, Key::ArrowRight) {
            Some(Action::Step(1))
        } else if free && (i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace)) {
            Some(Action::RemoveSelected)
        } else {
            None
        }
    })
}

/// `params.ui.filmstrip` of a control request, validated against the strip.
pub(crate) fn parse(b: &RawBatch, v: &Value) -> Result<Action, String> {
    let fields = v.as_object().ok_or("Camera Raw filmstrip must be an object")?;
    let mut entries = fields.iter();
    let (Some((key, value)), None) = (entries.next(), entries.next()) else {
        return Err("Camera Raw filmstrip takes one of select, toggle, extend, selectAll, next, previous, remove".into());
    };
    let n = b.photos.len();
    let index = |v: &Value| {
        v.as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| *i < n)
            .ok_or_else(|| format!("Camera Raw filmstrip indexes run from 0 to {}", n.saturating_sub(1)))
    };
    match key.as_str() {
        "select" => Ok(Action::Pick(index(value)?, Pick::Only)),
        "toggle" => Ok(Action::Pick(index(value)?, Pick::Toggle)),
        "extend" => Ok(Action::Pick(index(value)?, Pick::Range)),
        "selectAll" | "next" | "previous" if value != &Value::Bool(true) => Err(format!("Camera Raw filmstrip.{key} must be true")),
        "selectAll" => Ok(Action::SelectAll),
        "next" => Ok(Action::Step(1)),
        "previous" => Ok(Action::Step(-1)),
        "remove" => match value {
            Value::String(s) if s == "selected" => Ok(Action::RemoveSelected),
            Value::Array(list) if !list.is_empty() => list.iter().map(index).collect::<Result<Vec<_>, _>>().map(Action::Remove),
            Value::Array(_) => Err("Camera Raw filmstrip.remove needs an index, a list of them, or \"selected\"".into()),
            one => index(one).map(|i| Action::Remove(vec![i])),
        },
        _ => Err(format!("unknown Camera Raw filmstrip property {key} (select, toggle, extend, selectAll, next, previous, remove)")),
    }
}

/// The filmstrip in `rect` (under the preview): a thumbnail per photo with its file name, the
/// shown one highlighted and the selected ones outlined; a × on the hovered one. Returns what a
/// click asked for.
pub(crate) fn filmstrip(ui: &mut egui::Ui, b: &mut RawBatch, rect: ERect, c: &Acr) -> Option<Action> {
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, c.base);
    painter.rect_filled(ERect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 1.0)), 0.0, c.rule);
    painter.rect_filled(ERect::from_min_max(pos2(rect.left(), rect.bottom() - 1.0), rect.max), 0.0, c.rule);
    let mut action = None;
    let inner = ERect::from_min_max(pos2(rect.left() + 6.0, rect.top() + 4.0), pos2(rect.right() - 6.0, rect.bottom()));
    let mut strip = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    strip.set_clip_rect(rect.intersect(ui.clip_rect()));
    let mods = strip.input(|i| i.modifiers);
    let scroll = std::mem::take(&mut b.scroll_to_current);
    let current = b.current;
    let warning = crate::theme::Tokens::get(ui.ctx()).warning;
    egui::ScrollArea::horizontal().id_salt("camera-raw-filmstrip").auto_shrink([false, false]).show(&mut strip, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (i, p) in b.photos.iter().enumerate() {
                let (cell, resp) = ui.allocate_exact_size(vec2(CELL_W, CELL_H), Sense::click());
                resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, p.selected, &p.name));
                let resp = match &p.stage {
                    Stage::Failed(e) => resp.on_hover_text(format!("{}\n{e}", p.name)),
                    _ => resp.on_hover_text(&p.name),
                };
                paint_cell(ui, p, cell, i == current, warning, c);
                if scroll && i == current {
                    ui.scroll_to_rect(cell, None);
                }
                if resp.clicked() {
                    let how = if mods.command {
                        Pick::Toggle
                    } else if mods.shift {
                        Pick::Range
                    } else {
                        Pick::Only
                    };
                    action = Some(Action::Pick(i, how));
                }
                resp.context_menu(|ui| {
                    if ui.add(egui::Button::new(tl!("Remove from Filmstrip")).shortcut_text("Delete")).clicked() {
                        action = Some(if p.selected { Action::RemoveSelected } else { Action::Remove(vec![i]) });
                        ui.close();
                    }
                    if ui.add(egui::Button::new(tl!("Select All")).shortcut_text("Ctrl+A")).clicked() {
                        action = Some(Action::SelectAll);
                        ui.close();
                    }
                });
                if ui.rect_contains_pointer(cell) {
                    let x = ERect::from_min_size(pos2(cell.right() - 21.0, cell.top() + 3.0), vec2(18.0, 18.0));
                    let remove = ui
                        .interact(x, ui.id().with(("camera-raw-filmstrip-remove", p.id)), Sense::click())
                        .on_hover_text(tl!("Remove from Filmstrip: the photo is not opened; the file stays on disk"));
                    remove.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tl!("Remove from Filmstrip")));
                    ui.painter().circle_filled(x.center(), 9.0, c.well.gamma_multiply(0.85));
                    crate::icons::paint(ui, x, "x", 12.0, if remove.hovered() { c.text } else { c.label });
                    if remove.clicked() {
                        action = Some(Action::Remove(vec![i]));
                    }
                }
            }
        });
    });
    action
}

/// One thumbnail cell.
fn paint_cell(ui: &egui::Ui, p: &Photo, cell: ERect, current: bool, warning: Color32, c: &Acr) {
    let painter = ui.painter();
    if current {
        painter.rect_filled(cell, 3.0, c.hover);
    }
    let frame = ERect::from_center_size(pos2(cell.center().x, cell.top() + 4.0 + THUMB_BOX / 2.0), vec2(THUMB_BOX, THUMB_BOX));
    let picture = match &p.thumb {
        Thumb::Shown([w, h], tex) => {
            let s = (THUMB_BOX / (*w).max(1) as f32).min(THUMB_BOX / (*h).max(1) as f32);
            let r = ERect::from_center_size(frame.center(), vec2(*w as f32 * s, *h as f32 * s));
            painter.image(tex.id(), r, ERect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            r
        }
        Thumb::Pending | Thumb::None => {
            let r = ERect::from_center_size(frame.center(), vec2(THUMB_BOX, THUMB_BOX * 2.0 / 3.0));
            painter.rect_filled(r, 2.0, c.group);
            if matches!(p.thumb, Thumb::None) && !matches!(p.stage, Stage::Failed(_)) {
                crate::icons::paint(ui, r, "image", 22.0, c.faint);
            }
            r
        }
    };
    if p.selected {
        painter.rect_stroke(picture.expand(2.0), 2.0, Stroke::new(if current { 2.0 } else { 1.5 }, c.text), egui::StrokeKind::Outside);
    }
    match &p.stage {
        Stage::Opening(_) => egui::Spinner::new().size(18.0).color(c.text).paint_at(ui, ERect::from_center_size(picture.center(), vec2(18.0, 18.0))),
        Stage::Failed(_) => crate::icons::paint(ui, picture, "triangle-alert", 20.0, warning),
        Stage::Waiting | Stage::Ready(_) => {}
    }
    if p.edited() {
        // Camera Raw marks a photo with settings of its own.
        let at = pos2(picture.right() - 9.0, picture.bottom() - 9.0);
        painter.circle_filled(at, 8.0, c.well.gamma_multiply(0.85));
        crate::icons::paint(ui, ERect::from_center_size(at, vec2(12.0, 12.0)), "sliders-horizontal", 11.0, c.text);
    }
    let mut job =
        egui::text::LayoutJob::single_section(p.name.clone(), egui::TextFormat::simple(FontId::proportional(10.5), if p.selected { c.text } else { c.label }));
    job.wrap = egui::text::TextWrapping { max_width: CELL_W - 8.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let name = painter.layout_job(job);
    painter.galley(pos2(cell.center().x - name.size().x / 2.0, frame.bottom() + 5.0), name, c.text);
}

/// The preview well while the shown photo is still developing: its thumbnail, dimmed, with a
/// spinner, or why it could not open.
pub(crate) fn waiting_preview(ui: &mut egui::Ui, b: &RawBatch, view: ERect, c: &Acr) {
    let Some(p) = b.photos.get(b.current) else { return };
    let painter = ui.painter();
    if let Thumb::Shown([w, h], tex) = &p.thumb {
        let s = (view.width() / (*w).max(1) as f32).min(view.height() / (*h).max(1) as f32);
        let r = ERect::from_center_size(view.center(), vec2(*w as f32 * s, *h as f32 * s));
        painter.image(tex.id(), r, ERect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::from_white_alpha(110));
    }
    match &p.stage {
        Stage::Failed(e) => {
            let warning = crate::theme::Tokens::get(ui.ctx()).warning;
            crate::icons::paint(ui, ERect::from_center_size(view.center() - vec2(0.0, 18.0), vec2(28.0, 28.0)), "triangle-alert", 26.0, warning);
            let g = painter.layout(e.clone(), FontId::proportional(12.5), c.text, (view.width() - 40.0).max(40.0));
            painter.galley(pos2(view.center().x - g.size().x / 2.0, view.center().y + 6.0), g, c.text);
        }
        _ => {
            egui::Spinner::new().size(28.0).color(c.text).paint_at(ui, ERect::from_center_size(view.center() - vec2(0.0, 18.0), vec2(28.0, 28.0)));
            let text = crate::i18n::fmt(tl!("Opening {name}…"), &[("name", &p.name)]);
            painter.text(view.center() + vec2(0.0, 16.0), egui::Align2::CENTER_TOP, text, FontId::proportional(12.5), c.text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_raw_ui::RAW_COLOR_NOISE;

    fn strip(n: usize) -> RawBatch {
        let mut b = RawBatch::new();
        for i in 0..n {
            let id = b.next_id();
            b.photos.push(Photo::new(id, format!("IMG_{i}.dng"), None, None, Demosaic::default()));
        }
        b.pick(0, Pick::Only);
        b
    }

    #[test]
    fn clicks_select_like_camera_raw() {
        let mut b = strip(5);
        assert_eq!((b.selected(), b.current), (vec![0], 0));
        // Ctrl-click adds and shows; Ctrl-click on the shown one takes it out and shows another.
        b.pick(3, Pick::Toggle);
        assert_eq!((b.selected(), b.current), (vec![0, 3], 3));
        b.pick(3, Pick::Toggle);
        assert_eq!((b.selected(), b.current), (vec![0], 0));
        // The last selected photo stays selected.
        b.pick(0, Pick::Toggle);
        assert_eq!(b.selected(), vec![0]);
        // Shift-click: the range from the last click.
        b.pick(2, Pick::Only);
        b.pick(4, Pick::Range);
        assert_eq!((b.selected(), b.current), (vec![2, 3, 4], 4));
        b.pick(1, Pick::Range);
        assert_eq!((b.selected(), b.current), (vec![1, 2], 1));
        b.select_all();
        assert_eq!((b.selected(), b.current), (vec![0, 1, 2, 3, 4], 1));
        // Arrows: the neighbour, alone; clamped at the ends.
        b.step(1);
        assert_eq!((b.selected(), b.current), (vec![2], 2));
        b.step(-9);
        assert_eq!(b.current, 0);
        b.step(isize::MAX);
        assert_eq!(b.current, 4);
        // Out of range: nothing.
        b.pick(9, Pick::Only);
        assert_eq!(b.current, 4);
    }

    #[test]
    fn taking_photos_out_keeps_one_shown_and_selected() {
        let mut b = strip(5);
        b.pick(1, Pick::Toggle);
        b.pick(3, Pick::Toggle);
        assert_eq!((b.selected(), b.current), (vec![0, 1, 3], 3));
        // The shown photo goes: the next selected one from its place, or the one before.
        let gone = b.take_out(&[3, 3, 7]);
        assert_eq!(gone.len(), 1);
        assert_eq!(b.photos.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["IMG_0.dng", "IMG_1.dng", "IMG_2.dng", "IMG_4.dng"]);
        assert_eq!((b.selected(), b.current), (vec![0, 1], 1));
        // A photo before the shown one goes: the shown one stays.
        b.take_out(&[0]);
        assert_eq!((b.current_name(), b.selected()), ("IMG_1.dng", vec![0]));
        // No selected photo left at its place: the photo now there is shown, selected.
        b.take_out(&[0]);
        assert_eq!((b.current_name(), b.selected()), ("IMG_2.dng", vec![0]));
        b.take_out(&[0, 1]);
        assert!(b.photos.is_empty());
        assert_eq!(b.current_name(), "");
    }

    #[test]
    fn a_changed_setting_goes_to_the_selected_photos_only() {
        let a = CameraRaw { contrast: 20.0, ..raw_defaults() };
        let b = CameraRaw { exposure: 1.0, contrast: 20.0, encoding_gamma: 1.8, ..raw_defaults() };
        let changed = changed_settings(&a, &b);
        assert_eq!(changed.keys().collect::<Vec<_>>(), ["exposure"], "only what changed, never the per-image encoding");
        let other = CameraRaw { contrast: -15.0, vibrance: 30.0, ..raw_defaults() };
        let synced = with_settings(&other, &changed);
        assert_eq!((synced.exposure, synced.contrast, synced.vibrance), (1.0, -15.0, 30.0));
    }

    #[test]
    fn filmstrip_requests_are_validated() {
        let b = strip(3);
        assert_eq!(parse(&b, &json!({"select": 2})), Ok(Action::Pick(2, Pick::Only)));
        assert_eq!(parse(&b, &json!({"toggle": 1})), Ok(Action::Pick(1, Pick::Toggle)));
        assert_eq!(parse(&b, &json!({"extend": 0})), Ok(Action::Pick(0, Pick::Range)));
        assert_eq!(parse(&b, &json!({"selectAll": true})), Ok(Action::SelectAll));
        assert_eq!(parse(&b, &json!({"previous": true})), Ok(Action::Step(-1)));
        assert_eq!(parse(&b, &json!({"remove": [2, 0]})), Ok(Action::Remove(vec![2, 0])));
        assert_eq!(parse(&b, &json!({"remove": "selected"})), Ok(Action::RemoveSelected));
        for bad in [
            json!({"select": 3}),
            json!({"select": -1}),
            json!({"select": "1"}),
            json!({"remove": []}),
            json!({"remove": [0, 9]}),
            json!({"selectAll": false}),
            json!({"select": 0, "toggle": 1}),
            json!({}),
            json!({"open": true}),
            json!([0]),
        ] {
            assert!(parse(&b, &bad).is_err(), "{bad}");
        }
    }

    /// An app whose importer is the real one, and a synthetic DNG.
    fn raw_app() -> (PhotocraftApp, Vec<u8>) {
        use photocraft_raw::testgen::{DngSpec, mosaic, scene};
        let services = crate::Services {
            import: Some(Box::new(|name: &str, b: &[u8], opts: &photocraft_io::ImportOptions| {
                photocraft_io::import_with_options(name, b, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            ..Default::default()
        };
        let (w, h) = (48, 32);
        let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
        spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
        (PhotocraftApp::new(photocraft_engine::Session::new(), services), spec.build())
    }

    fn png() -> Vec<u8> {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 8, "height": 6, "background": "white"})).unwrap();
        photocraft_io::export(&s.active().unwrap().doc, "p.png", &Default::default()).unwrap().bytes
    }

    /// `names` written into a fresh directory for `tag` (raws get `dng`, the rest `other`).
    fn files(tag: &str, names: &[&str], dng: &[u8], other: &[u8]) -> (std::path::PathBuf, Vec<String>) {
        let dir = std::env::temp_dir().join(format!("photocraft-raw-batch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = names
            .iter()
            .map(|name| {
                let path = dir.join(name);
                std::fs::write(&path, if is_raw_name(name) { dng } else { other }).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        (dir, paths)
    }

    fn ui(app: &mut PhotocraftApp, ctx: &egui::Context, ui: Value) -> Result<Value, String> {
        crate::camera_raw_ui::menu(app, ctx, "filter.cameraRaw", &json!({ "ui": ui })).unwrap()
    }

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        ctx
    }

    fn frame(app: &mut PhotocraftApp, ctx: &egui::Context) {
        ctx.run_ui(Default::default(), |ui| crate::camera_raw_ui::show(app, ui.ctx())).textures_delta.clear();
    }

    fn strip_of(app: &PhotocraftApp) -> Value {
        crate::camera_raw_ui::state(app)["filmstrip"].clone()
    }

    fn names(app: &PhotocraftApp) -> Vec<String> {
        app.session.documents().iter().map(|d| d.doc.name.clone()).collect()
    }

    /// Three raws opened together (File › Open, the system) share one dialog; only the one shown
    /// is developed, and a PNG opened with them opens as usual.
    #[test]
    fn raws_opened_together_share_one_dialog_and_other_files_open_as_usual() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        let (dir, paths) = files("together", &["IMG_10.dng", "IMG_2.dng", "IMG_3.DNG", "logo.png"], &dng, &png());
        assert_eq!(app.open_paths(&paths), 4);
        assert_eq!(names(&app), ["IMG_2.dng", "logo.png"], "the first raw (name order) is developed, the others wait");
        frame(&mut app, &ctx);
        let state = crate::camera_raw_ui::state(&app);
        let strip = &state["filmstrip"];
        let listed: Vec<&str> = strip["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(listed, ["IMG_2.dng", "IMG_3.DNG", "IMG_10.dng"]);
        assert_eq!(strip["current"], 0);
        assert_eq!(strip["selected"], json!([0]));
        assert_eq!(strip["items"][0]["state"], "ready");
        assert_eq!(strip["items"][1]["state"], "waiting");
        assert_eq!(state["openingRaw"]["name"], "IMG_2.dng");
        assert_eq!(state["params"]["noiseColor"], RAW_COLOR_NOISE, "Camera Raw's raw defaults");
        assert!(crate::camera_raw_ui::is_open(&app));
        assert!(!crate::menus::modal_allows(&app, "file.new"), "modal like one raw's dialog");
        // A raw opened on its own meanwhile joins the strip instead of a dialog of its own.
        let more = dir.join("IMG_4.dng");
        std::fs::write(&more, &dng).unwrap();
        app.open_paths(&[more.to_string_lossy().into_owned()]);
        assert_eq!(strip_of(&app)["items"].as_array().unwrap().len(), 4);
        assert!(app.pending_raw_open.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_photo_keeps_its_own_settings() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        let (dir, paths) = files("settings", &["a.dng", "b.dng", "c.dng"], &dng, b"");
        app.open_paths(&paths);
        frame(&mut app, &ctx);
        ui(&mut app, &ctx, json!({"set": {"exposure": 1.0}})).unwrap();
        let shown = ui(&mut app, &ctx, json!({"filmstrip": {"select": 1}})).unwrap();
        assert_eq!(shown["openingRaw"]["name"], "b.dng");
        assert_eq!(shown["params"]["exposure"], 0.0, "b has its own settings");
        assert_eq!(shown["filmstrip"]["items"][0]["settings"]["exposure"], 1.0);
        ui(&mut app, &ctx, json!({"set": {"contrast": 25}, "demosaic": "bilinear"})).unwrap();
        // Arrows step back; each photo comes back as it was left.
        ui(&mut app, &ctx, json!({"filmstrip": {"previous": true}})).unwrap();
        let a = crate::camera_raw_ui::state(&app);
        assert_eq!((a["params"]["exposure"].clone(), a["params"]["contrast"].clone()), (json!(1.0), json!(0.0)));
        assert_eq!(a["openingRaw"]["demosaic"], "ahd");
        let b = ui(&mut app, &ctx, json!({"filmstrip": {"next": true}})).unwrap();
        assert_eq!((b["params"]["exposure"].clone(), b["params"]["contrast"].clone()), (json!(0.0), json!(25.0)));
        assert_eq!(b["openingRaw"]["demosaic"], "bilinear");
        assert_eq!(b["filmstrip"]["items"][1]["edited"], true);
        assert_eq!(b["filmstrip"]["items"][2]["edited"], false);
        // Shown photos keep their documents while the dialog is up; the third was never developed.
        assert_eq!(app.session.documents().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_setting_changed_with_several_selected_goes_to_the_selected_photos_only() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        let (dir, paths) = files("sync", &["a.dng", "b.dng", "c.dng"], &dng, b"");
        app.open_paths(&paths);
        frame(&mut app, &ctx);
        // b has a setting of its own first.
        ui(&mut app, &ctx, json!({"filmstrip": {"select": 1}})).unwrap();
        ui(&mut app, &ctx, json!({"set": {"vibrance": 30}})).unwrap();
        ui(&mut app, &ctx, json!({"filmstrip": {"select": 0}})).unwrap();
        ui(&mut app, &ctx, json!({"set": {"contrast": -20}})).unwrap();
        // a and c selected (c shown): Exposure goes to both, their other settings stay.
        let r = ui(&mut app, &ctx, json!({"filmstrip": {"toggle": 2}})).unwrap();
        assert_eq!(r["filmstrip"]["selected"], json!([0, 2]));
        assert_eq!(r["openingRaw"]["name"], "c.dng");
        ui(&mut app, &ctx, json!({"set": {"exposure": 0.75}})).unwrap();
        let items = strip_of(&app)["items"].clone();
        assert_eq!(items[0]["settings"]["exposure"], 0.75);
        assert_eq!(items[0]["settings"]["contrast"], -20.0, "a keeps its own contrast");
        assert_eq!(items[2]["settings"]["exposure"], 0.75);
        assert!(items[2]["settings"].get("contrast").is_none(), "nothing else is copied");
        assert!(items[1]["settings"].get("exposure").is_none(), "b is not selected");
        assert_eq!(items[1]["settings"]["vibrance"], 30.0);
        // Select All, then the demosaic method: every photo.
        ui(&mut app, &ctx, json!({"filmstrip": {"selectAll": true}})).unwrap();
        ui(&mut app, &ctx, json!({"demosaic": "mhc"})).unwrap();
        let items = strip_of(&app)["items"].clone();
        assert!(items.as_array().unwrap().iter().all(|i| i["demosaic"] == "mhc"), "{items}");
        assert_eq!(items[1]["settings"]["exposure"], json!(null), "the earlier change stays where it went");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Removed photos are not opened; their files stay as they were. Open opens the selected
    /// photos with their settings, each unmodified, and closes nothing else.
    #[test]
    fn removed_photos_are_not_opened_and_their_files_are_untouched() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let (dir, paths) = files("remove", &["a.dng", "b.dng", "c.dng"], &dng, b"");
        app.open_paths(&paths);
        frame(&mut app, &ctx);
        ui(&mut app, &ctx, json!({"set": {"exposure": 0.5}})).unwrap();
        // b is shown (so developed), then taken out: its document closes.
        ui(&mut app, &ctx, json!({"filmstrip": {"select": 1}})).unwrap();
        assert_eq!(app.session.documents().len(), 3);
        let r = ui(&mut app, &ctx, json!({"filmstrip": {"remove": 1}})).unwrap();
        assert_eq!(r["filmstrip"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(r["openingRaw"]["name"], "c.dng", "the photo after it is shown");
        assert_eq!(app.session.documents().len(), 3, "b's document closed, c's opened");
        ui(&mut app, &ctx, json!({"filmstrip": {"selectAll": true}})).unwrap();
        let open = ui(&mut app, &ctx, json!({"commit": true})).unwrap();
        assert_eq!(open["queued"], 2, "{open}");
        assert_eq!(open["pending"], false);
        assert_eq!((open["photos"][0]["document"].clone(), open["photos"][1]["document"].clone()), (json!(1), json!(2)), "{open}");
        assert!(!crate::camera_raw_ui::is_open(&app));
        assert_eq!(names(&app), ["Untitled", "a.dng", "c.dng"]);
        for st in &app.session.documents()[1..] {
            assert_eq!(st.saved_revision, st.revision, "Open leaves {} unmodified", st.doc.name);
            assert_eq!(st.doc.depth, photocraft_color::SampleType::U16);
            assert!(st.path.as_deref().is_some_and(|p| p.ends_with(&st.doc.name)));
        }
        assert_eq!(std::fs::read(&paths[1]).unwrap(), dng, "the removed file is untouched");
        assert!(app.ui.recent_files.iter().all(|p| !p.ends_with("b.dng")), "{:?}", app.ui.recent_files);
        assert_eq!(app.ui.recent_files.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_with_one_selected_opens_that_photo_and_closes_the_other_shown_ones() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        let (dir, paths) = files("one", &["a.dng", "b.dng", "c.dng"], &dng, b"");
        app.open_paths(&paths);
        frame(&mut app, &ctx);
        ui(&mut app, &ctx, json!({"filmstrip": {"select": 2}})).unwrap();
        // c's settings act on its sensor data, so Open re-develops it.
        let open = ui(&mut app, &ctx, json!({"set": {"exposure": 1.0}, "commit": true})).unwrap();
        assert_eq!(open["photos"][0]["redeveloped"], true, "{open}");
        assert_eq!(names(&app), ["c.dng"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_leaves_no_document_the_filmstrip_opened() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let (dir, paths) = files("cancel", &["a.dng", "b.dng", "c.dng", "d.png"], &dng, &png());
        app.open_paths(&paths);
        frame(&mut app, &ctx);
        ui(&mut app, &ctx, json!({"filmstrip": {"select": 2}})).unwrap();
        ui(&mut app, &ctx, json!({"filmstrip": {"toggle": 1}})).unwrap();
        assert_eq!(app.session.documents().len(), 5, "three raws shown, the PNG and the first document");
        assert_eq!(ui(&mut app, &ctx, json!({"cancel": true})).unwrap()["cancelled"], true);
        assert_eq!(names(&app), ["Untitled", "d.png"]);
        assert!(!crate::camera_raw_ui::is_open(&app));
        assert!(app.raw_batch.is_none() && app.camera_raw.is_none());
        // Taking every photo out is a Cancel too.
        app.open_paths(&paths[..2]);
        frame(&mut app, &ctx);
        ui(&mut app, &ctx, json!({"filmstrip": {"remove": [0, 1]}})).unwrap();
        assert!(!crate::camera_raw_ui::is_open(&app));
        assert_eq!(names(&app), ["Untitled", "d.png"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this fixes: raws opened one after another before the first dialog showed each
    /// replaced the one waiting, so only the last got a dialog. Now they share it.
    #[test]
    fn raws_opened_one_by_one_before_the_dialog_shows_share_it() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        app.open_bytes("a.dng", &dng).unwrap();
        app.open_bytes("b.dng", &dng).unwrap();
        frame(&mut app, &ctx);
        let state = crate::camera_raw_ui::state(&app);
        assert_eq!(state["openingRaw"]["name"], "a.dng");
        assert_eq!(state["filmstrip"]["items"].as_array().unwrap().len(), 2);
        // A third while the dialog shows: it joins too.
        app.open_bytes("c.dng", &dng).unwrap();
        assert_eq!(strip_of(&app)["items"].as_array().unwrap().len(), 3);
        ui(&mut app, &ctx, json!({"filmstrip": {"selectAll": true}})).unwrap();
        let open = ui(&mut app, &ctx, json!({"commit": true})).unwrap();
        assert_eq!(open["queued"], 3, "{open}");
        assert_eq!(names(&app), ["a.dng", "b.dng", "c.dng"]);
        assert!(app.session.documents().iter().all(|st| st.saved_revision == st.revision));
    }

    #[test]
    fn a_single_raw_dialog_becomes_a_filmstrip_when_more_raws_arrive() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        app.open_bytes("a.dng", &dng).unwrap();
        frame(&mut app, &ctx);
        assert!(app.raw_batch.is_none());
        ui(&mut app, &ctx, json!({"set": {"contrast": 15}})).unwrap();
        let mut contents = vec![("b.dng".to_string(), dng.clone()), ("c.dng".to_string(), dng.clone())];
        contents.push(("note.png".into(), png()));
        let rest = take_contents(&mut app, contents);
        assert_eq!(rest.len(), 1, "the PNG opens as usual");
        let state = crate::camera_raw_ui::state(&app);
        assert_eq!(state["filmstrip"]["items"].as_array().unwrap().len(), 3);
        assert_eq!(state["params"]["contrast"], 15.0, "the dialog stays as it was");
        ui(&mut app, &ctx, json!({"filmstrip": {"select": 1}})).unwrap();
        assert_eq!(strip_of(&app)["items"][0]["settings"]["contrast"], 15.0);
        assert!(ui(&mut app, &ctx, json!({"filmstrip": {"select": 0}, "set": {"exposure": 1}})).is_err(), "filmstrip goes alone");
        assert!(ui(&mut app, &ctx, json!({"filmstrip": {"select": 7}})).is_err());
        assert!(crate::camera_raw_ui::menu(&mut app, &ctx, "filter.cameraRaw", &json!({})).unwrap().is_err(), "no filter dialog over it");
        ui(&mut app, &ctx, json!({"cancel": true})).unwrap();
        assert!(app.session.documents().is_empty());
        assert!(ui(&mut app, &ctx, json!({"filmstrip": {"select": 0}})).is_err(), "no filmstrip now");
    }

    /// Raws arriving while the Camera Raw filter edits a layer wait for it: no document opens
    /// under it, and the filter's footer stays its own.
    #[test]
    fn raws_arriving_under_the_camera_raw_filter_wait_for_it() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        app.run("file.new", json!({"width": 16, "height": 16})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        crate::camera_raw_ui::menu(&mut app, &ctx, "filter.cameraRaw", &json!({})).unwrap().unwrap();
        take_contents(&mut app, vec![("a.dng".into(), dng.clone()), ("b.dng".into(), dng.clone())]);
        frame(&mut app, &ctx);
        assert_eq!(app.session.documents().len(), 1, "nothing opened under the filter");
        assert_eq!(strip_of(&app)["items"][0]["state"], "waiting");
        assert!(!showing(&app));
        // The filter's own Cancel, then the filmstrip shows its first photo.
        ui(&mut app, &ctx, json!({"cancel": true})).unwrap();
        assert!(app.raw_batch.is_some(), "the filmstrip is still there");
        frame(&mut app, &ctx);
        assert_eq!(crate::camera_raw_ui::state(&app)["openingRaw"]["name"], "a.dng");
        ui(&mut app, &ctx, json!({"cancel": true})).unwrap();
        assert_eq!(app.session.documents().len(), 1);
    }

    /// Stepping through a long strip keeps only the last few shown photos developed; the others
    /// keep their settings and develop again when shown or opened.
    #[test]
    fn stepping_through_many_photos_keeps_only_a_few_documents() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        let names_in: Vec<String> = (1..=7).map(|i| format!("IMG_{i}.dng")).collect();
        let refs: Vec<&str> = names_in.iter().map(String::as_str).collect();
        let (dir, paths) = files("many", &refs, &dng, b"");
        app.open_paths(&paths);
        frame(&mut app, &ctx);
        ui(&mut app, &ctx, json!({"set": {"exposure": 0.5}})).unwrap();
        for _ in 0..6 {
            ui(&mut app, &ctx, json!({"filmstrip": {"next": true}})).unwrap();
            assert!(app.session.documents().len() <= KEEP_DEVELOPED + 1, "{}", app.session.documents().len());
        }
        let strip = strip_of(&app);
        assert_eq!(strip["items"][0]["state"], "waiting", "the first photo's document closed");
        assert_eq!(strip["items"][0]["settings"]["exposure"], 0.5, "its settings stay");
        assert!(!strip["items"][0]["thumbnail"].is_null(), "and its thumbnail");
        // Back to it: developed again, as it was left.
        let first = ui(&mut app, &ctx, json!({"filmstrip": {"select": 0}})).unwrap();
        assert_eq!(first["params"]["exposure"], 0.5);
        ui(&mut app, &ctx, json!({"filmstrip": {"selectAll": true}})).unwrap();
        let open = ui(&mut app, &ctx, json!({"commit": true})).unwrap();
        assert_eq!(open["queued"], 7, "{open}");
        assert_eq!(names(&app), names_in, "the tabs in filmstrip order, however they were developed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In the desktop app the shown photo develops in the background (the dialog shows its
    /// preview meanwhile) and Open develops the selected photos one at a time.
    #[test]
    fn background_opens_show_the_photo_once_developed_and_open_one_at_a_time() {
        let (mut app, dng) = raw_app();
        app.background_jobs = true;
        let ctx = context();
        let (dir, paths) = files("background", &["a.dng", "b.dng", "c.dng"], &dng, b"");
        app.open_paths(&paths);
        assert_eq!(strip_of(&app)["items"][0]["state"], "opening");
        assert_eq!(crate::camera_raw_ui::state(&app)["params"], Value::Null);
        assert!(ui(&mut app, &ctx, json!({"set": {"exposure": 1}})).is_err(), "nothing to edit yet");
        let until = |app: &mut PhotocraftApp, done: &dyn Fn(&PhotocraftApp) -> bool| {
            for _ in 0..4000 {
                crate::jobs_ui::tick(app, &ctx);
                frame(app, &ctx);
                if done(app) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("timed out");
        };
        // Stepping on before it is developed stops that develop: one at a time.
        ui(&mut app, &ctx, json!({"filmstrip": {"next": true}})).unwrap();
        let strip = strip_of(&app);
        assert_eq!((strip["items"][0]["state"].clone(), strip["items"][1]["state"].clone()), (json!("waiting"), json!("opening")), "{strip}");
        ui(&mut app, &ctx, json!({"filmstrip": {"previous": true}})).unwrap();
        until(&mut app, &|app| app.camera_raw.is_some());
        assert_eq!(strip_of(&app)["items"][0]["state"], "ready");
        assert_eq!(strip_of(&app)["items"][1]["state"], "waiting");
        assert_eq!(app.session.documents().len(), 1);
        ui(&mut app, &ctx, json!({"filmstrip": {"selectAll": true}})).unwrap();
        ui(&mut app, &ctx, json!({"set": {"exposure": 0.5, "contrast": 10}})).unwrap();
        let open = ui(&mut app, &ctx, json!({"commit": true})).unwrap();
        assert_eq!(open["pending"], true, "{open}");
        assert_eq!(app.raw_redevelop.len(), 1, "one develop at a time");
        until(&mut app, &|app| app.raw_queue.is_empty() && app.raw_redevelop.is_empty() && !app.session.has_jobs());
        assert_eq!(names(&app), ["a.dng", "b.dng", "c.dng"]);
        for st in app.session.documents() {
            assert_eq!(st.saved_revision, st.revision, "{}", st.doc.name);
            assert_eq!(st.doc.depth, photocraft_color::SampleType::U16);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A NEF with Nikon's compression (not decoded yet) and a baseline JPEG preview in IFD0.
    fn nef_with_preview() -> Vec<u8> {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        use photocraft_raw::testgen::{TiffBuilder, Val};
        let img = Image::from_u8(32, 20, ChannelLayout::Rgb, vec![180; 32 * 20 * 3]).unwrap();
        let jpeg = photocraft_codecs::encode(&img, Format::Jpeg, &EncodeOptions::default()).unwrap();
        let mut t = TiffBuilder::default();
        let strip = t.blob(vec![0; 64]);
        let preview = t.blob(jpeg.clone());
        let raw = t.ifd(vec![
            (256, Val::Long(vec![8])),
            (257, Val::Long(vec![8])),
            (258, Val::Short(vec![12])),
            (259, Val::Short(vec![34713])),
            (262, Val::Short(vec![32803])),
            (273, Val::Blobs(vec![strip])),
            (279, Val::Long(vec![64])),
            (33421, Val::Short(vec![2, 2])),
            (33422, Val::Byte(vec![0, 1, 1, 2])),
        ]);
        let ifd0 = t.ifd(vec![
            (271, Val::Ascii("NIKON CORPORATION".into())),
            (330, Val::Ifds(vec![raw])),
            (513, Val::Blobs(vec![preview])),
            (514, Val::Long(vec![jpeg.len() as u32])),
        ]);
        t.chain = vec![ifd0];
        t.build()
    }

    /// A raw shows its camera preview in the strip before it is developed; one whose sensor data
    /// can't be developed yet opens as that preview, an ordinary document outside the strip.
    #[test]
    fn thumbnails_come_from_the_embedded_preview_and_a_preview_only_raw_opens_as_usual() {
        let (mut app, dng) = raw_app();
        let ctx = context();
        let (dir, mut paths) = files("preview", &["a.dng"], &dng, b"");
        let nef = dir.join("b.nef");
        std::fs::write(&nef, nef_with_preview()).unwrap();
        paths.push(nef.to_string_lossy().into_owned());
        app.open_paths(&paths);
        for _ in 0..2000 {
            frame(&mut app, &ctx);
            let strip = strip_of(&app);
            if !strip["items"][0]["thumbnail"].is_null() && !strip["items"][1]["thumbnail"].is_null() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let strip = strip_of(&app);
        assert_eq!(strip["items"][1]["thumbnail"], json!([32, 20]), "{strip}");
        assert_eq!(strip["items"][1]["state"], "waiting", "read without developing");
        assert_eq!(strip["items"][0]["thumbnail"], json!([48, 32]), "the shown photo: its developed preview");
        let r = ui(&mut app, &ctx, json!({"filmstrip": {"select": 1}})).unwrap();
        assert_eq!(r["filmstrip"]["items"].as_array().unwrap().len(), 1, "{r}");
        assert_eq!(r["openingRaw"]["name"], "a.dng", "the raw left is shown again");
        assert_eq!(names(&app), ["a.dng", "b.nef"]);
        ui(&mut app, &ctx, json!({"cancel": true})).unwrap();
        assert_eq!(names(&app), ["b.nef"], "it opened like any file");
        assert_eq!(app.session.documents()[0].path.as_deref(), Some(paths[1].as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_names_by_extension() {
        for name in ["a.NEF", "b.dng", "/x/c.Cr2", "d.ARW", "e.raf"] {
            assert!(is_raw_name(name), "{name}");
        }
        for name in ["a.png", "b.psd", "nef", "c.nef.jpg", ""] {
            assert!(!is_raw_name(name), "{name}");
        }
    }
}
