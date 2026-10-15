//! Channel transforms preview through the normal compositor and current channel eyes.
//! The captured proxy is immutable; geometry is resampled only when it changes, never per frame.

use std::sync::Arc;

use egui::{Color32, ColorImage, TextureHandle, TextureOptions};
use photocraft_doc::{Document, LayerId};
use photocraft_engine::channel_cmds::ChannelView;
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::state::TransformSession;

pub(crate) struct Preview {
    base: Arc<Document>,
    layer: Option<LayerId>,
    background: [f32; 4],
    factor: u32,
    geometry: Value,
    shown: Arc<Document>,
    view: Option<(ChannelView, bool, u64)>,
    texture: Option<TextureHandle>,
    gpu: Option<crate::channel_transform_gpu::Preview>,
    gpu_attempted: bool,
}

impl Preview {
    pub(crate) fn new(doc: &Document, layer: Option<LayerId>, background: [f32; 4], reduced: bool) -> Self {
        // Effects and vector masks need document-space geometry, so retain full resolution there.
        let factor = if photocraft_compose::proxy::proxy_faithful(doc) { crate::proxy::preview_factor(doc, reduced) } else { 1 };
        let base = Arc::new(crate::proxy::proxy_document(doc, factor));
        Self { shown: base.clone(), base, layer, background, factor, geometry: Value::Null, view: None, texture: None, gpu: None, gpu_attempted: false }
    }

    pub(crate) fn background(&self) -> [f32; 4] {
        self.background
    }

    fn update(&mut self, t: &TransformSession) -> Result<(), String> {
        let k = f64::from(self.factor);
        let mut params = json!({"target": t.target, "rect": t.rect.map(|v| v / k), "interpolation": t.interpolation});
        if let Some(layer) = self.layer {
            params["layer"] = json!(layer.0);
        }
        let command = if let Some(warp) = &t.warp {
            let mut warp = warp.clone();
            warp.bounds = warp.bounds.map(|v| v / k);
            warp.mesh = warp.mesh.map(|m| m.map_points(|p| [p[0] / k, p[1] / k]));
            params["warp"] = json!(warp);
            "edit.transform.warp"
        } else {
            params["quad"] = json!(t.quad.map(|p| p.map(|v| v / k)));
            "edit.transform"
        };
        if self.geometry == params {
            return Ok(());
        }
        let mut session = photocraft_engine::Session::new();
        session.tools.background = self.background;
        session.add_document((*self.base).clone(), None);
        if let Some(layer) = self.layer {
            session.select_layer(layer).map_err(|e| e.to_string())?;
        }
        session.execute(command, params.clone()).map_err(|e| e.to_string())?;
        self.shown = session.active().ok_or("no channel preview document")?.doc.clone();
        self.geometry = params;
        self.view = None;
        Ok(())
    }

    fn image(&self, view: &ChannelView, in_color: bool, display: Option<&photocraft_engine::display_color::CanvasDisplay>) -> ColorImage {
        let overlay = match crate::channel_view::render(&self.shown, view, self.shown.bounds(), 1, in_color, self.layer) {
            // Solo/partial colour views already contain the opaque composite. Do not render it twice.
            Some(pixels) if pixels.iter().all(|p| p.a() == 255) => {
                return ColorImage::new([self.shown.size.width as usize, self.shown.size.height as usize], pixels);
            }
            overlay => overlay,
        };
        let buffer = photocraft_compose::flatten(&self.shown);
        let mut image = crate::canvas::display_image(display, &buffer);
        let pixels = &mut image.pixels;
        if let Some(overlay) = overlay {
            for (base, over) in pixels.iter_mut().zip(overlay) {
                let weight = u32::from(255 - over.a());
                let b = base.to_array();
                let o = over.to_array();
                let result = std::array::from_fn::<_, 4, _>(|i| (u32::from(o[i]) + (u32::from(b[i]) * weight + 127) / 255).min(255) as u8);
                *base = Color32::from_rgba_premultiplied(result[0], result[1], result[2], result[3]);
            }
        }
        image
    }
}

/// Keep uploads within the renderer's limit, including very long, narrow documents.
fn limit_image(image: ColorImage, max_side: usize) -> ColorImage {
    let k = image.size.into_iter().max().unwrap_or(1).div_ceil(max_side.max(1)).max(1);
    if k == 1 {
        return image;
    }
    let size = image.size.map(|v| v.div_ceil(k));
    let pixels = image.pixels.chunks(image.size[0].max(1)).step_by(k).flat_map(|row| row.iter().step_by(k).copied()).collect();
    ColorImage::new(size, pixels)
}

