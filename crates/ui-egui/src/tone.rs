//! Histograms for the tone editors (Levels, Curves, Threshold) and the Histogram panel.
//!
//! The adjustment editors themselves live in `adjust_editors`; this module computes and caches
//! the histograms they draw behind their controls, in the channels of the adjustment's space
//! (RGB, CMYK ink brightness, Lab), and draws them. Photoshop keeps the two audiences apart
//! (see [`HistScope`]): the panel follows the selection, the editors' dialogs do not.

use std::sync::Arc;

use egui::{Color32, Rect, Sense, vec2};
use photocraft_doc::adjust::ToneSpace;
use photocraft_doc::{Document, Layer, LayerId};

use crate::PhotocraftApp;
use crate::theme::Tokens;

const CHANNELS: [(&str, &str); 4] = [("rgb", "RGB"), ("red", "Red"), ("green", "Green"), ("blue", "Blue")];

static EMPTY: [u32; 256] = [0; 256];

/// 256-bin histograms: the composite then one per channel of the space (RGB: luminosity, R, G, B;
/// CMYK: composite, C, M, Y, K as channel brightness; Lab: L, a, b). `tag` identifies what they
/// were computed from (see [`HistSource`]).
#[derive(Clone, Debug, Default)]
pub struct Histograms {
    pub tag: u64,
    pub ch: Vec<[u32; 256]>,
}

impl std::ops::Index<usize> for Histograms {
    type Output = [u32; 256];
    fn index(&self, i: usize) -> &[u32; 256] {
        self.ch.get(i).unwrap_or(&EMPTY)
    }
}

/// What a tone editor's histogram shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistSource {
    /// The image below an adjustment layer (what the adjustment receives).
    BelowLayer(LayerId),
    /// One layer on its own (the target of a destructive Image › Adjustments command).
    Layer(LayerId),
    /// The gray plane a Curves target edits: a layer mask, a saved channel or the Quick Mask.
    /// The dialog shows this plane's own levels, not the colour layers beneath (#5335).
    Channel { layer: LayerId, surface: photocraft_engine::channel_cmds::GraySurface },
}

/// Whether a histogram counts the whole input or only the selected pixels. Photoshop's Histogram
/// panel follows the selection ("to display histogram data for a portion of the image, first
/// select that portion" — Adobe), while its Levels/Curves/Threshold dialogs show the
/// adjustment's whole input: with a selection active the histogram behind the curve is of the
/// entire image, not just the selected pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistScope {
    /// The whole input, unmasked by any selection (the tone editors' histograms).
    WholeInput,
    /// Only the selected pixels, partial coverage counting from ½ up (the Histogram panel).
    Selection,
}

fn tag(source: HistSource, space: ToneSpace, scope: HistScope, sel: u64) -> u64 {
    let (k, id) = match source {
        HistSource::BelowLayer(l) => (1u64, l.0),
        HistSource::Layer(l) => (2, l.0),
        HistSource::Channel { layer, surface } => match surface {
            photocraft_engine::channel_cmds::GraySurface::LayerMask => (3, layer.0),
            photocraft_engine::channel_cmds::GraySurface::Alpha(i) => (4, i as u64),
            photocraft_engine::channel_cmds::GraySurface::QuickMask => (5, layer.0),
        },
    };
    let s = match space {
        ToneSpace::Rgb => 0u64,
        ToneSpace::Cmyk => 1,
        ToneSpace::Lab => 2,
    };
    let m = match scope {
        HistScope::WholeInput => 0u64,
        HistScope::Selection => 1,
    };
    id.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (k << 60) ^ (s << 56) ^ (m << 52) ^ sel.rotate_left(31)
}

/// Cache identity of the current selection: the mask's default (what tile-less pixels read as),
/// its bounds, and which tiles it is built from. A clone shares its tiles, so an untouched
/// selection keeps its key across edits (the #125 fast path stays); any command that writes a
/// new mask replaces at least the tiles it touched, or the default (e.g. `select.inverse`'s
/// `prune` leaves a tile-less mask), which changes the key. 0 when nothing is selected.
///
/// Tile identities are pointers, and an allocator can hand a freed tile's address to a new tile
/// (ABA), so two different selections can in principle hash alike. A stale hit needs that
/// collision inside the single view-only revision the caches promote across, and selection
/// commands build the new mask before dropping the old one (no free-then-allocate), so the
/// addresses cannot meet within one command; the default and bounds in the key keep the
/// pointer-less masks (all vs. none) apart regardless.
fn selection_key(doc: &Document) -> u64 {
    use std::hash::{Hash, Hasher};
    let Some(m) = doc.selection.as_ref() else { return 0 };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    1u8.hash(&mut h);
    m.default_bytes().hash(&mut h);
    let b = m.tile_bounds();
    (b.x0, b.y0, b.x1, b.y1).hash(&mut h);
    for (c, t) in m.tiles() {
        (c.tx, c.ty).hash(&mut h);
        std::ptr::hash(Arc::as_ptr(t), &mut h);
    }
    h.finish()
}

