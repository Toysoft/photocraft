//! Full CPU redraws of the GPU canvas off the UI thread (#4024). When the wgpu compositor can't
//! draw a full refresh (the layers exceed its memory budget, or the document uses something it
//! doesn't cover), the CPU compositor redraws the whole document: minutes on a multi-gigabyte
//! PSB. With background jobs (the desktop app) a worker composites it in blocks instead, the
//! visible ones first, and each frame uploads the blocks that are done, so the window keeps
//! drawing and answering while the rest of the canvas fills in. Until a block lands the texture
//! keeps what it showed before.
//!
//! An edit while it runs restarts the worker on the new document with the blocks still to draw;
//! the edit's own damage is drawn by the canvas as usual. A full change (a new preview, a new
//! display transform) starts over. Without background jobs (web, tests) the canvas keeps the
//! synchronous [`GpuCanvas::upload_composite`].
//!
//! Zoomed out to 50% or less, the worker first composites a reduced copy of the document at the
//! view's scale ([`preview_document`]) and the canvas puts it in the mip level the view samples,
//! so a whole picture shows in a fraction of the full redraw's time. Each full-resolution block
//! rebuilds its mip levels from level 0 as it lands, replacing the reduced picture there, so the
//! finished canvas is the same as without it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};

use photocraft_doc::{DocId, Document, Layer, LayerContent};
use photocraft_engine::display_color::CanvasDisplay;
use photocraft_geom::{Affine, Rect};
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;

use crate::PhotocraftApp;
use crate::gpu_canvas::GpuCanvas;

/// Side of a block in document pixels: whole render tiles, and a power of two so each block's
/// mip levels don't depend on its neighbours.
const BLOCK: i32 = 1024;

/// Blocks composited at once (in parallel, on top of each block's own tiles), and how many
/// finished blocks may wait for the UI thread: bounds the memory a redraw holds.
const BATCH: usize = 4;

/// The coarsest mip level a reduced first picture goes to (1/16 of the document's size): finer
/// than any view zoomed out further needs, and already a small fraction of the full redraw.
const MAX_PREVIEW_LEVEL: u32 = 4;

/// What the worker hands over, as texels of the canvas texture.
enum Piece {
    /// One finished block.
    Block { block: usize, texels: Vec<u8> },
    /// The reduced first picture: `rect` of mip `level`.
    Preview { level: u32, rect: Rect, texels: Vec<u8> },
}

/// A redraw in progress for one document's GPU texture.
pub(crate) struct Redraw {
    blocks: Vec<Rect>,
    pending: Vec<bool>,
    left: usize,
    /// Pixels uploaded so far.
    drawn: u64,
    /// The mip level of a reduced first picture still to land.
    preview: Option<u32>,
    rx: Receiver<Piece>,
    stop: Arc<AtomicBool>,
    started_ms: f64,
    fallback: Option<String>,
}

impl Redraw {
    /// Share of the document drawn, 0..=1.
    pub fn fraction(&self) -> f32 {
        if self.blocks.is_empty() { 1.0 } else { 1.0 - self.left as f32 / self.blocks.len() as f32 }
    }
}

