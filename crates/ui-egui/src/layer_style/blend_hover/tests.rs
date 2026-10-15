//! #5945: hovering a mode in a Layer Style Blend Mode list previews it on the canvas, as the
//! Layers panel's list does; only a choice edits the dialog, and only OK edits the document.

use std::sync::Arc;

use egui::accesskit::Role;
use egui::{Key, PointerButton, Pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use photocraft_color::BlendMode;
use photocraft_doc::{Document, Effect, LayerId};

use super::*;
use crate::layer_style::{self, KINDS, P, add_instance, spec};
use crate::state::DialogKind;
use crate::{canvas, dialogs};

/// A grey background and a grey square on layer `top` (active).
fn session() -> (photocraft_engine::Session, LayerId) {
    let mut s = photocraft_engine::Session::new();
    s.execute("prefs.set", json!({"path": "interface.language", "value": "en"})).unwrap();
    s.execute("file.new", json!({"width": 32, "height": 24, "background": "#808080"})).unwrap();
    let top = s.execute("layer.new.layer", json!({"name": "top"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("select.rect", json!({"x": 4, "y": 4, "width": 16, "height": 12})).unwrap();
    s.execute("edit.fill", json!({"color": "#808080"})).unwrap();
    s.execute("select.deselect", json!({})).unwrap();
    (s, LayerId(top))
}

fn app() -> (PhotocraftApp, LayerId) {
    let (s, top) = session();
    (PhotocraftApp::new(s, crate::Services::default()), top)
}

fn fields(app: &PhotocraftApp, id: u64) -> Map<String, Value> {
    app.ui.dialogs.iter().find(|d| d.id == id).unwrap().fields.clone()
}

fn edit(app: &mut PhotocraftApp, id: u64, edit: impl FnOnce(&mut Map<String, Value>)) {
    edit(&mut app.ui.dialog_mut(id).unwrap().fields);
}

fn selected(app: &PhotocraftApp, id: u64) -> String {
    fields(app, id)["selected"].as_str().unwrap().into()
}

/// What the Layers panel's dropdown would report for page `page` of dialog `id`.
fn hover_on(app: &mut PhotocraftApp, id: u64, page: &str, mode: Option<&str>) {
    hover(app, &egui::Context::default(), id, page, mode.map(|m| ("blend", m.to_string())));
}

/// The canvas's document and key.
fn shown(app: &mut PhotocraftApp) -> (Arc<Document>, u64) {
    canvas::display_doc(app, 0)
}

/// The preview of dialog `id`'s pending fields with `change` made to them.
fn expected(app: &PhotocraftApp, id: u64, change: impl FnOnce(&mut Map<String, Value>)) -> Document {
    let mut f = fields(app, id);
    change(&mut f);
    let st = app.session.active().unwrap();
    layer_style::preview_document(&st.doc, &app.session.patterns, &f).unwrap()
}

/// The blend mode of each of `layer`'s effects (Bevel & Emboss has none).
fn blends(doc: &Document, layer: LayerId) -> Vec<Option<BlendMode>> {
    let blend = |e: &Effect| match e {
        Effect::DropShadow(s) | Effect::InnerShadow(s) => Some(s.common.blend),
        Effect::OuterGlow(g) | Effect::InnerGlow(g) => Some(g.common.blend),
        Effect::Stroke(s) => Some(s.common.blend),
        Effect::Satin(s) => Some(s.common.blend),
        Effect::ColorOverlay { common, .. } | Effect::GradientOverlay { common, .. } | Effect::PatternOverlay { common, .. } => Some(common.blend),
        Effect::BevelEmboss(_) => None,
    };
    doc.layer(layer).unwrap().effects.items.iter().map(blend).collect()
}

/// The document, its history length, revision and dirty flag.
struct Untouched(Arc<Document>, usize, u64, bool);

impl Untouched {
    fn of(app: &PhotocraftApp) -> Self {
        let st = app.session.active().unwrap();
        Untouched(st.doc.clone(), st.history.past_len(), st.revision, st.is_dirty())
    }

    fn check(&self, app: &PhotocraftApp, what: &str) {
        let st = app.session.active().unwrap();
        assert!(Arc::ptr_eq(&st.doc, &self.0), "{what}: the document is untouched");
        assert_eq!((st.history.past_len(), st.revision, st.is_dirty()), (self.1, self.2, self.3), "{what}: no history, revision or dirty change");
    }
}

fn mode(label: &str) -> BlendMode {
    *BlendMode::LAYER_MODES.iter().find(|m| m.label() == label).unwrap()
}

#[test]
fn every_effect_blend_list_previews_the_hovered_mode_and_edits_nothing() {
    let mut kinds = 0;
    for &(kind, _) in KINDS {
        if !spec(kind).iter().any(|(_, _, p)| matches!(p, P::Blend)) {
            continue;
        }
        kinds += 1;
        let (mut app, layer) = app();
        let id = layer_style::open(&mut app, Some(kind)).unwrap();
        let effect = selected(&app, id);
        let pending = fields(&app, id);
        let before = Untouched::of(&app);
        let (_, plain) = shown(&mut app);
        let mut keys = vec![plain];
        let current = pending["effects"][0]["params"]["blend"].as_str().unwrap().to_string();
        for label in ["Darken", "Screen", "Difference", "Linear Light", "Multiply"].into_iter().filter(|m| *m != current) {
            hover_on(&mut app, id, &effect, Some(label));
            let (doc, key) = shown(&mut app);
            let want = expected(&app, id, |f| set_param(f, &effect, "blend", json!(label)));
            assert_eq!(blends(&doc, layer), vec![Some(mode(label))], "{kind}: {label} shows");
            assert_eq!(doc.layer(layer).unwrap().effects, want.layer(layer).unwrap().effects, "{kind}: {label} with the page's other settings");
            assert!(!keys.contains(&key), "{kind}: {label} renders anew");
            keys.push(key);
            let (again, same) = shown(&mut app);
            assert!(Arc::ptr_eq(&doc, &again) && key == same, "{kind}: one render per hovered mode, not one per frame");
            assert_eq!(fields(&app, id), pending, "{kind}: hovering is not an edit");
            before.check(&app, kind);
        }
    }
    assert_eq!(kinds, 9, "every effect with a Blend Mode list");
}

#[test]
fn the_hovered_overlay_mode_changes_the_composited_pixels() {
    let (mut app, _) = app();
    let id = layer_style::open(&mut app, Some("colorOverlay")).unwrap();
    let effect = selected(&app, id);
    edit(&mut app, id, |f| {
        set_param(f, &effect, "color", json!("#ff0000"));
        set_param(f, &effect, "blend", json!("Normal"));
    });
    let pixel = |app: &mut PhotocraftApp| photocraft_compose::flatten(&shown(app).0).get(8, 8);
    let normal = pixel(&mut app);
    assert!(normal[0] > 0.95 && normal[1] < 0.05, "Normal: red {normal:?}");
    hover_on(&mut app, id, &effect, Some("Multiply"));
    let multiply = pixel(&mut app);
    assert!(multiply[0] < 0.8 && multiply[0] > 0.1 && multiply[1] < 0.05, "Multiply over grey: dark red {multiply:?}");
    hover_on(&mut app, id, &effect, Some("Screen"));
    let screen = pixel(&mut app);
    assert!(screen[0] > 0.95 && screen[1] > 0.2, "Screen over grey: light red {screen:?}");
    hover_on(&mut app, id, &effect, None);
    assert_eq!(pixel(&mut app), normal, "off the list: the pending style again");
}

#[test]
fn leaving_the_list_shows_the_pending_mode_not_the_opening_one() {
    let (mut app, layer) = app();
    let id = layer_style::open(&mut app, Some("colorOverlay")).unwrap();
    let effect = selected(&app, id);
    // Multiply chosen in the dialog (pending, not applied).
    edit(&mut app, id, |f| set_param(f, &effect, "blend", json!("Multiply")));
    let before = Untouched::of(&app);
    let (_, pending) = shown(&mut app);
    for label in ["Screen", "Normal"] {
        hover_on(&mut app, id, &effect, Some(label));
        let (doc, key) = shown(&mut app);
        assert_ne!(key, pending);
        assert_eq!(blends(&doc, layer), vec![Some(mode(label))]);
    }
    // Hovering the pending mode itself, or nothing, shows the pending style from the same key.
    hover_on(&mut app, id, &effect, Some("Multiply"));
    assert_eq!(shown(&mut app).1, pending);
    hover_on(&mut app, id, &effect, Some("Screen"));
    shown(&mut app);
    hover_on(&mut app, id, &effect, None);
    let (doc, key) = shown(&mut app);
    assert_eq!(key, pending);
    assert_eq!(blends(&doc, layer), vec![Some(BlendMode::Multiply)], "back to the pending mode, not stale Screen pixels");
    assert_eq!(fields(&app, id)["effects"][0]["params"]["blend"], "Multiply");
    before.check(&app, "hover");
}

#[test]
fn the_blending_options_list_previews_the_layers_own_mode() {
    let (mut app, layer) = app();
    let id = layer_style::open(&mut app, Some("colorOverlay")).unwrap();
    edit(&mut app, id, |f| {
        f.insert("selected".into(), json!(layer_style::BLENDING));
    });
    let pending = fields(&app, id);
    let before = Untouched::of(&app);
    hover_on(&mut app, id, layer_style::BLENDING, Some("Multiply"));
    let (doc, _) = shown(&mut app);
    let l = doc.layer(layer).unwrap();
    assert_eq!(l.blend, BlendMode::Multiply, "the layer's mode shows");
    assert_eq!(blends(&doc, layer), vec![Some(BlendMode::Normal)], "its effects keep their own modes");
    let want = expected(&app, id, |f| set_param(f, layer_style::BLENDING, "blend", json!("Multiply")));
    assert_eq!(l, want.layer(layer).unwrap());
    assert_eq!(fields(&app, id), pending);
    before.check(&app, "blending options");
    assert_eq!(app.session.active().unwrap().doc.layer(layer).unwrap().blend, BlendMode::Normal);
}

#[test]
fn a_hover_belongs_to_its_dialog_page_and_instance() {
    let (mut app, layer) = app();
    let id = layer_style::open(&mut app, Some("stroke")).unwrap();
    edit(&mut app, id, |f| add_instance(f, "stroke"));
    let (first, second) = ("fx1".to_string(), selected(&app, id));
    assert_ne!(first, second);
    let (_, plain_second) = shown(&mut app);
    let normal = Some(BlendMode::Normal);
    hover_on(&mut app, id, &second, Some("Multiply"));
    assert_eq!(blends(&shown(&mut app).0, layer), vec![normal, Some(BlendMode::Multiply)], "only the hovered instance");
    // Another instance of the same kind shown: the first one's list isn't drawn any more.
    edit(&mut app, id, |f| {
        f.insert("selected".into(), json!(first));
    });
    let (doc, key) = shown(&mut app);
    assert_eq!((blends(&doc, layer), key), (vec![normal, normal], plain_second), "no hover leaks to the sibling");
    hover_on(&mut app, id, &first, Some("Screen"));
    assert_eq!(blends(&shown(&mut app).0, layer), vec![Some(BlendMode::Screen), normal]);
    // A hover reported for a page or dialog not shown previews nothing.
    for (dialog, page) in [(id, second.as_str()), (id, layer_style::BLENDING), (id + 1, first.as_str())] {
        hover_on(&mut app, dialog, page, Some("Difference"));
        let (doc, _) = shown(&mut app);
        assert_eq!(blends(&doc, layer), vec![normal, normal], "{dialog} {page}");
        assert_eq!(doc.layer(layer).unwrap().blend, BlendMode::Normal);
    }
    // A switched-off instance draws nothing whatever its mode: no new render.
    edit(&mut app, id, |f| f["effects"][0]["on"] = json!(false));
    let (_, off) = shown(&mut app);
    hover_on(&mut app, id, &first, Some("Difference"));
    assert_eq!(shown(&mut app).1, off);
}

#[test]
fn a_list_no_longer_drawn_previews_nothing() {
    let (mut app, layer) = app();
    let id = layer_style::open(&mut app, Some("colorOverlay")).unwrap();
    let effect = selected(&app, id);
    hover_on(&mut app, id, &effect, Some("Multiply"));
    app.frame += 1;
    assert_eq!(blends(&shown(&mut app).0, layer), vec![Some(BlendMode::Multiply)], "drawn last frame: the canvas is drawn first");
    app.frame += 1;
    assert_eq!(blends(&shown(&mut app).0, layer), vec![Some(BlendMode::Normal)], "a stale hover previews nothing");
}

#[test]
fn preview_off_shows_the_document_while_hovering() {
    let (mut app, layer) = app();
    let id = layer_style::open(&mut app, Some("colorOverlay")).unwrap();
    let effect = selected(&app, id);
    edit(&mut app, id, |f| {
        f.insert("preview".into(), json!(false));
    });
    let pending = fields(&app, id);
    let before = Untouched::of(&app);
    for label in ["Multiply", "Screen"] {
        hover_on(&mut app, id, &effect, Some(label));
        assert!(Arc::ptr_eq(&shown(&mut app).0, &before.0), "Preview off: {label} doesn't show");
    }
    assert_eq!(fields(&app, id), pending, "nor does it turn Preview on");
    // Preview on again: the pending style, and the list still hovered over.
    edit(&mut app, id, |f| {
        f.insert("preview".into(), json!(true));
    });
    assert_eq!(blends(&shown(&mut app).0, layer), vec![Some(BlendMode::Screen)]);
    hover_on(&mut app, id, &effect, None);
    assert_eq!(blends(&shown(&mut app).0, layer), vec![Some(BlendMode::Normal)]);
    before.check(&app, "preview off");
}

#[test]
fn cancel_and_reopen_leave_no_hover_behind() {
    let (mut app, layer) = app();
    let id = layer_style::open(&mut app, Some("colorOverlay")).unwrap();
    let effect = selected(&app, id);
    let before = Untouched::of(&app);
    hover_on(&mut app, id, &effect, Some("Multiply"));
    hover_on(&mut app, id, &effect, Some("Screen"));
    shown(&mut app);
    dialogs::cancel(&mut app, id).unwrap();
    assert!(Arc::ptr_eq(&shown(&mut app).0, &before.0), "Cancel: the document as it was");
    before.check(&app, "cancel");
    assert!(app.session.active().unwrap().doc.layer(layer).unwrap().effects.items.is_empty());
    // The same page id in a new dialog isn't the old dialog's hover.
    let again = layer_style::open(&mut app, Some("colorOverlay")).unwrap();
    assert_eq!(selected(&app, again), effect);
    assert_eq!(blends(&shown(&mut app).0, layer), vec![Some(BlendMode::Normal)]);
}

#[test]
fn ok_commits_the_chosen_mode_not_the_hovered_one() {
    let (mut app, layer) = app();
    let id = layer_style::open(&mut app, Some("colorOverlay")).unwrap();
    let effect = selected(&app, id);
    let undo = app.session.active().unwrap().history.past_len();
    edit(&mut app, id, |f| set_param(f, &effect, "blend", json!("Multiply")));
    hover_on(&mut app, id, &effect, Some("Screen"));
    dialogs::confirm(&mut app, id).unwrap();
    let doc = app.session.active().unwrap().doc.clone();
    assert_eq!(blends(&doc, layer), vec![Some(BlendMode::Multiply)]);
    assert_eq!(blends(&shown(&mut app).0, layer), vec![Some(BlendMode::Multiply)], "no hover after OK");
    while app.session.active().unwrap().history.past_len() > undo {
        app.run("edit.undo", json!({})).unwrap();
    }
    assert!(app.session.active().unwrap().doc.layer(layer).unwrap().effects.items.is_empty());
}

fn harness(session: photocraft_engine::Session) -> Harness<'static, PhotocraftApp> {
    let mut h = Harness::builder().with_size(vec2(1440.0, 900.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(session, crate::Services::default())
    });
    h.run_steps(8);
    h
}

fn click(h: &mut Harness<'_, PhotocraftApp>, at: Pos2) {
    h.hover_at(at);
    h.run_steps(1);
    h.event(egui::Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Default::default() });
    h.run_steps(1);
    h.event(egui::Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Default::default() });
    h.run_steps(3);
}

/// The Blend Mode dropdown on the dialog's page: the one on the row of its label.
fn blend_dropdown(h: &Harness<'_, PhotocraftApp>) -> egui::Rect {
    let label = h.get_by_label("Blend Mode").rect();
    h.query_all_by_role(Role::ComboBox)
        .map(|n| n.rect())
        .filter(|r| r.left() > label.right() && (r.center().y - label.center().y).abs() < 8.0)
        .min_by(|a, b| a.left().total_cmp(&b.left()))
        .unwrap()
}

fn layer_style_open(h: &Harness<'_, PhotocraftApp>) -> bool {
    h.state().ui.dialogs.iter().any(|d| d.kind == DialogKind::LayerStyle)
}

#[test]
fn hovering_the_dialogs_list_previews_and_escape_or_leaving_restores() {
    let (s, layer) = session();
    let mut h = harness(s);
    let id = layer_style::open(h.state_mut(), Some("colorOverlay")).unwrap();
    h.run_steps(4);
    let before = Untouched::of(h.state());
    let pending = fields(h.state(), id);
    let previewed = |h: &mut Harness<'_, PhotocraftApp>| blends(&shown(h.state_mut()).0, layer);
    assert_eq!(previewed(&mut h), vec![Some(BlendMode::Normal)]);
    let at = blend_dropdown(&h).center();
    click(&mut h, at);
    let darken = h.get_by_label("Darken").rect();
    h.hover_at(darken.center());
    h.run_steps(3);
    assert_eq!(previewed(&mut h), vec![Some(BlendMode::Darken)], "the hovered mode shows");
    // Between two rows the nearest one keeps showing (#2553).
    let multiply = h.get_by_label("Multiply").rect();
    h.hover_at(Pos2::new(multiply.center().x, (darken.bottom() + multiply.top()) / 2.0));
    h.run_steps(3);
    assert!(previewed(&mut h)[0].is_some_and(|m| m == BlendMode::Darken || m == BlendMode::Multiply));
    h.hover_at(multiply.center());
    h.run_steps(3);
    assert_eq!(previewed(&mut h), vec![Some(BlendMode::Multiply)]);
    assert_eq!(fields(h.state(), id), pending, "hovering is not an edit");
    before.check(h.state(), "hover");
    // Off the list (still open): the pending style.
    h.hover_at(Pos2::new(5.0, 450.0));
    h.run_steps(3);
    assert_eq!(previewed(&mut h), vec![Some(BlendMode::Normal)], "off the list");
    // Back on the list, then Esc closes it without a choice; the dialog stays.
    h.hover_at(h.get_by_label("Screen").rect().center());
    h.run_steps(3);
    assert_eq!(previewed(&mut h), vec![Some(BlendMode::Screen)]);
    h.key_press(Key::Escape);
    h.run_steps(4);
    assert!(h.query_by_label("Darken").is_none(), "Esc closes the list");
    assert!(layer_style_open(&h), "and leaves the dialog open");
    assert_eq!(previewed(&mut h), vec![Some(BlendMode::Normal)], "Esc: the pending style");
    assert_eq!(fields(h.state(), id), pending);
    before.check(h.state(), "escape");
}

#[test]
fn choosing_in_the_dialogs_lists_edits_the_page_and_ok_applies_it() {
    let (s, layer) = session();
    let mut h = harness(s);
    let id = layer_style::open(h.state_mut(), Some("colorOverlay")).unwrap();
    h.run_steps(4);
    let before = Untouched::of(h.state());
    let at = blend_dropdown(&h).center();
    click(&mut h, at);
    let at = h.get_by_label("Screen").rect().center();
    click(&mut h, at);
    let effect = selected(h.state(), id);
    assert_eq!(layer_style::entry(&fields(h.state(), id), &effect).unwrap()["params"]["blend"], "Screen", "a click chooses");
    assert_eq!(blends(&shown(h.state_mut()).0, layer), vec![Some(BlendMode::Screen)]);
    before.check(h.state(), "choice");
    // Blending Options (its row is painted, without an accessibility node): its list previews
    // the layer's own mode.
    edit(h.state_mut(), id, |f| {
        f.insert("selected".into(), json!(layer_style::BLENDING));
    });
    h.run_steps(3);
    assert_eq!(selected(h.state(), id), layer_style::BLENDING);
    let at = blend_dropdown(&h).center();
    click(&mut h, at);
    h.hover_at(h.get_by_label("Multiply").rect().center());
    h.run_steps(3);
    let (doc, _) = shown(h.state_mut());
    assert_eq!(doc.layer(layer).unwrap().blend, BlendMode::Multiply, "the layer's mode shows");
    assert_eq!(blends(&doc, layer), vec![Some(BlendMode::Screen)], "the overlay keeps its chosen mode");
    let at = h.get_by_label("Lighten").rect().center();
    click(&mut h, at);
    assert_eq!(fields(h.state(), id)["p:blendingOptions"]["blend"], "Lighten");
    before.check(h.state(), "blending options");
    dialogs::confirm(h.state_mut(), id).unwrap();
    h.run_steps(2);
    let doc = h.state().session.active().unwrap().doc.clone();
    assert_eq!((doc.layer(layer).unwrap().blend, blends(&doc, layer)), (BlendMode::Lighten, vec![Some(BlendMode::Screen)]), "OK applies the choices");
}