/// Shows only the branch of the layer tree holding `id`. Returns whether `id` is in `layers`.
fn isolate(layers: &mut [Layer], id: LayerId) -> bool {
    let mut found = false;
    for l in layers.iter_mut() {
        let here = l.id == id || l.children_mut().is_some_and(|c| isolate(c, id));
        l.visible = here;
        found |= here;
    }
    found
}

/// Histograms of `doc` as seen by `source`, in the channels of `space`. [`HistScope::Selection`]
/// counts only the selected pixels (the Histogram panel; partial coverage counts from ½ up);
/// [`HistScope::WholeInput`] ignores the selection, like Photoshop's Levels/Curves/Threshold
/// dialogs, which show the adjustment's whole input.
pub fn compute_histograms(doc: &Document, source: HistSource, space: ToneSpace, scope: HistScope) -> Histograms {
    let mut h = vec![
        [0u32; 256];
        match space {
            ToneSpace::Rgb => 4,
            ToneSpace::Cmyk => 5,
            ToneSpace::Lab => 3,
        }
    ];
    match source {
        // The gray plane itself: its levels are what the curve edits, whatever the document's
        // colour mode (#5335). A selection does not mask a plane the dialog edits directly.
        HistSource::Channel { layer, surface } => {
            let surf = match surface {
                photocraft_engine::channel_cmds::GraySurface::LayerMask => doc.layer(layer).and_then(|l| l.mask.as_ref()).map(|m| &m.surface),
                photocraft_engine::channel_cmds::GraySurface::Alpha(i) => doc.channels.get(i).map(|c| &c.surface),
                photocraft_engine::channel_cmds::GraySurface::QuickMask => doc.quick_mask.as_ref().map(|c| &c.surface),
            };
            if let Some(s) = surf
                && let Some(bin) = h.first_mut()
            {
                *bin = gray_bins(s, doc.bounds());
            }
        }
        HistSource::BelowLayer(_) | HistSource::Layer(_) => {
            composite_bins(doc, source, space, scope, &mut h);
        }
    }
    let sel_tag = if scope == HistScope::Selection { selection_key(doc) } else { 0 };
    Histograms { tag: tag(source, space, scope, sel_tag), ch: h }
}

/// Bins of the composited image below (`BelowLayer`) or of one layer alone (`Layer`), in `space`,
/// counting the whole input or only the selected pixels per `scope`.
fn composite_bins(doc: &Document, source: HistSource, space: ToneSpace, scope: HistScope, h: &mut [[u32; 256]]) {
    let mut d = doc.clone();
    match source {
        HistSource::BelowLayer(hide) => {
            if let Some(l) = d.layer_mut(hide) {
                l.visible = false;
            }
        }
        HistSource::Layer(id) => {
            isolate(&mut d.layers, id);
        }
        HistSource::Channel { .. } => {}
    }
    let img = photocraft_compose::thumbnail(&d, 384);
    let b = d.bounds();
    let sel = match scope {
        HistScope::WholeInput => None,
        HistScope::Selection => d.selection.as_ref(),
    };
    // Document coordinate of each thumbnail pixel's centre (the reduction starts at the canvas
    // origin, so scaling alone maps between them).
    let (fx, fy) = (b.width().max(1) as f32 / img.width.max(1) as f32, b.height().max(1) as f32 / img.height.max(1) as f32);
    let iw = img.width.max(1) as usize;
    let bin = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as usize;
    for (i, p) in img.pixels.as_chunks::<4>().0.iter().enumerate() {
        if p[3] == 0 {
            continue;
        }
        if let Some(m) = sel {
            let mx = ((i % iw) as f32 + 0.5) * fx;
            let my = ((i / iw) as f32 + 0.5) * fy;
            if m.sample_channel(mx as i32, my as i32, 0) < 0.5 {
                continue;
            }
        }
        match space {
            ToneSpace::Rgb => {
                for c in 0..3 {
                    h[c + 1][p[c] as usize] += 1;
                }
                h[0][((p[0] as u32 + p[1] as u32 + p[2] as u32) / 3) as usize] += 1;
            }
            ToneSpace::Cmyk => {
                let rgb = [p[0], p[1], p[2]].map(|v| f32::from(v) / 255.0);
                let ink = photocraft_color::convert::rgb_to_cmyk(rgb);
                for (c, v) in ink.iter().enumerate() {
                    h[c + 1][bin(1.0 - v)] += 1;
                }
                h[0][bin(1.0 - ink.iter().sum::<f32>() / 4.0)] += 1;
            }
            ToneSpace::Lab => {
                let rgb = [p[0], p[1], p[2]].map(|v| f32::from(v) / 255.0);
                let l = photocraft_color::convert::srgb_to_lab(rgb);
                h[0][bin(l[0] / 100.0)] += 1;
                h[1][bin((l[1] + 128.0) / 255.0)] += 1;
                h[2][bin((l[2] + 128.0) / 255.0)] += 1;
            }
        }
    }
}