impl Drop for Redraw {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Whether full CPU refreshes run in the background here.
pub(crate) fn enabled(app: &PhotocraftApp) -> bool {
    cfg!(not(target_arch = "wasm32")) && app.background_jobs
}

/// `bounds` cut into [`BLOCK`]-sized blocks, row by row.
fn blocks(bounds: Rect) -> Vec<Rect> {
    let mut out = Vec::new();
    let mut y = bounds.y0;
    while y < bounds.y1 {
        let mut x = bounds.x0;
        while x < bounds.x1 {
            out.push(Rect::new(x, y, (x + BLOCK).min(bounds.x1), (y + BLOCK).min(bounds.y1)));
            x += BLOCK;
        }
        y += BLOCK;
    }
    out
}

/// The mip level a view at `scale` (screen pixels per document pixel) samples, if a reduced
/// first picture is worth drawing there: at 50% or less, capped at [`MAX_PREVIEW_LEVEL`].
fn preview_level(scale: f32) -> Option<u32> {
    (scale.is_finite() && scale > 0.0 && scale <= 0.5).then(|| ((-scale.log2()).floor() as u32).clamp(1, MAX_PREVIEW_LEVEL))
}

/// `r` (document pixels) in the pixels of a level `k` times smaller, covering every pixel `r`
/// touches.
fn reduce(r: Rect, k: i32) -> Rect {
    Rect::new(
        r.x0.div_euclid(k),
        r.y0.div_euclid(k),
        r.x1.div_euclid(k) + i32::from(r.x1.rem_euclid(k) != 0),
        r.y1.div_euclid(k) + i32::from(r.y1.rem_euclid(k) != 0),
    )
}

/// `doc` scaled down by `k` for a reduced first picture: [`photocraft_compose::proxy::proxy_document`]
/// with what is measured in document pixels scaled too (layer effects, mask feathers, vector mask
/// paths), so styled documents look right at the reduced size. Only a stand-in: the full
/// redraw replaces it.
pub(crate) fn preview_document(doc: &Document, k: u32) -> Document {
    fn scale_layer(l: &mut Layer, k: f32) {
        photocraft_engine::image_cmds::scale_effects(&mut l.effects, 1.0 / k);
        l.effects.reference = l.effects.reference.map(|(x, y)| (x / f64::from(k), y / f64::from(k)));
        if let Some(m) = &mut l.mask {
            m.feather /= k;
        }
        if let Some(v) = &mut l.vector_mask {
            v.path = v.path.transform(&Affine::scale(1.0 / f64::from(k)));
            v.feather /= k;
        }
        if let LayerContent::Group(g) = &mut l.content {
            for c in &mut g.children {
                scale_layer(c, k);
            }
        }
    }
    let mut p = photocraft_compose::proxy::proxy_document(doc, k);
    if k > 1 {
        for l in &mut p.layers {
            scale_layer(l, k as f32);
        }
    }
    p
}

/// The area (document pixels) a reduced first picture covers: the blocks still to draw, `todo`,
/// that the view shows. The others aren't in sight, and draw as they come.
fn preview_area(blocks: &[Rect], todo: &[usize], visible: Rect) -> Rect {
    todo.iter().map(|&i| blocks[i]).filter(|b| !b.intersect(&visible).is_empty()).fold(Rect::EMPTY, |r, b| if r.is_empty() { b } else { r.union(&b) })
}

/// The order to draw `wanted` blocks in: those `visible` shows first, then the rest, each group
/// nearest the view's centre first.
fn order(blocks: &[Rect], wanted: impl Iterator<Item = usize>, visible: Rect) -> Vec<usize> {
    let (cx, cy) = ((visible.x0 as i64 + visible.x1 as i64) / 2, (visible.y0 as i64 + visible.y1 as i64) / 2);
    let mut out: Vec<usize> = wanted.collect();
    out.sort_by_key(|&i| {
        let b = blocks[i];
        let (bx, by) = ((b.x0 as i64 + b.x1 as i64) / 2, (b.y0 as i64 + b.y1 as i64) / 2);
        (b.intersect(&visible).is_empty(), (bx - cx).pow(2) + (by - cy).pow(2))
    });
    out
}

/// Redraw `region` of document `doc` (`doc.id`'s texture, prepared by
/// [`GpuCanvas::refresh_with`]) in the background, for a view at `scale` screen pixels per
/// document pixel. A redraw already running for it continues on `doc` with `region`'s blocks
/// added.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start(
    app: &mut PhotocraftApp,
    ctx: &egui::Context,
    gpu: &GpuCanvas,
    doc: Arc<Document>,
    display: Option<Arc<CanvasDisplay>>,
    visible: Rect,
    scale: f32,
    region: Rect,
    fallback: Option<String>,
) {
    let all = blocks(doc.bounds());
    let (mut pending, since, fallback, preview) = match app.redraws.remove(&doc.id) {
        Some(old) if old.blocks == all => (old.pending.clone(), (old.started_ms, old.drawn), fallback.or_else(|| old.fallback.clone()), old.preview),
        _ => (vec![false; all.len()], (crate::gpu_canvas::now_ms(), 0), fallback, None),
    };
    for (p, b) in pending.iter_mut().zip(&all) {
        *p |= !b.intersect(&region).is_empty();
    }
    // A reduced first picture when the redraw covers what the view shows (one still to land is
    // drawn again from `doc`).
    let shown = visible.intersect(&doc.bounds());
    let preview = preview_level(scale).filter(|_| !shown.is_empty() && region.contains_rect(&shown)).or(preview);
    if pending.contains(&true) {
        spawn(app, ctx, gpu, doc, display, visible, all, pending, preview, since, fallback);
    }
}

