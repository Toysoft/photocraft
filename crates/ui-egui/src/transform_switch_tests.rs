//! Free Transform with the whole app: picking another tool or layer, or New Layer, while the box
//! is open applies the transform as it stands, as Photoshop does (#5441, #5665, #5667).

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use crate::PhotocraftApp;

/// The app with a 400 × 300 document: a red 100 × 60 rectangle at (100, 100) on its own layer
/// above the Background.
fn app() -> Harness<'static, PhotocraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).with_max_steps(64).build_eframe(|cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 400, "height": 300, "background": "white"})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("select.rect", json!({"x": 100, "y": 100, "width": 100, "height": 60})).unwrap();
        app.run("edit.fill", json!({"color": "#ff0000"})).unwrap();
        app.run("select.deselect", json!({})).unwrap();
        app
    });
    h.run_steps(6);
    h
}

/// Open Free Transform and move the box 50 px right, 40 px down.
fn transform_and_move(h: &mut Harness<'static, PhotocraftApp>) {
    let ctx = h.ctx.clone();
    crate::menus::invoke(h.state_mut(), &ctx, "edit.freeTransform", json!({})).unwrap();
    h.run_steps(2);
    let t = h.state_mut().ui.transform.as_mut().expect("Free Transform is open");
    for p in &mut t.quad {
        p[0] += 50.0;
        p[1] += 40.0;
    }
    h.run_steps(2);
}

fn red_bounds(h: &Harness<'static, PhotocraftApp>) -> photocraft_geom::Rect {
    let st = h.state().session.active().unwrap();
    let top = st.doc.layers.last().unwrap();
    top.surface().unwrap().content_bounds()
}

#[test]
fn picking_another_tool_in_the_toolbar_applies_the_transform() {
    let mut h = app();
    transform_and_move(&mut h);
    let hand = h.query_by_label_contains("Hand Tool").expect("the toolbar's Hand Tool");
    hand.click();
    h.run_steps(4);
    assert!(h.state().ui.transform.is_none(), "the box closed");
    assert_eq!(red_bounds(&h), photocraft_geom::Rect::new(150, 140, 250, 200), "the move was applied, not cancelled");
}

/// The box on a just-placed layer (a file dropped on the canvas), moved.
fn placed_and_moved(h: &mut Harness<'static, PhotocraftApp>) {
    let ctx = h.ctx.clone();
    crate::transform_tool::begin_placed(h.state_mut(), &ctx).unwrap();
    h.run_steps(2);
    let t = h.state_mut().ui.transform.as_mut().expect("Free Transform is open");
    for p in &mut t.quad {
        p[0] += 50.0;
        p[1] += 40.0;
    }
    h.run_steps(2);
}

#[test]
fn a_placed_image_is_applied_when_another_tool_is_picked() {
    let mut h = app();
    placed_and_moved(&mut h);
    h.query_by_label_contains("Hand Tool").expect("the toolbar's Hand Tool").click();
    h.run_steps(4);
    assert!(h.state().ui.transform.is_none());
    assert_eq!(h.state().session.active().unwrap().doc.layers.len(), 2, "the placed layer stays");
    assert_eq!(red_bounds(&h), photocraft_geom::Rect::new(150, 140, 250, 200));
}

#[test]
fn picking_another_layer_in_the_panel_applies_the_transform() {
    for placed in [false, true] {
        let mut h = app();
        if placed {
            placed_and_moved(&mut h);
        } else {
            transform_and_move(&mut h);
        }
        h.get_by_label("Background").click();
        h.run_steps(4);
        assert!(h.state().ui.transform.is_none(), "placed {placed}");
        assert_eq!(h.state().session.active().unwrap().doc.layers.len(), 2, "placed {placed}");
        assert_eq!(red_bounds(&h), photocraft_geom::Rect::new(150, 140, 250, 200), "placed {placed}");
    }
}

#[test]
fn new_layer_applies_the_transform_first() {
    for placed in [false, true] {
        let mut h = app();
        if placed {
            placed_and_moved(&mut h);
        } else {
            transform_and_move(&mut h);
        }
        // The Layers panel's New Layer button (the menu item opens the New Layer dialog).
        h.state_mut().run("layer.new.layer", json!({})).unwrap();
        h.run_steps(4);
        assert!(h.state().ui.transform.is_none(), "placed {placed}");
        let st = h.state().session.active().unwrap();
        assert_eq!(st.doc.layers.len(), 3, "placed {placed}");
        let red = st.doc.layers[1].surface().unwrap().content_bounds();
        assert_eq!(red, photocraft_geom::Rect::new(150, 140, 250, 200), "placed {placed}: the moved image, not the original");
    }
}

#[test]
fn an_untouched_placed_image_stays_when_another_tool_or_layer_is_picked() {
    for via_layer in [false, true] {
        let mut h = app();
        let ctx = h.ctx.clone();
        crate::transform_tool::begin_placed(h.state_mut(), &ctx).unwrap();
        h.run_steps(2);
        if via_layer {
            let bg = h.state().session.active().unwrap().doc.layers[0].id;
            h.state_mut().run("layer.select", json!({"layer": bg.0})).unwrap();
        } else {
            h.query_by_label_contains("Hand Tool").expect("Hand Tool").click();
        }
        h.run_steps(4);
        let st = h.state().session.active().unwrap();
        assert_eq!(st.doc.layers.len(), 2, "via layer {via_layer}: the placed image stays");
        assert_eq!(red_bounds(&h), photocraft_geom::Rect::new(100, 100, 200, 160));
    }
}