/// 256-bin histogram of a gray plane: its levels in 0–255, sampled like the engine's auto-tone
/// histograms so a big canvas costs no more than about a megapixel here (#5335).
fn gray_bins(surf: &photocraft_raster::Surface, area: photocraft_geom::Rect) -> [u32; 256] {
    let mut h = [0u32; 256];
    let step = ((u64::from(area.width()) * u64::from(area.height())) / 1_000_000).max(1).isqrt().max(1) as i32;
    let mut y = area.y0;
    while y < area.y1 {
        let mut x = area.x0;
        while x < area.x1 {
            let v = (surf.sample_channel(x, y, 0).clamp(0.0, 1.0) * 255.0).round() as usize;
            if let Some(bin) = h.get_mut(v) {
                *bin += 1;
            }
            x += step;
        }
        y += step;
    }
    h
}

/// Cached histograms for a tone editor; recomputed only when something other than the editor's
/// own commits changed the document (see [`keep_after_commit`]). The editors' histograms show the
/// whole input ([`HistScope::WholeInput`]), so a selection change — view-only, no pixels moved —
/// keeps the cache (#125); only the panel's masked histogram tracks the selection.
pub fn histograms(app: &mut PhotocraftApp, source: HistSource, space: ToneSpace) -> Arc<Histograms> {
    let Some(st) = app.session.active() else { return Arc::new(Histograms::default()) };
    let (doc_id, rev) = (st.doc.id, st.revision);
    let want = tag(source, space, HistScope::WholeInput, 0);
    let layer = match source {
        HistSource::BelowLayer(l) | HistSource::Layer(l) => l,
        HistSource::Channel { layer, .. } => layer,
    };
    let pixels_kept = view_only(st);
    if let Some((d, l, r, h)) = app.tone_hist.as_mut()
        && *d == doc_id
        && *l == layer
        && (*r == rev || (*r + 1 == rev && pixels_kept))
        && h.tag == want
    {
        *r = rev;
        return h.clone();
    }
    let t0 = crate::gpu_canvas::now_ms();
    let h = Arc::new(compute_histograms(&st.doc, source, space, HistScope::WholeInput));
    app.perf.span("histogram", crate::gpu_canvas::now_ms() - t0);
    app.tone_hist = Some((doc_id, layer, rev, h.clone()));
    h
}

/// Whether the latest revision changed no pixels (selecting a layer, say): cached histograms of
/// the previous revision still hold (#125).
fn view_only(st: &photocraft_engine::DocState) -> bool {
    st.last_damage.is_some_and(|r| r.is_empty())
}

/// After an editor commits its own adjustment layer, the image below it is unchanged: keep the
/// cached histogram valid for the new revision. Call with the revision seen before the commit.
pub fn keep_after_commit(app: &mut PhotocraftApp, layer: LayerId, before: Option<u64>) {
    let now = app.session.active().map(|s| s.revision);
    if let (Some((_, l, r, _)), Some(before), Some(now)) = (app.tone_hist.as_mut(), before, now)
        && *l == layer
        && *r == before
    {
        *r = now;
    }
}

/// The pixel count the Histogram panel reports: the whole canvas, or with a selection the
/// selected pixels (Photoshop). Counts mask coverage ≥ ½ at full resolution, exactly.
pub fn histogram_pixels(doc: &Document) -> u64 {
    let Some(m) = doc.selection.as_ref() else {
        return u64::from(doc.size.width) * u64::from(doc.size.height);
    };
    let b = doc.bounds();
    let row_n = b.width().max(1) as usize;
    if m.format() != photocraft_color::PixelFormat::GRAY8 {
        // Selection masks are 8-bit today; anything else scans one strided row at a time.
        let mut row = Vec::new();
        let mut n = 0u64;
        for y in b.y0..b.y1 {
            row.clear();
            row.resize(row_n, 0.0);
            m.sample_row_strided(y, b.x0, 1, 0, &mut row);
            n += row.iter().filter(|&&v| v >= 0.5).count() as u64;
        }
        return n;
    }
    let absent_is_selected = m.default_bytes().first().is_some_and(|&v| v >= 128);
    let ts = photocraft_geom::TILE_SIZE;
    let mut n = 0u64;
    for tc in b.tiles() {
        let tr = tc.rect().intersect(&b);
        if tr.is_empty() {
            continue;
        }
        match m.tile(tc) {
            Some(t) => {
                let data = t.bytes();
                let (x0, y0) = ((tr.x0 - tc.tx * ts) as usize, (tr.y0 - tc.ty * ts) as usize);
                for y in 0..tr.height() as usize {
                    let off = (y0 + y) * photocraft_geom::TILE_SIZE as usize + x0;
                    n += data.get(off..off + tr.width() as usize).map_or(0, |row| row.iter().filter(|&&v| v >= 128).count()) as u64;
                }
            }
            None if absent_is_selected => n += u64::from(tr.width()) * u64::from(tr.height()),
            None => {}
        }
    }
    n
}