/// After an edit drew its own damage: continue document `doc.id`'s redraw, if one is running, on
/// the edited document, so blocks still to come don't bring back what was there before it.
pub(crate) fn restart(app: &mut PhotocraftApp, ctx: &egui::Context, gpu: &GpuCanvas, doc: Arc<Document>, display: Option<Arc<CanvasDisplay>>, visible: Rect) {
    if app.redraws.contains_key(&doc.id) {
        start(app, ctx, gpu, doc, display, visible, 1.0, Rect::EMPTY, None);
    }
}

/// Upload the blocks document `doc`'s redraw has finished since the last frame. When the last one
/// lands, the redraw is recorded like the synchronous refresh it replaces (and explained once if
/// it was slow).
pub(crate) fn poll(app: &mut PhotocraftApp, gpu: &GpuCanvas, doc: DocId) {
    let Some(r) = app.redraws.get_mut(&doc) else { return };
    loop {
        match r.rx.try_recv() {
            Ok(Piece::Block { block, texels }) => {
                let b = r.blocks[block];
                if r.pending[block] && gpu.upload_rect(doc.0, [b.x0 as u32, b.y0 as u32], [b.width(), b.height()], &texels) {
                    r.pending[block] = false;
                    r.left -= 1;
                    r.drawn += b.width() as u64 * b.height() as u64;
                }
            }
            Ok(Piece::Preview { level, rect, texels }) => {
                upload_preview(r, gpu, doc, level, rect, &texels);
                r.preview = None;
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => break,
        }
    }
    let Some(r) = app.redraws.remove(&doc) else { return };
    if r.left > 0 {
        log::warn!("canvas redraw stopped with {} of {} blocks left", r.left, r.blocks.len());
        return;
    }
    let ms = crate::gpu_canvas::now_ms() - r.started_ms;
    app.perf.record("background", r.drawn, ms, 0.0);
    crate::notices::slow_cpu_refresh(app, doc.0, ms, r.fallback.as_deref());
}

/// Put a reduced first picture (`rect` of mip `level`) where blocks are still to come: they
/// replace it as they land. Elsewhere the texture already shows the finished composite.
fn upload_preview(r: &Redraw, gpu: &GpuCanvas, doc: DocId, level: u32, rect: Rect, texels: &[u8]) {
    let (k, stride) = (1 << level, rect.width());
    let px = stride as usize * rect.height() as usize;
    if px == 0 || !texels.len().is_multiple_of(px) {
        return;
    }
    let bpp = texels.len() / px;
    let put = |at: Rect| {
        let at = at.intersect(&rect);
        if at.is_empty() {
            return;
        }
        let offset = ((at.y0 - rect.y0) as usize * stride as usize + (at.x0 - rect.x0) as usize) * bpp;
        if let Some(t) = texels.get(offset..) {
            gpu.upload_level(doc.0, level, [at.x0 as u32, at.y0 as u32], [at.width(), at.height()], stride, t);
        }
    };
    let touched = |b: &Rect| !reduce(*b, k).intersect(&rect).is_empty();
    // A fresh redraw: every block under the picture is pending, so one upload covers them all.
    if r.blocks.iter().zip(&r.pending).all(|(b, &p)| p || !touched(b)) {
        put(rect);
    } else {
        for (b, _) in r.blocks.iter().zip(&r.pending).filter(|(b, p)| **p && touched(b)) {
            put(reduce(*b, k));
        }
    }
}

/// The active document's redraw in the status bar: "Redrawing" and how much is done.
pub(crate) fn status(app: &PhotocraftApp, ui: &mut egui::Ui) {
    let Some(r) = app.session.active().and_then(|st| app.redraws.get(&st.doc.id)) else { return };
    let t = crate::theme::Tokens::get(ui.ctx());
    ui.label(egui::RichText::new(format!("{:.0}%", r.fraction() * 100.0)).color(t.text_dim).size(11.5).monospace());
    ui.label(egui::RichText::new(tl!("Redrawing")).color(t.text_dim).size(12.0));
}

/// Stop document `doc`'s redraw (its canvas was redrawn some other way, or is gone).
pub(crate) fn cancel(app: &mut PhotocraftApp, doc: DocId) {
    app.redraws.remove(&doc);
}

/// Stop redraws of documents that are no longer open.
pub(crate) fn retain(app: &mut PhotocraftApp) {
    let docs = app.session.documents();
    app.redraws.retain(|id, _| docs.iter().any(|st| st.doc.id == *id));
}

#[allow(clippy::too_many_arguments)]
fn spawn(
    app: &mut PhotocraftApp,
    ctx: &egui::Context,
    gpu: &GpuCanvas,
    doc: Arc<Document>,
    display: Option<Arc<CanvasDisplay>>,
    visible: Rect,
    blocks: Vec<Rect>,
    pending: Vec<bool>,
    preview: Option<u32>,
    (started_ms, drawn): (f64, u64),
    fallback: Option<String>,
) {
    let id = doc.id;
    let Some(format) = gpu.prepare_texture(id.0, &doc) else { return };
    let todo = order(&blocks, (0..blocks.len()).filter(|&i| pending[i]), visible);
    let first = preview.map(|level| (level, preview_area(&blocks, &todo, visible))).filter(|(_, area)| !area.is_empty());
    let left = todo.len();
    let (tx, rx) = std::sync::mpsc::sync_channel(BATCH);
    let stop = Arc::new(AtomicBool::new(false));
    let work = (blocks.clone(), stop.clone(), ctx.clone());
    let spawned = std::thread::Builder::new().name("canvas-redraw".into()).spawn(move || {
        let (blocks, stop, ctx) = work;
        // A panic stops the redraw: the receiver sees the worker gone and drops it.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| draw(&doc, display.as_deref(), format, first, &blocks, &todo, &stop, &tx, &ctx)));
        ctx.request_repaint();
    });
    if let Err(e) = spawned {
        log::warn!("couldn't start the canvas redraw: {e}");
        return;
    }
    app.redraws.insert(id, Redraw { blocks, pending, left, drawn, preview, rx, stop, started_ms, fallback });
}

/// The worker: composite the reduced first picture of `first`'s area (mip level, document
/// pixels), if any, then `todo`'s blocks of `doc` in batches, handing each over, until done or
/// stopped.
#[allow(clippy::too_many_arguments)]
fn draw(
    doc: &Document,
    display: Option<&CanvasDisplay>,
    format: eframe::wgpu::TextureFormat,
    first: Option<(u32, Rect)>,
    blocks: &[Rect],
    todo: &[usize],
    stop: &AtomicBool,
    tx: &SyncSender<Piece>,
    ctx: &egui::Context,
) {
    if let Some((level, area)) = first {
        let k = 1 << level;
        let small = preview_document(doc, k as u32);
        let rect = reduce(area, k).intersect(&small.bounds());
        let texels = crate::gpu_canvas::display_texels(format, display, &photocraft_compose::render(&small, rect));
        if stop.load(Ordering::Relaxed) || tx.send(Piece::Preview { level, rect, texels }).is_err() {
            return;
        }
        ctx.request_repaint();
    }
    for batch in todo.chunks(BATCH) {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Never reached in the browser ([`enabled`]); it only has to build there, without rayon.
        #[cfg(not(target_arch = "wasm32"))]
        let batch = batch.par_iter();
        #[cfg(target_arch = "wasm32")]
        let batch = batch.iter();
        let pieces: Vec<Piece> = batch
            .map(|&block| Piece::Block { block, texels: crate::gpu_canvas::display_texels(format, display, &photocraft_compose::render(doc, blocks[block])) })
            .collect();
        for p in pieces {
            if stop.load(Ordering::Relaxed) || tx.send(p).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
#[path = "background_redraw_tests.rs"]
mod tests;