/// Current preview texture: the eyes affect rendering only, never the captured source or target.
pub(crate) fn texture(app: &mut PhotocraftApp, ctx: &egui::Context, idx: usize) -> Option<egui::TextureId> {
    if app.session.active_index() != Some(idx) {
        return None;
    }
    let (display, display_key) = crate::canvas::canvas_display(app, &app.session.documents().get(idx)?.doc, None);
    let t = app.ui.transform.as_ref()?;
    let pv = app.transform_preview.as_mut()?;
    let preview = pv.channel.as_mut()?;
    let st = app.session.documents().get(idx)?;
    if st.doc.id != pv.doc.id {
        return None;
    }
    let view = st.channel_view.clone();
    let in_color = app.session.prefs().interface.show_channels_in_color;
    // Colour mixing is performed on the native layer, before any display conversion. Other
    // modes/stacks and non-identity monitor transforms retain the CPU compositor oracle.
    let fast_view = display.as_ref().is_none_or(|d| d.transform.is_none())
        && view.shown_layer_mask(&preview.base).is_none()
        && (0..preview.base.channels.len()).all(|i| !view.alpha_shown(i))
        && (preview.base.quick_mask.is_none() || view.quick_mask_hidden);
    if let Some(gpu) = &app.gpu
        && gpu.health().is_ok()
        && fast_view
        && t.target.as_ref().is_some_and(|t| t.get("color").is_some())
    {
        if !preview.gpu_attempted {
            preview.gpu = crate::channel_transform_gpu::Preview::new(gpu, &preview.base, preview.layer, preview.background);
            preview.gpu_attempted = true;
        }
        if let Some(tex) = preview.gpu.as_mut().and_then(|g| g.texture(t, &view, in_color, display.as_ref().is_some_and(|d| d.encode_srgb), preview.factor)) {
            return Some(tex);
        }
    }
    if let Err(error) = preview.update(t) {
        app.ui.status = error;
        app.ui.status_error = true;
        return preview.texture.as_ref().map(TextureHandle::id);
    }
    if preview.view.as_ref() != Some(&(view.clone(), in_color, display_key)) {
        let image = limit_image(preview.image(&view, in_color, display.as_deref()), ctx.input(|i| i.max_texture_side));
        match preview.texture.as_mut() {
            Some(texture) => texture.set(image, TextureOptions::LINEAR),
            None => preview.texture = Some(ctx.load_texture(format!("channel-transform-{}", t.session), image, TextureOptions::LINEAR)),
        }
        preview.view = Some((view, in_color, display_key));
    }
    preview.texture.as_ref().map(TextureHandle::id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::{Color, ColorMode, SampleType};
    use photocraft_doc::{LayerMask, Size};
    use photocraft_engine::{Session, channel_cmds::ChannelTarget};
    use photocraft_geom::{
        Rect,
        warp::{BezierMesh, Warp},
    };

    // These oracles each submit thousands of readbacks. Overlapping their independent devices
    // with the rest of the UI suite can crash the native driver at default test parallelism.
    static GPU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn gpu_pixel_stretch_matches_native_channel_commit_across_tile_edges() {
        let _gpu_test = GPU_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(rs) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            egui_kittest::wgpu::create_render_state(crate::gpu_canvas::wgpu_setup(), Default::default())
        })) else {
            eprintln!("no GPU adapter for channel preview readback");
            return;
        };
        let gpu = crate::gpu_canvas::GpuCanvas::new(&rs);
        for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
            for selected in [false, true] {
                let mut doc = Document::with_background("channel pixel stretch", Size::new(320, 24), ColorMode::Rgb, depth, Color::WHITE);
                doc.layers[0].surface_mut().unwrap().fill_rect(Rect::new(250, 4, 251, 20), &[0.2, 0.6, 0.8, 0.75]);
                if selected {
                    let mut selection = photocraft_raster::Surface::new(photocraft_color::PixelFormat::GRAY8);
                    selection.fill_rect(Rect::new(250, 4, 251, 20), &[1.0]);
                    doc.selection = Some(selection);
                }
                let mut a = app();
                a.session.add_document(doc, None);
                a.run("channel.target", json!({"channel":"red"})).unwrap();
                a.run("channel.setVisible", json!({"channel":"composite","visible":true})).unwrap();
                crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap();
                let st = a.session.active().unwrap();
                let doc = &st.doc;
                let mut fast = crate::channel_transform_gpu::Preview::new(&gpu, doc, st.active_layer, [1.0; 4]).unwrap();
                for interpolation in ["bilinear", "bicubic"] {
                    for perspective in [false, true] {
                        let mut t = a.ui.transform.clone().unwrap();
                        t.interpolation = interpolation.into();
                        t.quad = t.quad.map(|[x, y]| [250.0 + (x - 250.0) * 16.0, y]);
                        if perspective {
                            t.quad[2][0] += 0.75;
                            t.quad[2][1] += 2.0;
                        }
                        assert!(fast.texture(&t, &st.channel_view, false, false, 1).is_some(), "stretch stays on the cached GPU path");
                        let actual = fast.readback();
                        let mut oracle = Preview::new(doc, st.active_layer, [1.0; 4], false);
                        oracle.update(&t).unwrap();
                        let expected = oracle.image(&st.channel_view, false, None);
                        let max = actual
                            .iter()
                            .zip(&expected.pixels)
                            .flat_map(|(a, b)| a.to_array().into_iter().zip(b.to_array()).map(|(a, b)| a.abs_diff(b)))
                            .max()
                            .unwrap();
                        assert!(max <= 2, "{depth:?} selected={selected} {interpolation} perspective={perspective}: {max}");
                        let before = doc.layers[0].surface().unwrap();
                        let after = oracle.shown.layers[0].surface().unwrap();
                        for y in 0..24 {
                            for x in 0..320 {
                                for channel in [1, 2, 3] {
                                    assert_eq!(after.sample_channel(x, y, channel), before.sample_channel(x, y, channel));
                                }
                            }
                        }
                        if !perspective {
                            for x in 250..266 {
                                assert!((after.sample_channel(x, 12, 0) - 0.2).abs() < 0.005, "stretched column stays solid at {x}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn gpu_sparse_and_high_frequency_channels_match_the_cpu_or_fall_back() {
        let _gpu_test = GPU_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(rs) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            egui_kittest::wgpu::create_render_state(crate::gpu_canvas::wgpu_setup(), Default::default())
        })) else {
            eprintln!("no GPU adapter for channel preview readback");
            return;
        };
        let gpu = crate::gpu_canvas::GpuCanvas::new(&rs);
        for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
            for pattern in 0..2 {
                for selection in 0..3 {
                    let mut doc = Document::with_background("scalar edge oracle", Size::new(32, 24), ColorMode::Rgb, depth, Color::WHITE);
                    let s = doc.layers[0].surface_mut().unwrap();
                    if pattern == 0 {
                        s.fill_rect(Rect::new(8, 8, 16, 16), &[0.5, 1.0, 1.0, 1.0]);
                    } else {
                        for y in 0..24 {
                            for x in 0..32 {
                                s.write_pixel(x, y, &[((x + y) % 2) as f32, 1.0, 1.0, 1.0]);
                            }
                        }
                    }
                    if selection != 0 {
                        let mut sel = photocraft_raster::Surface::new(photocraft_color::PixelFormat::new(ColorMode::Grayscale, SampleType::F32, false));
                        sel.fill_rect(doc.bounds(), &[1.0]);
                        if selection == 2 {
                            sel.fill_rect(Rect::new(0, 0, 16, 24), &[0.5]);
                        }
                        doc.selection = Some(sel);
                    }
                    let mut a = app();
                    a.session.add_document(doc, None);
                    a.run("channel.target", json!({"channel":"red"})).unwrap();
                    a.run("channel.setVisible", json!({"channel":"composite","visible":true})).unwrap();
                    crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap();
                    let st = a.session.active().unwrap();
                    let doc = &st.doc;
                    let fill = [1.0; 4];
                    let mut fast = crate::channel_transform_gpu::Preview::new(&gpu, doc, st.active_layer, fill).unwrap();
                    for interpolation in ["bilinear", "bicubic"] {
                        for geometry in 0..10 {
                            let mut t = a.ui.transform.clone().unwrap();
                            t.interpolation = interpolation.into();
                            let factor = if geometry == 4 { 3 } else { 1 };
                            match geometry {
                                0 => t.quad = t.quad.map(|[x, y]| [x + 0.25, y + 0.25]),
                                1 | 2 => {
                                    let scale = if geometry == 1 { 0.6 } else { 0.75 };
                                    t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] * scale, p[1] * scale]), t.rect));
                                }
                                3 => t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0], p[1] + 0.02 * p[0] * p[0]]), t.rect)),
                                4 => {
                                    // An integer full-resolution selection becomes a fractional
                                    // proxy frame. CPU lifting encloses its boundary pixels.
                                    t.rect = [1.0, 1.0, 95.0, 71.0];
                                    let [x0, y0, x1, y1] = t.rect;
                                    t.quad = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
                                    t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] + 0.75, p[1] + 0.75]), t.rect));
                                }
                                5 => {
                                    t.quad = t.quad.map(|[x, y]| [x + 3.0, y + 2.0]);
                                    t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1), t.rect));
                                }
                                8 | 9 => {
                                    let scale = if geometry == 8 { 0.75 } else { 0.998 };
                                    t.quad = t.quad.map(|[x, y]| [x * scale, y * scale]);
                                }
                                _ => {
                                    t.rect = if geometry == 6 { [-4.0, -4.0, 36.0, 28.0] } else { [4.0, 4.0, 28.0, 20.0] };
                                    let [x0, y0, x1, y1] = t.rect;
                                    t.quad = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
                                    t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] + 0.25, p[1] + 0.25]), t.rect));
                                }
                            }
                            let texture = fast.texture(&t, &st.channel_view, false, false, factor);
                            if geometry == 1
                                || (geometry == 3 && texture.is_none())
                                || (geometry == 6 && selection == 0)
                                || geometry == 7
                                || (geometry >= 8 && interpolation == "bicubic")
                            {
                                assert!(texture.is_none(), "shrinking or explicitly clipped Warp needs the CPU oracle");
                                continue;
                            }
                            texture.unwrap();
                            let actual = fast.readback();
                            let mut oracle = Preview::new(doc, st.active_layer, fill, false);
                            oracle.factor = factor;
                            oracle.update(&t).unwrap();
                            let expected = oracle.image(&st.channel_view, false, None);
                            let max = actual
                                .iter()
                                .zip(&expected.pixels)
                                .flat_map(|(a, b)| a.to_array().into_iter().zip(b.to_array()).map(|(a, b)| a.abs_diff(b)))
                                .max()
                                .unwrap();
                            assert!(max <= 2, "{depth:?}/{pattern}/{selection}/{interpolation}/{geometry}: max error {max}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn gpu_stretched_thin_channel_warp_matches_native_sampling() {
        let _gpu_test = GPU_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let rs = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            egui_kittest::wgpu::create_render_state(crate::gpu_canvas::wgpu_setup(), Default::default())
        }));
        let Ok(rs) = rs else {
            eprintln!("no GPU adapter for stretched channel Warp readback");
            return;
        };
        let gpu = crate::gpu_canvas::GpuCanvas::new(&rs);
        for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
            for selected in [false, true] {
                let mut doc = Document::with_background("thin channel Warp", Size::new(128, 128), ColorMode::Rgb, depth, Color::rgba(0.0, 0.4, 0.6, 0.75));
                doc.layers[0].surface_mut().unwrap().fill_rect(Rect::new(40, 20, 43, 60), &[0.8, 0.4, 0.6, 0.75]);
                if selected {
                    let mut sel = photocraft_raster::Surface::new(photocraft_color::PixelFormat::new(ColorMode::Grayscale, SampleType::F32, false));
                    sel.fill_rect(Rect::new(40, 20, 43, 60), &[1.0]);
                    sel.fill_rect(Rect::new(40, 20, 41, 60), &[0.5]);
                    doc.selection = Some(sel);
                }
                let mut session = Session::new();
                session.tools.background = [0.0, 0.0, 0.0, 1.0];
                session.add_document(doc, None);
                let mut a = PhotocraftApp::new(session, crate::Services::default());
                a.run("channel.target", json!({"channel":"red"})).unwrap();
                a.run("channel.setVisible", json!({"channel":"composite","visible":true})).unwrap();
                crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap();
                let st = a.session.active().unwrap();
                let doc = &st.doc;
                let fill = [0.0, 0.0, 0.0, 1.0];
                let mut fast = crate::channel_transform_gpu::Preview::new(&gpu, doc, st.active_layer, fill).unwrap();
                for factor in [1, 2] {
                    for offset in [0.0, 0.125] {
                        for interpolation in ["bilinear", "bicubic"] {
                            let mut t = a.ui.transform.clone().unwrap();
                            let k = f64::from(factor);
                            // The 3 px source becomes a 90 px band with a 30 px sag. A grid sized
                            // only by the source has one cell across and loses the entire curve.
                            // Factor 2 also checks the map in reduced-preview coordinates.
                            t.rect = [40.0, 20.0, 43.0, 60.0].map(|v| v * k);
                            let mesh = BezierMesh::identity(t.rect, 1, 1).map_points(|p| {
                                let u = (p[0] / k - 40.0) / 3.0;
                                [k * (41.5 + (p[0] / k - 41.5) * 30.0 + offset), p[1] + k * 180.0 * u * (1.0 - u)]
                            });
                            t.warp = Some(Warp::custom(mesh, t.rect));
                            t.interpolation = interpolation.into();
                            let texture = fast.texture(&t, &st.channel_view, false, false, factor);
                            let mut oracle = Preview::new(doc, st.active_layer, fill, false);
                            oracle.factor = factor;
                            oracle.update(&t).unwrap();
                            let expected = oracle.image(&st.channel_view, false, None);
                            if selected && offset != 0.0 {
                                texture.expect("unfolded selected thin Warp keeps the GPU path");
                                let actual = fast.readback();
                                let max = actual
                                    .iter()
                                    .zip(&expected.pixels)
                                    .flat_map(|(a, b)| a.to_array().into_iter().zip(b.to_array()).map(|(a, b)| a.abs_diff(b)))
                                    .max()
                                    .unwrap();
                                assert!(max <= 2, "{depth:?}, selected={selected}, factor={factor}, offset={offset}, {interpolation}: max error {max}");
                            } else {
                                // An explicit frame smaller than the canvas has clipped coverage;
                                // the existing CPU fallback must remain in force without a selection.
                                assert!(texture.is_none(), "clipped frames and pixel-centre edge ties keep the CPU oracle");
                            }
                            let shown = oracle.shown.layers[0].surface().unwrap();
                            assert!(shown.sample_channel(41, 70, 0) > 0.6, "the middle follows the sag");
                            assert!(shown.sample_channel(41, 20, 0) < 0.01, "the old straight band is cleared");
                            for y in 0..128 {
                                for x in 0..128 {
                                    for c in [1, 2, 3] {
                                        assert_eq!(shown.sample_channel(x, y, c).to_bits(), doc.layers[0].surface().unwrap().sample_channel(x, y, c).to_bits());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn gpu_colour_preview_matches_cpu_for_depth_selection_geometry_and_eyes() {
        let _gpu_test = GPU_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let rs = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            egui_kittest::wgpu::create_render_state(crate::gpu_canvas::wgpu_setup(), Default::default())
        }));
        let Ok(rs) = rs else {
            eprintln!("no GPU adapter for channel preview readback");
            return;
        };
        let gpu = crate::gpu_canvas::GpuCanvas::new(&rs);
        eprintln!("channel preview adapter: {:?}", rs.adapter.get_info());
        for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
            for selection in 0..3 {
                let mut a = app();
                let mut doc = Document::with_background("GPU oracle", Size::new(32, 24), ColorMode::Rgb, depth, Color::WHITE);
                doc.layers[0].opacity = 0.7;
                let s = doc.layers[0].surface_mut().unwrap();
                for y in 0..24 {
                    for x in 0..32 {
                        s.write_pixel(x, y, &[0.1 + x as f32 / 40.0, 0.1 + y as f32 / 32.0, 0.3, 0.75]);
                    }
                }
                if depth == SampleType::F32 {
                    s.fill_rect(Rect::new(10, 8, 14, 12), &[2.0, -0.2, 0.4, 0.8]);
                }
                if selection == 1 {
                    let mut sel = photocraft_raster::Surface::new(photocraft_color::PixelFormat::new(ColorMode::Grayscale, SampleType::F32, false));
                    sel.fill_rect(Rect::new(4, 4, 24, 20), &[0.5]);
                    sel.fill_rect(Rect::new(8, 8, 20, 16), &[1.0]);
                    doc.selection = Some(sel);
                }
                if selection == 2 {
                    let mut sel = photocraft_raster::Surface::new(photocraft_color::PixelFormat::GRAY8);
                    sel.fill_rect(doc.bounds(), &[1.0]);
                    doc.selection = Some(sel);
                }
                a.session.add_document(doc, None);
                a.run("channel.target", json!({"channel":"green"})).unwrap();
                crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap();
                let st = a.session.active().unwrap();
                let doc = st.doc.clone();
                let layer = st.active_layer;
                let (display, _) = crate::canvas::canvas_display(&a, &doc, None);
                assert!(display.as_ref().unwrap().transform.is_none());
                let encode = display.as_ref().is_some_and(|d| d.encode_srgb);
                let fill = [0.15, 0.35, 0.65, 1.0];
                let mut fast = crate::channel_transform_gpu::Preview::new(&gpu, &doc, layer, fill).expect("GPU colour fast path");
                let initial = a.ui.transform.clone().unwrap();
                for (geometry, interpolation) in (0..9).flat_map(|g| ["nearest", "bilinear", "bicubic"].map(|i| (g, i))) {
                    for component in 0..3 {
                        let mut t = initial.clone();
                        t.target = Some(json!({"color":component}));
                        t.interpolation = interpolation.into();
                        match geometry {
                            1 => t.quad = t.quad.map(|[x, y]| [x + 3.0, y + 2.0]),
                            2 => t.quad = t.quad.map(|[x, y]| [x * 0.85 + 2.5, y * 0.9 + 1.25]),
                            3 => t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] + 3.0, p[1] + 2.0]), t.rect)),
                            4 => t.quad = t.quad.map(|[x, y]| [x + 0.1 * y, y + 0.05 * x]),
                            5 => t.quad = t.quad.map(|[x, y]| [0.94 * x - 0.342 * y + 4.0, 0.342 * x + 0.94 * y]),
                            6 => {
                                t.quad[2][0] += 4.0;
                                t.quad[2][1] += 2.0;
                            }
                            7 => {
                                t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0], p[1] + 0.02 * p[0] * p[0] / 32.0]), t.rect))
                            }
                            8 => {
                                let [x0, y0, x1, y1] = t.rect;
                                let (w, h) = (x1 - x0, y1 - y0);
                                t.quad = [[x0, y0], [x0 + 3.0 * w, y0], [x0 + 0.6 * w, y0 + 0.6 * h], [x0, y0 + 3.0 * h]];
                            }
                            _ => {}
                        }
                        let mut oracle = Preview::new(&doc, layer, fill, false);
                        if geometry == 8 {
                            // The scalar engine rejects unbounded perspective geometry; neither
                            // preview path may turn it into a wrapped or mirrored sample.
                            let before = oracle.shown.clone();
                            assert!(oracle.update(&t).unwrap_err().contains("perspective horizon"));
                            assert!(Arc::ptr_eq(&before, &oracle.shown));
                            assert!(fast.texture(&t, &st.channel_view, false, encode, 1).is_none());
                            continue;
                        }
                        oracle.update(&t).unwrap();
                        for visible in 0..8 {
                            let mut view = st.channel_view.clone();
                            // Exercise public eye commands rather than depending on the view representation.
                            let mut session = Session::new();
                            session.add_document((*doc).clone(), None);
                            session.execute("channel.target", json!({"channel":"green"})).unwrap();
                            session.active_mut().unwrap().channel_transform = st.channel_transform.clone();
                            for (i, name) in ["red", "green", "blue"].iter().enumerate() {
                                session.execute("channel.setVisible", json!({"channel":name,"visible": visible & (1<<i) != 0})).unwrap();
                            }
                            view.clone_from(&session.active().unwrap().channel_view);
                            for in_color in [false, true] {
                                let texture = fast.texture(&t, &view, in_color, encode, 1);
                                if (interpolation == "nearest" && geometry > 1)
                                    || (interpolation == "bicubic" && matches!(geometry, 2 | 4))
                                    || (geometry == 7 && selection != 1)
                                {
                                    assert!(texture.is_none(), "fractional nearest ties, Bicubic reductions and ambiguous curved outlines use the CPU oracle");
                                    continue;
                                }
                                texture.unwrap();
                                let actual = fast.readback();
                                let expected = oracle.image(&view, in_color, display.as_deref());
                                let max = actual
                                    .iter()
                                    .zip(&expected.pixels)
                                    .flat_map(|(a, b)| a.to_array().into_iter().zip(b.to_array()).map(|(a, b)| a.abs_diff(b)))
                                    .max()
                                    .unwrap();
                                if max > 2 {
                                    let (at, (got, want)) = actual
                                        .iter()
                                        .zip(&expected.pixels)
                                        .enumerate()
                                        .max_by_key(|(_, (a, b))| a.to_array().into_iter().zip(b.to_array()).map(|(a, b)| a.abs_diff(b)).max())
                                        .unwrap();
                                    eprintln!("worst pixel {at}: {got:?} != {want:?}; view={view:?}");
                                }
                                assert!(
                                    max <= 2,
                                    "{depth:?} selection={selection} geometry={geometry} interpolation={interpolation} component={component} eyes={visible} colour={in_color}: max error {max}"
                                );
                            }
                        }
                    }
                }
                assert!(gpu.health().is_ok(), "{:?}", gpu.fault());
            }
        }
        let mut doc = Document::with_background("allocation guard", Size::new(4, 4), ColorMode::Rgb, SampleType::U8, Color::WHITE);
        let layer = Some(doc.layers[0].id);
        gpu.set_memory_budget(1);
        assert!(crate::channel_transform_gpu::Preview::new(&gpu, &doc, layer, [1.0; 4]).is_none());
        gpu.set_memory_budget(256 * 1024 * 1024);
        doc.size = Size::new(u32::MAX, u32::MAX);
        assert!(crate::channel_transform_gpu::Preview::new(&gpu, &doc, layer, [1.0; 4]).is_none());
        doc.size = Size::new(4, 4);
        for coverage in [2.0, f32::NAN, f32::INFINITY] {
            let mut selection = photocraft_raster::Surface::new(photocraft_color::PixelFormat::new(ColorMode::Grayscale, SampleType::F32, false));
            selection.fill_rect(doc.bounds(), &[coverage]);
            doc.selection = Some(selection);
            assert!(crate::channel_transform_gpu::Preview::new(&gpu, &doc, layer, [1.0; 4]).is_none());
        }
        let mut outside = photocraft_raster::Surface::new(photocraft_color::PixelFormat::GRAY8);
        outside.fill_rect(Rect::new(-2, 0, 4, 4), &[1.0]);
        doc.selection = Some(outside);
        assert!(crate::channel_transform_gpu::Preview::new(&gpu, &doc, layer, [1.0; 4]).is_none(), "off-canvas selected defaults use the CPU oracle");
        doc.selection = Some(photocraft_raster::Surface::with_default(photocraft_color::PixelFormat::GRAY8, &[1.0]));
        assert!(crate::channel_transform_gpu::Preview::new(&gpu, &doc, layer, [1.0; 4]).is_none(), "unbounded selection defaults use the CPU oracle");
        doc.selection = None;
        gpu.health().mark(photocraft_gpu::Fault::Lost("synthetic loss".into()));
        assert!(crate::channel_transform_gpu::Preview::new(&gpu, &doc, layer, [1.0; 4]).is_none());
    }

    fn app() -> PhotocraftApp {
        let mut session = Session::new();
        let mut doc = Document::with_background("channel preview", Size::new(32, 24), ColorMode::Rgb, SampleType::F32, Color::WHITE);
        let s = doc.layers[0].surface_mut().unwrap();
        s.fill_rect(Rect::new(0, 0, 32, 24), &[0.2, 0.4, 0.6, 0.75]);
        s.fill_rect(Rect::new(4, 4, 16, 16), &[0.8, 0.1, 0.3, 0.5]);
        session.add_document(doc, None);
        let mut app = PhotocraftApp::new(session, crate::Services::default());
        app.sync_views();
        app
    }

    #[test]
    fn layer_style_cannot_implicitly_finish_an_isolated_channel_transform() {
        let mut a = app();
        a.run("channel.target", json!({"channel":"green"})).unwrap();
        let ctx = egui::Context::default();
        crate::transform_tool::begin(&mut a, &ctx).unwrap();
        let before = a.session.active().unwrap().doc.clone();
        let history = a.session.active().unwrap().history.entries();
        let captured = a.ui.transform.clone();
        assert!(crate::layer_style::open(&mut a, Some("dropShadow")).is_none());
        assert!(a.ui.status.contains(photocraft_engine::channel_transform::LOCKED));
        assert_eq!(a.ui.transform, captured);
        assert!(Arc::ptr_eq(&before, &a.session.active().unwrap().doc));
        assert_eq!(a.session.active().unwrap().history.entries(), history);
        assert!(photocraft_engine::channel_transform::locked(&a.session));
        crate::transform_tool::cancel(&mut a);
        assert!(crate::layer_style::open(&mut a, Some("dropShadow")).is_some());
    }

    #[test]
    fn narrow_preview_obeys_texture_limit() {
        let image = ColorImage::new([9000, 2], vec![Color32::WHITE; 18000]);
        let result = limit_image(image, 4096);
        assert_eq!(result.size, [3000, 1]);
        assert_eq!(result.pixels.len(), 3000);
    }

    #[test]
    fn visible_mask_overlay_never_overrides_the_channels_colour_target() {
        for warp in [false, true] {
            let mut a = app();
            a.run("layer.layerMask.revealAll", json!({})).unwrap();
            a.run("view.layerMask", json!({"mode":"overlay"})).unwrap();
            a.run("channel.target", json!({"channel":"green"})).unwrap();
            a.ui.mask_target = true; // A Layers thumbnail or a stale UI target must not win.
            let before = a.session.active().unwrap().doc.clone();
            let view = a.session.active().unwrap().channel_view.clone();
            let ctx = egui::Context::default();
            crate::transform_tool::begin(&mut a, &ctx).unwrap();
            assert_eq!(a.ui.transform.as_ref().unwrap().target, Some(json!({"color":1})));
            assert_eq!(a.session.active().unwrap().channel_view, view);
            let t = a.ui.transform.as_mut().unwrap();
            t.quad = t.quad.map(|[x, y]| [x + 3.0, y]);
            if warp {
                t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] + 3.0, p[1]]), t.rect));
            }
            let t = a.ui.transform.clone().unwrap();
            let pv = a.transform_preview.as_mut().unwrap().channel.as_mut().unwrap();
            pv.update(&t).unwrap();
            let shown = pv.shown.clone();
            crate::transform_tool::commit(&mut a);
            assert!(!a.ui.status_error, "{}", a.ui.status);
            let after = &a.session.active().unwrap().doc;
            assert_eq!(after.layers[0].mask, before.layers[0].mask);
            assert_eq!(after.layers, shown.layers);
            assert_ne!(after.layers[0].surface(), before.layers[0].surface());
            for y in 0..24 {
                for x in 0..32 {
                    for k in [0, 2, 3] {
                        assert_eq!(after.layers[0].surface().unwrap().sample_channel(x, y, k), before.layers[0].surface().unwrap().sample_channel(x, y, k));
                    }
                }
            }
        }
    }

    #[test]
    fn inline_warp_grid_and_splits_work_for_every_captured_scalar_target() {
        for target in [json!("green"), json!("mask"), json!(0), json!("quickMask")] {
            let mut a = app();
            match target.as_str() {
                Some("mask") => {
                    a.run("layer.layerMask.revealAll", json!({})).unwrap();
                }
                Some("quickMask") => {
                    a.run("select.editInQuickMaskMode", json!({"on":true})).unwrap();
                }
                _ if target == 0 => {
                    a.run("channel.new", json!({})).unwrap();
                }
                _ => {}
            }
            if target != "quickMask" {
                a.run("channel.target", json!({"channel":target})).unwrap();
            }
            if target == "mask" {
                a.run("view.layerMask", json!({"mode":"overlay"})).unwrap();
            }
            if target == 0 {
                a.session.active_mut().unwrap().active_layer = None;
            }
            let before = a.session.active().unwrap().doc.clone();
            let ctx = egui::Context::default();
            crate::transform_tool::begin(&mut a, &ctx).unwrap();
            crate::transform_tool::enter_warp(&mut a, &ctx);
            let captured = a.ui.transform.as_ref().unwrap().target.clone();
            crate::transform_tool::edit_session_warp(&mut a, "edit.transform.warpGrid", &json!({"size":3})).unwrap();
            assert_eq!(a.ui.transform.as_ref().unwrap().warp.as_ref().unwrap().mesh.as_ref().unwrap().us.len(), 4);
            for id in [
                "edit.transform.splitWarpCrosswise",
                "edit.transform.splitWarpHorizontally",
                "edit.transform.splitWarpVertically",
                "edit.transform.removeWarpSplit",
            ] {
                crate::transform_tool::edit_session_warp(&mut a, id, &json!({})).unwrap();
                assert_eq!(a.ui.transform.as_ref().unwrap().target, captured);
                assert!(a.session.active().unwrap().channel_transform.is_some());
                assert!(Arc::ptr_eq(&before, &a.session.active().unwrap().doc));
            }
            crate::transform_tool::cancel(&mut a);
            assert!(a.session.active().unwrap().channel_transform.is_none());
        }
    }

    #[test]
    fn cmyk_preview_and_commit_share_the_embedded_background_conversion() {
        for warp in [false, true] {
            let mut session = Session::new();
            let mut doc = Document::with_background("CMYK preview", Size::new(32, 24), ColorMode::Cmyk, SampleType::F32, Color::WHITE);
            doc.icc_profile = Some(
                photocraft_cms::synth::cmyk_profile(&photocraft_cms::synth::CmykParams {
                    description: "Preview Uncoated".into(),
                    tvi: [0.26, 0.26, 0.26, 0.3],
                    grid_a2b: 5,
                    grid_b2a: 9,
                    ..Default::default()
                })
                .to_bytes(),
            );
            let bounds = doc.bounds();
            doc.layers[0].surface_mut().unwrap().fill_rect(bounds, &[0.1, 0.2, 0.3, 0.4, 1.0]);
            session.tools.background = [0.8, 0.2, 0.4, 1.0];
            let space = photocraft_compose::cmyk_space(&doc).unwrap();
            let expected =
                photocraft_color::convert::with_cmyk_space(Some(&space), || photocraft_raster::from_rgba(&doc.pixel_format(), session.tools.background))[1];
            session.add_document(doc, None);
            let mut a = PhotocraftApp::new(session, crate::Services::default());
            a.run("channel.target", json!({"channel":{"color":1}})).unwrap();
            crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap();
            let t = a.ui.transform.as_mut().unwrap();
            t.quad = t.quad.map(|[x, y]| [x + 3.0, y]);
            if warp {
                t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] + 3.0, p[1]]), t.rect));
            }
            let t = a.ui.transform.clone().unwrap();
            let pv = a.transform_preview.as_mut().unwrap().channel.as_mut().unwrap();
            pv.update(&t).unwrap();
            let shown = pv.shown.clone();
            assert!((shown.layers[0].surface().unwrap().sample_channel(0, 0, 1) - expected).abs() < 1e-5);
            crate::transform_tool::commit(&mut a);
            assert!(!a.ui.status_error, "{}", a.ui.status);
            assert_eq!(a.session.active().unwrap().doc.layers, shown.layers);
        }
    }

    #[test]
    fn reduced_warp_preview_preserves_samples_outside_a_fractional_selection_frame() {
        let mut session = Session::new();
        let mut doc = Document::with_background("Reduced Warp", Size::new(2400, 2400), ColorMode::Rgb, SampleType::U8, Color::WHITE);
        doc.layers[0].surface_mut().unwrap().fill_rect(Rect::new(0, 0, 2400, 2400), &[0.2, 0.4, 0.6, 1.0]);
        session.add_document(doc, None);
        let mut a = PhotocraftApp::new(session, crate::Services::default());
        a.run("channel.target", json!({"channel":"green"})).unwrap();
        a.run("select.rect", json!({"x":101,"y":101,"width":100,"height":100})).unwrap();
        crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap();
        let t = a.ui.transform.as_mut().unwrap();
        t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] + 4.0, p[1]]), t.rect));
        let t = a.ui.transform.clone().unwrap();
        let pv = a.transform_preview.as_mut().unwrap().channel.as_mut().unwrap();
        assert_eq!(pv.factor, 2);
        pv.update(&t).unwrap();
        let shown = pv.shown.layers[0].surface().unwrap().clone();
        let original = pv.base.layers[0].surface().unwrap();
        assert_eq!(shown.pixel(10, 10), original.pixel(10, 10));
        assert!((shown.sample_channel(51, 70, 1) - 1.0).abs() < 0.01, "vacated channel uses the captured background");
        assert!((shown.sample_channel(80, 70, 1) - 0.4).abs() < 0.01);
        crate::transform_tool::commit(&mut a);
        assert!(!a.ui.status_error, "{}", a.ui.status);
        let committed = crate::proxy::downsample(a.session.active().unwrap().doc.layers[0].surface().unwrap(), 2);
        for (x, y) in [(10, 10), (51, 70), (80, 70), (104, 70)] {
            assert_eq!(shown.pixel(x, y), committed.pixel(x, y), "proxy and full-resolution commit at ({x},{y})");
        }
    }

    #[test]
    fn quick_mask_preview_scales_once_with_the_document() {
        for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
            for (size, reduced, factor) in [(Size::new(3000, 2000), false, 1), (Size::new(3000, 2000), true, 2), (Size::new(6000, 4000), true, 4)] {
                let mut doc = Document::new("Quick Mask proxy", size, ColorMode::Rgb, depth);
                let mut mask = photocraft_raster::Surface::new(photocraft_color::PixelFormat::new(ColorMode::Grayscale, depth, false));
                mask.fill_rect(Rect::new(400, 400, 800, 800), &[1.0]);
                mask.fill_rect(Rect::new(480, 480, 720, 720), &[0.5]);
                doc.quick_mask = Some(photocraft_doc::AlphaChannel::new("Quick Mask", mask));
                let original = doc.quick_mask.clone();
                let preview = Preview::new(&doc, None, [1.0; 4], reduced);
                assert_eq!(preview.factor, factor);
                assert_eq!(preview.base.size, Size::new(size.width / factor, size.height / factor));
                let mask = &preview.base.quick_mask.as_ref().unwrap().surface;
                assert_eq!(mask.format().sample, depth);
                let k = factor as i32;
                assert_eq!(mask.content_bounds(), Rect::new(400 / k, 400 / k, 800 / k, 800 / k));
                assert_eq!(
                    mask.to_interleaved(mask.content_bounds()),
                    crate::proxy::downsample(&original.as_ref().unwrap().surface, factor).to_interleaved(mask.content_bounds())
                );
                assert_eq!(doc.quick_mask, original, "capturing a proxy must not change the source");
            }
        }
    }

    #[test]
    fn reduced_quick_mask_affine_and_warp_previews_match_commit_and_leave_cancel_unchanged() {
        for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
            for (size, factor) in [(Size::new(3000, 2000), 2), (Size::new(6000, 4000), 4)] {
                for warp in [false, true] {
                    for commit in [false, true] {
                        let mut doc = Document::new("Reduced Quick Mask transform", size, ColorMode::Rgb, depth);
                        doc.layers.push(photocraft_doc::Layer::raster("pixels", doc.pixel_format()));
                        let mut mask = photocraft_raster::Surface::new(photocraft_color::PixelFormat::new(ColorMode::Grayscale, depth, false));
                        mask.fill_rect(Rect::new(400, 400, 800, 800), &[1.0]);
                        mask.fill_rect(Rect::new(480, 480, 720, 720), &[0.5]);
                        doc.quick_mask = Some(photocraft_doc::AlphaChannel::new("Quick Mask", mask));
                        let mut session = Session::new();
                        session.add_document(doc, None);
                        let mut a = PhotocraftApp::new(session, crate::Services::default());
                        let before = a.session.active().unwrap().doc.clone();
                        let eyes = a.session.active().unwrap().channel_view.clone();
                        crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap();
                        assert_eq!(a.session.active().unwrap().channel_view, eyes);
                        let t = a.ui.transform.as_mut().unwrap();
                        assert_eq!(t.target, Some(json!("quickMask")));
                        t.quad = t.quad.map(|[x, y]| [x + 40.0, y + 24.0]);
                        if warp {
                            t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] + 40.0, p[1] + 24.0]), t.rect));
                        }
                        let t = a.ui.transform.clone().unwrap();
                        let pv = a.transform_preview.as_mut().unwrap().channel.as_mut().unwrap();
                        assert_eq!(pv.factor, factor);
                        pv.update(&t).unwrap();
                        let shown = pv.shown.quick_mask.as_ref().unwrap().surface.clone();
                        let k = factor as i32;
                        assert_eq!(shown.content_bounds(), Rect::new(440 / k, 424 / k, 840 / k, 824 / k));
                        assert!(Arc::ptr_eq(&before, &a.session.active().unwrap().doc), "preview must not edit document pixels");
                        if commit {
                            crate::transform_tool::commit(&mut a);
                            assert!(!a.ui.status_error, "{}", a.ui.status);
                            let after = a.session.active().unwrap().doc.clone();
                            let full = &after.quick_mask.as_ref().unwrap().surface;
                            assert_eq!(full.content_bounds(), Rect::new(440, 424, 840, 824));
                            let committed = crate::proxy::downsample(full, factor);
                            let area = Rect::new(0, 0, 900 / k, 900 / k);
                            assert_eq!(shown.to_interleaved(area), committed.to_interleaved(area), "{depth:?}, factor {factor}, warp {warp}");
                            assert_eq!(after.layers, before.layers);
                            a.run("edit.undo", json!({})).unwrap();
                            assert_eq!(a.session.active().unwrap().doc.quick_mask, before.quick_mask);
                            a.run("edit.redo", json!({})).unwrap();
                            assert_eq!(a.session.active().unwrap().doc.quick_mask.as_ref().unwrap().surface, *full);
                        } else {
                            crate::transform_tool::cancel(&mut a);
                            assert!(Arc::ptr_eq(&before, &a.session.active().unwrap().doc));
                        }
                        assert_eq!(a.session.active().unwrap().channel_view, eyes);
                        assert!(!photocraft_engine::channel_transform::locked(&a.session));
                    }
                }
            }
        }
    }

    #[test]
    fn hidden_target_refuses_to_start_without_changing_eyes() {
        for target in [json!("red"), json!(0), json!("mask"), json!("composite")] {
            let mut a = app();
            if target == 0 {
                a.run("channel.new", json!({})).unwrap();
            }
            if target == "mask" {
                a.run("layer.layerMask.revealAll", json!({})).unwrap();
            }
            if target == "composite" {
                a.run("select.editInQuickMaskMode", json!({"on":true})).unwrap();
            }
            a.run("channel.target", json!({"channel":target})).unwrap();
            if target == "red" {
                a.run("channel.setVisible", json!({"channel":"green","visible":true})).unwrap();
            }
            if target == "mask" {
                a.run("view.layerMask", json!({"mode":"off"})).unwrap();
            } else {
                a.run("channel.setVisible", json!({"channel": if target=="composite" { json!("quickMask") } else { target.clone() },"visible":false})).unwrap();
            }
            let before = a.session.active().unwrap().channel_view.clone();
            assert!(
                crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap_err().contains(photocraft_engine::channel_transform::HIDDEN),
                "{target}"
            );
            assert_eq!(a.session.active().unwrap().channel_view, before);
            assert!(a.session.active().unwrap().channel_transform.is_none());
            assert!(a.ui.transform.is_none());
        }
    }

    #[test]
    fn eyes_recompose_captured_preview_and_persist_through_commit_cancel_and_undo() {
        for selection in 0..4 {
            for warp in [false, true] {
                for commit in [false, true] {
                    let mut a = app();
                    if selection == 1 {
                        a.run("select.all", json!({})).unwrap();
                    }
                    if selection >= 2 {
                        a.run("select.rect", json!({"x":4,"y":4,"width":12,"height":12})).unwrap();
                    }
                    if selection == 3 {
                        a.run("select.modify.feather", json!({"radius":1})).unwrap();
                    }
                    a.run("channel.new", json!({"name":"overlay"})).unwrap();
                    a.run("channel.target", json!({"channel":"red"})).unwrap();
                    a.run("channel.setVisible", json!({"channel":"green","visible":true})).unwrap();
                    a.run("channel.setVisible", json!({"channel":0,"visible":true})).unwrap();
                    let before = a.session.active().unwrap().doc.clone();
                    let view = a.session.active().unwrap().channel_view.clone();
                    let history = a.session.active().unwrap().history.past_len();
                    crate::transform_tool::begin(&mut a, &egui::Context::default()).unwrap();
                    assert_eq!(a.session.active().unwrap().channel_view, view, "start preserves every eye");
                    let t = a.ui.transform.as_mut().unwrap();
                    t.quad = t.quad.map(|[x, y]| [x + 3.0, y]);
                    if warp {
                        t.warp = Some(Warp::custom(BezierMesh::identity(t.rect, 1, 1).map_points(|p| [p[0] + 3.0, p[1]]), t.rect));
                    }
                    let t = a.ui.transform.clone().unwrap();
                    let pv = a.transform_preview.as_mut().unwrap().channel.as_mut().unwrap();
                    pv.update(&t).unwrap();
                    let shown = pv.shown.clone();
                    let expected_image = pv.image(&view, false, None);
                    for (cmd, p) in [
                        ("channel.target", json!({"channel":"green"})),
                        ("layer.select", json!({"layer":t.layer})),
                        ("channel.delete", json!({"channel":0})),
                        ("layer.delete", json!({})),
                        ("select.editInQuickMaskMode", json!({})),
                        ("history.selectState", json!({"state":0})),
                    ] {
                        assert!(a.run(cmd, p).is_err(), "{cmd}");
                        assert_eq!(a.ui.transform.as_ref().unwrap().quad, t.quad);
                    }
                    let ctx = egui::Context::default();
                    let (req, _) = crate::control::ControlRequest::new("ui.set", json!({"maskTarget":true}));
                    let crate::control::Outcome::Done(reply) = crate::control::handle(&mut a, &ctx, &req) else { panic!("immediate reply") };
                    assert_eq!(reply["ok"], false);
                    a.run("channel.setVisible", json!({"channel":"red","visible":false})).unwrap();
                    assert!(a.ui.transform.is_some(), "hiding target keeps handles active");
                    let hidden = a.session.active().unwrap().channel_view.clone();
                    let pv = a.transform_preview.as_mut().unwrap().channel.as_mut().unwrap();
                    pv.update(&t).unwrap();
                    assert!(Arc::ptr_eq(&shown, &pv.shown), "eyes do not resample or change captured geometry");
                    assert_ne!(pv.image(&hidden, false, None).pixels, expected_image.pixels);
                    a.run("channel.setVisible", json!({"channel":"red","visible":true})).unwrap();
                    let restored = a.session.active().unwrap().channel_view.clone();
                    assert_eq!(a.transform_preview.as_ref().unwrap().channel.as_ref().unwrap().image(&restored, false, None).pixels, expected_image.pixels);
                    a.run("channel.setVisible", json!({"channel":"composite","visible":false})).unwrap();
                    a.run("channel.setVisible", json!({"channel":0,"visible":false})).unwrap();
                    let final_view = a.session.active().unwrap().channel_view.clone();
                    assert_eq!(final_view.visible_colors(3), 0);
                    assert_eq!(a.session.active().unwrap().channel_view.target, ChannelTarget::Color(0));
                    assert!(Arc::ptr_eq(&before, &a.session.active().unwrap().doc), "live preview never edits the document");
                    if commit {
                        crate::transform_tool::commit(&mut a);
                    } else {
                        crate::transform_tool::cancel(&mut a);
                    }
                    assert!(!a.ui.status_error, "{}", a.ui.status);
                    let st = a.session.active().unwrap();
                    assert!(st.channel_transform.is_none());
                    assert_eq!(st.channel_view, final_view);
                    assert_eq!(st.history.past_len(), history + usize::from(commit));
                    if commit {
                        assert_eq!(st.doc.layers[0].surface(), shown.layers[0].surface(), "native preview and commit agree");
                        a.session.undo();
                        assert_eq!(a.session.active().unwrap().doc.layers[0].surface(), before.layers[0].surface());
                        assert_eq!(a.session.active().unwrap().channel_view, final_view, "undo does not undo eyes");
                        a.session.redo();
                        assert_eq!(a.session.active().unwrap().doc.layers[0].surface(), shown.layers[0].surface());
                    } else {
                        assert!(Arc::ptr_eq(&before, &st.doc));
                    }
                    a.run("channel.target", json!({"channel":"blue"})).unwrap();
                }
            }
        }
    }

    #[test]
    fn failed_commit_and_document_switch_release_target_and_preserve_pixels() {
        let mut a = app();
        a.run("channel.target", json!({"channel":"red"})).unwrap();
        let before = a.session.active().unwrap().doc.clone();
        let ctx = egui::Context::default();
        crate::transform_tool::begin(&mut a, &ctx).unwrap();
        a.ui.transform.as_mut().unwrap().quad = [[0.0; 2]; 4];
        a.session.tools.background = [0.25, 0.5, 0.75, 1.0];
        crate::transform_tool::commit(&mut a);
        assert!(a.ui.status_error);
        assert!(a.session.active().unwrap().channel_transform.is_none());
        assert!(Arc::ptr_eq(&before, &a.session.active().unwrap().doc));
        assert_eq!(a.session.tools.background, [0.25, 0.5, 0.75, 1.0]);
        crate::transform_tool::begin(&mut a, &ctx).unwrap();
        a.session.add_document(Document::new("other", Size::new(8, 8), ColorMode::Rgb, SampleType::U8), None);
        crate::transform_tool::end_if_left(&mut a);
        assert!(a.ui.transform.is_none());
        assert!(a.session.documents().iter().all(|st| st.channel_transform.is_none()));
        assert!(Arc::ptr_eq(&before, &a.session.documents()[0].doc));
    }

    #[test]
    fn targeted_mask_can_hide_reappear_and_warp_without_retargeting() {
        let mut a = app();
        a.session
            .edit("mask", |doc, active| {
                let mut mask = LayerMask::reveal_all();
                mask.surface.fill_rect(Rect::new(4, 4, 16, 16), &[0.0]);
                doc.layer_mut(active.unwrap()).unwrap().mask = Some(mask);
                Ok(())
            })
            .unwrap();
        a.run("channel.target", json!({"channel":"mask"})).unwrap();
        a.run("view.layerMask", json!({"mode":"overlay"})).unwrap();
        let ctx = egui::Context::default();
        crate::transform_tool::begin(&mut a, &ctx).unwrap();
        crate::transform_tool::enter_warp(&mut a, &ctx);
        let t = a.ui.transform.clone().unwrap();
        a.run("view.layerMask", json!({"mode":"off"})).unwrap();
        a.sync_mask_targets();
        assert!(a.ui.mask_target);
        assert_eq!(a.ui.transform.as_ref().unwrap().target, t.target);
        assert!(a.ui.transform.as_ref().unwrap().warp.is_some());
        a.run("view.layerMask", json!({"mode":"overlay"})).unwrap();
        crate::transform_tool::cancel(&mut a);
        assert!(photocraft_engine::mask_view_cmds::current(a.session.active().unwrap()).is_some());
    }

    #[test]
    fn every_nonmask_eye_reveals_its_channel_during_a_grayscale_mask_transform() {
        for channel in [json!("composite"), json!("green"), json!(0), json!("quickMask")] {
            let mut a = app();
            a.run("channel.new", json!({})).unwrap();
            a.run("channel.target.composite", json!({})).unwrap();
            a.run("select.editInQuickMaskMode", json!({"on":true})).unwrap();
            a.run("layer.layerMask.revealAll", json!({})).unwrap();
            a.run("channel.target", json!({"channel":"mask"})).unwrap();
            a.run("view.layerMask", json!({"mode":"gray"})).unwrap();
            let ctx = egui::Context::default();
            crate::transform_tool::begin(&mut a, &ctx).unwrap();
            let t = a.ui.transform.clone().unwrap();
            let before = a.session.active().unwrap().doc.clone();
            let eyes = a.session.active().unwrap().channel_view.clone();
            assert!(a.run("channel.setVisible", json!({"channel":"mask","visible":true})).is_err());
            assert_eq!(a.session.active().unwrap().channel_view, eyes, "invalid eyes cannot change the mask view");
            a.run("channel.setVisible", json!({"channel":channel,"visible":true})).unwrap();
            let st = a.session.active().unwrap();
            assert_eq!(photocraft_engine::mask_view_cmds::current(st).unwrap().mode, photocraft_engine::mask_view_cmds::MaskViewMode::Overlay);
            assert_eq!(st.channel_view.target, ChannelTarget::LayerMask);
            assert_eq!(a.ui.transform.as_ref().unwrap().target, t.target);
            assert!(Arc::ptr_eq(&before, &st.doc));
            let final_eyes = st.channel_view.clone();
            crate::transform_tool::cancel(&mut a);
            assert_eq!(a.session.active().unwrap().channel_view, final_eyes);
        }
    }
}