pub fn draw_histogram(p: &egui::Painter, r: Rect, h: &[u32; 256], color: Color32) {
    // Scale to a high percentile so a single spike (e.g. pure white) doesn't flatten the rest.
    let mut sorted: Vec<u32> = h.to_vec();
    sorted.sort_unstable();
    let top = sorted.get(250).copied().unwrap_or(1).max(1) as f32 * 1.1;
    let mut pts = vec![egui::pos2(r.left(), r.bottom())];
    for (i, v) in h.iter().enumerate() {
        let x = r.left() + (i as f32 + 0.5) / 256.0 * r.width();
        let y = r.bottom() - (*v as f32 / top).min(1.0) * r.height();
        pts.push(egui::pos2(x, y));
    }
    pts.push(egui::pos2(r.right(), r.bottom()));
    // One mesh of vertical strips: no anti-aliasing feather, so no seams between bins.
    let mut mesh = egui::Mesh::default();
    for w in pts.windows(2).skip(1).take(256) {
        let (a, b) = (w[0], w[1]);
        let i = mesh.vertices.len() as u32;
        for q in [egui::pos2(a.x, r.bottom()), a, b, egui::pos2(b.x, r.bottom())] {
            mesh.colored_vertex(q, color);
        }
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i, i + 2, i + 3);
    }
    p.add(mesh);
}

/// Display colour of channel `name` (curve, histogram and gradient bars).
pub fn channel_color(name: &str, t: &Tokens) -> Color32 {
    match name {
        "red" => Color32::from_rgb(235, 80, 80),
        "green" => Color32::from_rgb(80, 205, 95),
        "blue" => Color32::from_rgb(90, 140, 255),
        "cyan" => Color32::from_rgb(40, 200, 230),
        "magenta" => Color32::from_rgb(225, 70, 200),
        "yellow" => Color32::from_rgb(235, 210, 40),
        "a" => Color32::from_rgb(220, 90, 140),
        "b" => Color32::from_rgb(200, 180, 60),
        _ => t.text,
    }
}

fn channel_picker(ui: &mut egui::Ui, id: egui::Id, label: &str) -> usize {
    let mut ch: usize = ui.data(|d| d.get_temp(id)).unwrap_or(0);
    ui.horizontal(|ui| {
        let t = Tokens::get(ui.ctx());
        ui.label(egui::RichText::new(tl!(&label)).color(t.text_dim).size(12.0));
        let opts: Vec<(usize, &str)> = CHANNELS.iter().enumerate().map(|(i, (_, l))| (i, *l)).collect();
        crate::widgets::dropdown(ui, &format!("{id:?}-ch"), &mut ch, &opts, 110.0);
    });
    ui.data_mut(|d| d.insert_temp(id, ch));
    ch
}

// ---------------------------------------------------------------------------------------------
// Histogram panel

/// Histogram panel (Window › Histogram): whole-image histogram with Photoshop's statistics.
/// Recomputed at most every 250 ms while the document keeps changing (e.g. during painting).
pub fn histogram_panel(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let Some(st) = app.session.active() else {
        ui.label(egui::RichText::new(tl!("No document")).color(t.text_faint));
        return;
    };
    let (doc_id, rev) = (st.doc.id, st.revision);
    let want = tag(HistSource::BelowLayer(LayerId(u64::MAX)), ToneSpace::Rgb, HistScope::Selection, selection_key(&st.doc));
    let now = crate::gpu_canvas::now_ms();
    // A selection change moves no pixels but does change what the histogram counts, so only a
    // cache computed for the very same selection may follow a view-only revision (#125).
    if view_only(st)
        && let Some((d, r, _, _, h)) = app.doc_hist.as_mut()
        && *d == doc_id
        && *r + 1 == rev
        && h.tag == want
    {
        *r = rev;
    }
    let stale = !matches!(&app.doc_hist, Some((d, r, _, _, h)) if *d == doc_id && *r == rev && h.tag == want);
    let due = app.doc_hist.as_ref().is_none_or(|(d, _, at, _, _)| *d != doc_id || now - at > 250.0);
    if stale && due {
        // Computed off the UI thread (doc clone + proxy + pixel counts cost tens of ms on
        // large documents): the panel keeps showing the previous histogram with the existing
        // "cached data" warning until the worker delivers.
        let pending = app.hist_job.as_ref().is_some_and(|(d, r, _)| *d == doc_id && *r == rev);
        if !pending {
            let (tx, rx) = std::sync::mpsc::channel();
            // No threads on the web (and a spawn can fail natively): compute inline then, and
            // deliver through the same channel so one receive path lands both.
            #[cfg(not(target_arch = "wasm32"))]
            let spawned = {
                let doc = st.doc.clone();
                let tx = tx.clone();
                let ctx = ui.ctx().clone();
                std::thread::Builder::new()
                    .name("histogram".into())
                    .spawn(move || {
                        let t0 = crate::gpu_canvas::now_ms();
                        let h = Arc::new(compute_histograms(&doc, HistSource::BelowLayer(LayerId(u64::MAX)), ToneSpace::Rgb, HistScope::Selection));
                        let pixels = histogram_pixels(&doc);
                        let _ = tx.send((doc.id, crate::gpu_canvas::now_ms() - t0, h, pixels));
                        ctx.request_repaint();
                    })
                    .is_ok()
            };
            #[cfg(target_arch = "wasm32")]
            let spawned = false;
            if !spawned {
                let t0 = crate::gpu_canvas::now_ms();
                let h = Arc::new(compute_histograms(&st.doc, HistSource::BelowLayer(LayerId(u64::MAX)), ToneSpace::Rgb, HistScope::Selection));
                let pixels = histogram_pixels(&st.doc);
                let _ = tx.send((doc_id, crate::gpu_canvas::now_ms() - t0, h, pixels));
            }
            app.hist_job = Some((doc_id, rev, rx));
        }
    } else if stale {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(260));
    }
    // A finished worker lands its histogram; the revision check keeps a stale result from
    // overwriting a newer one. A worker that died without sending clears the job so the next
    // frame schedules a new one.
    if let Some((jd, jr, rx)) = &app.hist_job {
        match rx.try_recv() {
            Ok((_, ms, h, pixels)) => {
                app.perf.span("histogram", ms);
                if *jd == doc_id {
                    app.doc_hist = Some((doc_id, *jr, crate::gpu_canvas::now_ms(), pixels, h));
                }
                app.hist_job = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => app.hist_job = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }
    let Some((_, _, _, pixels, h)) = app.doc_hist.clone() else { return };
    let size = app.session.active().map_or(photocraft_doc::Size::new(0, 0), |s| s.doc.size);
    // Statistics come from a downsampled cache (like Photoshop's cache levels).
    let level = (size.width.max(size.height) as f32 / 384.0).max(1.0).log2().ceil() as u32 + 1;
    let ch = channel_picker(ui, egui::Id::new("histogram-panel-ch"), "Channel:");
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 100.0), Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, 0.0, t.field);
    let color = if ch == 0 { Color32::from_gray(if t.pro { 190 } else { 70 }) } else { channel_color(CHANNELS[ch.min(3)].0, &t) };
    draw_histogram(&p, r, &h[ch], color);
    if stale {
        // Photoshop's "cached data" warning triangle while the histogram lags the image.
        crate::icons::paint(ui, Rect::from_min_size(r.right_top() + vec2(-18.0, 2.0), vec2(16.0, 16.0)), "triangle-alert", 12.0, t.warning);
    }
    let hist = &h[ch];
    let n: u64 = hist.iter().map(|v| *v as u64).sum();
    if n == 0 {
        return;
    }
    let mean = hist.iter().enumerate().map(|(i, v)| i as f64 * *v as f64).sum::<f64>() / n as f64;
    let var = hist.iter().enumerate().map(|(i, v)| (i as f64 - mean).powi(2) * *v as f64).sum::<f64>() / n as f64;
    let mut acc = 0u64;
    let median = hist.iter().position(|v| {
        acc += *v as u64;
        acc * 2 >= n
    });
    ui.add_space(4.0);
    egui::Grid::new("hist-stats").num_columns(2).spacing(vec2(12.0, 2.0)).show(ui, |ui| {
        for (k, v) in [
            (tl!("Mean:"), format!("{mean:.2}")),
            (tl!("Std Dev:"), format!("{:.2}", var.sqrt())),
            (tl!("Median:"), median.unwrap_or(0).to_string()),
            (tl!("Pixels:"), pixels.to_string()),
            (tl!("Cache Level:"), level.to_string()),
        ] {
            ui.label(egui::RichText::new(k).color(t.text_dim).size(11.5));
            ui.label(egui::RichText::new(v).font(crate::theme::mono(11.5)).color(t.text));
            ui.end_row();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn histogram_ignores_the_edited_adjustment() {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 64, "height": 64})).unwrap(); // white
        s.execute("layer.newAdjustmentLayer.invert", json!({})).unwrap();
        let st = s.active().unwrap();
        let h = compute_histograms(&st.doc, HistSource::BelowLayer(st.active_layer.unwrap()), ToneSpace::Rgb, HistScope::WholeInput);
        assert!(h[0][255] > 0 && h[0][0] == 0, "sees the white image, not the inverted result");
        assert_eq!(h[9], [0; 256], "out-of-range channels are empty, not a panic");
    }

    #[test]
    fn layer_histogram_sees_only_that_layer_and_spaces_have_their_channels() {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 32, "height": 32})).unwrap(); // white background
        let bg = s.active().unwrap().doc.layers[0].id;
        s.execute("layer.new.layer", json!({})).unwrap();
        s.execute("select.rect", json!({"x": 0, "y": 0, "width": 32, "height": 32})).unwrap();
        s.execute("edit.fill", json!({"color": "#000000"})).unwrap();
        let st = s.active().unwrap();
        let h = compute_histograms(&st.doc, HistSource::Layer(bg), ToneSpace::Rgb, HistScope::WholeInput);
        assert!(h[0][255] > 0 && h[0][0] == 0, "the black layer above is hidden");
        assert_eq!(compute_histograms(&st.doc, HistSource::Layer(bg), ToneSpace::Cmyk, HistScope::WholeInput).ch.len(), 5);
        assert_eq!(compute_histograms(&st.doc, HistSource::Layer(bg), ToneSpace::Lab, HistScope::WholeInput).ch.len(), 3);
    }

    #[test]
    fn a_selection_restricts_the_panel_histogram_but_not_the_editors() {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 32, "height": 32})).unwrap(); // white background
        s.execute("layer.new.layer", json!({})).unwrap();
        s.execute("select.rect", json!({"x": 0, "y": 0, "width": 16, "height": 32})).unwrap();
        s.execute("edit.fill", json!({"color": "#000000"})).unwrap(); // left half black
        s.execute("select.deselect", json!({})).unwrap();
        let st = s.active().unwrap();
        let whole = compute_histograms(&st.doc, HistSource::BelowLayer(LayerId(u64::MAX)), ToneSpace::Rgb, HistScope::WholeInput);
        assert!(whole[0][0] > 0 && whole[0][255] > 0, "black and white both count");
        assert_eq!(histogram_pixels(&st.doc), 32 * 32, "no selection: the whole canvas");
        s.execute("select.rect", json!({"x": 0, "y": 0, "width": 16, "height": 32})).unwrap();
        let st = s.active().unwrap();
        let left = compute_histograms(&st.doc, HistSource::BelowLayer(LayerId(u64::MAX)), ToneSpace::Rgb, HistScope::Selection);
        assert!(left[0][0] > 0 && left[0][255] == 0, "the panel counts only the selected black half");
        let editor = compute_histograms(&st.doc, HistSource::BelowLayer(LayerId(u64::MAX)), ToneSpace::Rgb, HistScope::WholeInput);
        assert!(editor[0][255] > 0, "the editors show the adjustment's whole input (Photoshop)");
        assert_eq!(histogram_pixels(&st.doc), 16 * 32, "Pixels counts the selection");
        s.execute("select.deselect", json!({})).unwrap();
        s.execute("select.rect", json!({"x": 12, "y": 0, "width": 8, "height": 32, "feather": 2})).unwrap();
        let st = s.active().unwrap();
        let soft = compute_histograms(&st.doc, HistSource::BelowLayer(LayerId(u64::MAX)), ToneSpace::Rgb, HistScope::Selection);
        assert!(soft[0][0] > 0 && soft[0][255] > 0, "a feathered mask keeps its ≥ ½ core on both sides");
    }

    #[test]
    fn the_selection_cache_key_tracks_the_selection() {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 32, "height": 32})).unwrap();
        assert_eq!(selection_key(&s.active().unwrap().doc), 0, "no selection");
        s.execute("select.rect", json!({"x": 0, "y": 0, "width": 8, "height": 8})).unwrap();
        let a = s.active().unwrap().doc.clone();
        let k = selection_key(&a);
        assert_ne!(k, 0);
        assert_eq!(selection_key(&a), k, "a clone keeps the key");
        s.execute("select.rect", json!({"x": 8, "y": 8, "width": 8, "height": 8})).unwrap();
        assert_ne!(selection_key(&s.active().unwrap().doc), k, "a new mask is a new key");
    }

    /// Two masks with no tiles at all differ only in their default (what every pixel reads as):
    /// the key must tell an all-selected mask from a none-selected one, or the panel's histogram
    /// goes stale between them.
    #[test]
    fn tile_less_selection_masks_differ_by_their_default() {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 32, "height": 32})).unwrap();
        let mut doc = s.active().unwrap().doc.as_ref().clone();
        doc.selection = Some(photocraft_raster::Surface::with_default(photocraft_color::PixelFormat::GRAY8, &[1.0]));
        let all = selection_key(&doc);
        doc.selection = Some(photocraft_raster::Surface::with_default(photocraft_color::PixelFormat::GRAY8, &[0.0]));
        let none = selection_key(&doc);
        assert_ne!(all, 0);
        assert_ne!(none, 0, "a mask selecting nothing is still a selection, not `None`");
        assert_ne!(all, none, "tile-less masks differ by their default");
    }

    /// The key's tile identities are pointers, so two masks may only hash alike if a write kept a
    /// tile's pointer while changing its bytes. Commands edit a staged clone whose tiles are
    /// shared until written, and a COW write allocates a new tile (`raster` pins that). This test
    /// pins the consequence the cache relies on: masks differing only in tile *content* — same
    /// default, same bounds, same tile coordinates — get different keys.
    #[test]
    fn selection_key_distinguishes_content_only_changes() {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 64, "height": 64})).unwrap();
        let mut doc = s.active().unwrap().doc.as_ref().clone();
        let mut mask = photocraft_raster::Surface::with_default(photocraft_color::PixelFormat::GRAY8, &[0.0]);
        mask.try_write_region(photocraft_geom::Rect::new(0, 0, 64, 64), &vec![1.0; 64 * 64]).unwrap();
        doc.selection = Some(mask.clone());
        let before = selection_key(&doc);
        // A staged edit writes through the shared tiles: same default, same bounds, same tile
        // coordinates, different content in the first quarter only.
        let mut staged = doc.clone();
        staged.selection.as_mut().unwrap().try_write_region(photocraft_geom::Rect::new(0, 0, 16, 64), &vec![0.5; 16 * 64]).unwrap();
        let after = selection_key(&staged);
        assert_ne!(before, 0);
        assert_ne!(after, 0);
        assert_ne!(before, after, "a content-only mask change must change the key");
    }

    /// #5335: the Curves histogram of a targeted gray plane describes that plane, not the
    /// colour layers beneath it: a channel filled with 25% grey over a black layer shows a
    /// spike at 25% grey, not at black.
    #[test]
    fn channel_histogram_describes_the_gray_plane() {
        use photocraft_engine::channel_cmds::GraySurface;
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 32, "height": 32})).unwrap(); // white background
        s.execute("layer.new.layer", json!({})).unwrap();
        s.execute("select.rect", json!({"x": 0, "y": 0, "width": 32, "height": 32})).unwrap();
        s.execute("edit.fill", json!({"color": "#000000"})).unwrap();
        s.execute("channel.new", json!({"name": "Sky sel", "fill": "white"})).unwrap();
        s.execute("layer.layerMask.revealAll", json!({})).unwrap();
        let b = photocraft_geom::Rect::from_xywh(0, 0, 32, 32);
        s.edit("fill-planes", |doc, active| {
            doc.channels[0].surface.fill_rect(b, &[0.25]);
            doc.layer_mut(active.unwrap()).unwrap().mask.as_mut().unwrap().surface.fill_rect(b, &[0.0]);
            Ok(())
        })
        .unwrap();
        let st = s.active().unwrap();
        let layer = st.active_layer.unwrap();
        let h = compute_histograms(&st.doc, HistSource::Channel { layer, surface: GraySurface::Alpha(0) }, ToneSpace::Rgb, HistScope::WholeInput);
        assert!(h[0][64] > 0, "a spike at 25% grey, got {:?}", &h[0][0..80]);
        assert_eq!(h[0][0], 0, "not the black layer beneath");
        let m = compute_histograms(&st.doc, HistSource::Channel { layer, surface: GraySurface::LayerMask }, ToneSpace::Rgb, HistScope::WholeInput);
        assert!(m[0][0] > 0, "the hidden mask is black");
        // Each plane keeps its own cache entry: switching targets recomputes.
        assert_ne!(
            tag(HistSource::Channel { layer, surface: GraySurface::Alpha(0) }, ToneSpace::Rgb, HistScope::WholeInput, 0),
            tag(HistSource::Channel { layer, surface: GraySurface::LayerMask }, ToneSpace::Rgb, HistScope::WholeInput, 0)
        );
        assert_ne!(
            tag(HistSource::Channel { layer, surface: GraySurface::LayerMask }, ToneSpace::Rgb, HistScope::WholeInput, 0),
            tag(HistSource::Layer(layer), ToneSpace::Rgb, HistScope::WholeInput, 0)
        );
    }
}

#[cfg(test)]
mod hist_async_tests {
    use super::*;
    use crate::PhotocraftApp;
    use serde_json::json;

    /// The panel schedules the histogram on a worker and keeps showing the previous data
    /// until it lands: right after the trigger the job exists (nothing computed in this
    /// frame), the receiver delivers, and a stale result cannot overwrite a newer revision.
    #[test]
    fn panel_histogram_is_computed_off_the_ui_thread() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        let ctx = egui::Context::default();
        let _ = &ctx;
        app.run("file.new", json!({"width": 640, "height": 480})).unwrap();
        let (doc_id, rev) = {
            let st = app.session.active().unwrap();
            (st.doc.id, st.revision)
        };
        // Drive one panel frame through a minimal egui pass.
        let ctx = egui::Context::default();
        ctx.begin_pass(Default::default());
        let panel_ui = egui::Ui::new(
            ctx.clone(),
            egui::Id::new("hist-test"),
            egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 200.0))),
        );
        let mut panel_ui = panel_ui;
        histogram_panel(&mut app, &mut panel_ui);
        // A fast worker (or the inline fallback) may already have landed the result this frame.
        if let Some(job) = app.hist_job.take() {
            assert_eq!(job.0, doc_id);
            assert_eq!(job.1, rev);
            // The worker delivers (its repaint request is a no-op outside a frame).
            let (did, _, h, pixels) = job.2.recv().expect("the worker delivers the histogram");
            assert_eq!(did, doc_id);
            assert!(!h.ch.is_empty(), "the histogram has data");
            assert_eq!(pixels, 640 * 480, "no selection: every canvas pixel is counted");
            // Delivering it for the revision it was computed from lands in the cache.
            app.doc_hist = Some((doc_id, rev, crate::gpu_canvas::now_ms(), pixels, h));
        }
        assert_eq!(app.doc_hist.as_ref().map(|h| (h.0, h.1)), Some((doc_id, rev)));
        // …and a newer revision keeps the old entry stale instead of being overwritten by it.
        app.run("layer.new.layer", json!({})).unwrap();
        let rev2 = app.session.active().unwrap().revision;
        assert_ne!(rev, rev2);
        assert_ne!(app.doc_hist.as_ref().unwrap().1, rev2, "stale after an edit");
    }

    /// A worker that died without sending (its sender dropped) clears the job, so the next
    /// frame schedules a new one instead of waiting forever.
    #[test]
    fn a_dead_histogram_worker_clears_the_job() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 64, "height": 48})).unwrap();
        let (doc_id, rev) = {
            let st = app.session.active().unwrap();
            (st.doc.id, st.revision)
        };
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        app.hist_job = Some((doc_id, rev, rx));
        let ctx = egui::Context::default();
        ctx.begin_pass(Default::default());
        let mut ui = egui::Ui::new(
            ctx.clone(),
            egui::Id::new("hist-dead"),
            egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 200.0))),
        );
        histogram_panel(&mut app, &mut ui);
        assert!(app.hist_job.is_none(), "a disconnected job is dropped");
        assert!(app.doc_hist.is_none(), "nothing landed from the dead worker");
    }

    /// Drives one panel frame and delivers whatever worker it scheduled (like the real panel
    /// receiving a repaint after the worker finishes).
    fn frame(app: &mut PhotocraftApp, id: &str) {
        let ctx = egui::Context::default();
        ctx.begin_pass(Default::default());
        let mut ui =
            egui::Ui::new(ctx, egui::Id::new(id), egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 400.0))));
        histogram_panel(app, &mut ui);
        if let Some((d, r, rx)) = app.hist_job.take() {
            let (did, _, h, pixels) = rx.recv().expect("the worker delivers");
            if did == d {
                app.doc_hist = Some((did, r, crate::gpu_canvas::now_ms(), pixels, h));
            }
        }
    }

    fn age_cache(app: &mut PhotocraftApp) {
        if let Some((_, _, at, _, _)) = app.doc_hist.as_mut() {
            *at -= 1000.0; // let the 250 ms throttle pass so the next stale frame recomputes
        }
    }

    /// A selection changes no pixels, but the histogram counts different ones: the panel must
    /// recompute for the selection instead of riding the #125 view-only fast path, and its
    /// Pixels statistic reports the selection's size.
    #[test]
    fn the_panel_histogram_follows_the_selection() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 640, "height": 480})).unwrap();
        frame(&mut app, "hist-follow-1");
        let whole = app.doc_hist.clone().expect("landed");
        assert_eq!(whole.3, 640 * 480);
        app.run("select.rect", json!({"x": 0, "y": 0, "width": 20, "height": 10})).unwrap();
        age_cache(&mut app);
        frame(&mut app, "hist-follow-2");
        let selected = app.doc_hist.clone().expect("landed");
        assert_eq!(selected.1, app.session.active().unwrap().revision, "fresh for the selection revision");
        assert_eq!(selected.3, 200, "Pixels counts the selection");
        assert_ne!(selected.4.tag, whole.4.tag, "the selection changed the histogram's identity");
        // A later view-only edit (no selection change) keeps the selection histogram.
        app.run("layer.new.layer", json!({})).unwrap();
        age_cache(&mut app);
        frame(&mut app, "hist-follow-3");
        let after = app.doc_hist.clone().unwrap();
        assert_eq!(after.4.tag, selected.4.tag, "same selection, same histogram");
        assert_eq!(after.3, 200);
    }

    /// The tone editors' histogram shows the adjustment's whole input (Photoshop's dialogs are
    /// not selection-masked), so a selection change — view-only, no pixels moved — keeps the
    /// cached histogram (#125). The panel's masked histogram is covered above.
    #[test]
    fn the_tone_histogram_cache_ignores_selection_changes() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 64, "height": 64})).unwrap();
        let src = HistSource::BelowLayer(LayerId(u64::MAX));
        let first = histograms(&mut app, src, ToneSpace::Rgb);
        assert!(Arc::ptr_eq(&first, &histograms(&mut app, src, ToneSpace::Rgb)), "unchanged state reuses the cache");
        app.run("select.rect", json!({"x": 0, "y": 0, "width": 8, "height": 8})).unwrap();
        assert!(Arc::ptr_eq(&first, &histograms(&mut app, src, ToneSpace::Rgb)), "a selection change is view-only for the whole-input histogram");
        app.run("select.deselect", json!({})).unwrap();
        assert!(Arc::ptr_eq(&first, &histograms(&mut app, src, ToneSpace::Rgb)), "deselecting keeps it too");
    }
}
