//! Type Details: every OpenType feature and variation axis of the type target's face, with a
//! live preview drawn by the document's own text engine (not a picture): hovering a control
//! shows what it does to the face, as Figma's type details do. Opened from the Character
//! section; the floating window is drawn with the other type panels.
//!
//! *Details* lists the layout features the face has (letter case, numbers, letterforms,
//! stylistic sets and character variants by the names the font gives them, kerning, and any
//! other feature), greying out the common ones it lacks. *Variable* lists the face's axes by
//! the font's own names with a field, a slider and a reset each, its named instances, and the
//! automatic optical size. Every change goes through `type.setStyle` on the Character panel's
//! scope (the edited characters, else the selected type layers).

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::PoisonError;

use egui::{ColorImage, Rect, Sense, Stroke, TextureHandle, TextureOptions, pos2, vec2};
use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::TextLayer;
use photocraft_doc::text::{Caps, CharStyle, FontFeature, FontVariation, Kerning, TextRun, TextShape};
use photocraft_geom::Affine;
use photocraft_text::FaceDetails;
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::theme::Tokens;

/// Window width and the preview's height, in points.
const WIDTH: f32 = 300.0;
const PREVIEW_H: f32 = 116.0;
/// The scrolling list below the preview.
const LIST_H: f32 = 520.0;
const ROW_H: f32 = 28.0;
const TOGGLE_W: f32 = 64.0;
const PANGRAM: &str = "The quick brown fox jumps over 17 lazy dogs.";
const AXIS_SAMPLE: &str = "Ra";
/// Shown for a feature whose substituted characters can't be read (contextual ones).
const FALLBACK_SAMPLE: &str = "Hamburgefonstiv 0123456789";

/// Features with no switch of their own: required shaping, script forms, and the glyph-palette
/// feature (`aalt`). Listing them would only invite breaking the text.
const INTERNAL: &[&str] = &[
    "aalt", "abvf", "abvm", "abvs", "akhn", "blwf", "blwm", "blws", "ccmp", "cfar", "cjct", "clig", "curs", "dist", "dtls", "fin2", "fin3", "fina", "flac",
    "half", "haln", "init", "isol", "ljmo", "locl", "mark", "med2", "medi", "mkmk", "mset", "nukt", "pref", "pres", "pstf", "psts", "rand", "rclt", "rkrf",
    "rlig", "rphf", "rvrn", "size", "stch", "tjmo", "vjmo", "vert", "vrt2", "vkrn", "vpal", "vhal", "valt", "ltra", "ltrm", "rtla", "rtlm",
];

/// Features on unless the style turns them off (CSS and Photoshop defaults).
fn default_on(tag: &str) -> bool {
    matches!(tag, "liga" | "calt" | "kern")
}

/// What the preview shows: `samples` of `text`, each in its own style with an optional caption,
/// side by side (`across`) or stacked.
#[derive(Clone, Debug, PartialEq)]
struct Preview {
    text: String,
    samples: Vec<(String, CharStyle)>,
    across: bool,
    /// Wrap the text to the preview's width (the pangram).
    wrap: bool,
}

fn hover_id() -> egui::Id {
    egui::Id::new("type-details-hover")
}

/// Draws the Type Details window when it is open.
pub(crate) fn window(app: &mut PhotocraftApp, ctx: &egui::Context, frame: egui::Frame) {
    if !app.ui.type_panels.details {
        return;
    }
    let canvas = app.last_canvas_rect;
    let mut close = false;
    egui::Window::new(tl!("Type Details"))
        .id(egui::Id::new("type-details-window"))
        .title_bar(false)
        .resizable(false)
        .frame(frame)
        .default_pos(pos2((canvas.right() - WIDTH - 24.0).max(canvas.left() + 8.0), canvas.top() + 40.0))
        .show(ctx, |ui| {
            ui.set_width(WIDTH);
            ui.horizontal(|ui| {
                let mut tab = app.ui.type_panels.details_tab;
                for (i, name) in ["Details", "Variable"].iter().enumerate() {
                    if crate::widgets::pill_tab(ui, name, tab == i).clicked() {
                        tab = i;
                    }
                }
                app.ui.type_panels.details_tab = tab;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::icons::button(ui, "x", 20.0, false, tl!("Close")).clicked() {
                        close = true;
                    }
                });
            });
            crate::widgets::hairline(ui);
            ui.add_space(6.0);
            body(app, ui);
        });
    if close {
        app.ui.type_panels.details = false;
    }
}

fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let Some((c, _)) = crate::type_tool::styles_at(app) else {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(tl!("Select a type layer to edit its character and paragraph settings.")).color(t.text_dim));
        ui.add_space(8.0);
        return;
    };
    // Drawn every frame: nothing to list until the background font scan is done (#3021).
    let Some(engine) = photocraft_text::try_shared() else {
        ui.label(egui::RichText::new(tl!("Loading fonts…")).color(t.text_dim));
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        return;
    };
    let details = engine.lock().unwrap_or_else(PoisonError::into_inner).fonts.face_details(&c);
    let variable = app.ui.type_panels.details_tab == 1;
    // The preview reads the hover of the previous pass: the rows are drawn below it.
    let hovered: Option<Preview> = ui.ctx().data(|d| d.get_temp::<Preview>(hover_id()));
    let shown = hovered.unwrap_or_else(|| {
        if variable {
            Preview { text: AXIS_SAMPLE.into(), samples: vec![(String::new(), c.clone())], across: true, wrap: false }
        } else {
            Preview { text: PANGRAM.into(), samples: vec![(String::new(), c.clone())], across: false, wrap: true }
        }
    });
    preview(ui, &shown, &details, c.size_pt);
    ui.add_space(4.0);
    let mut hover: Option<Preview> = None;
    let mut change: Option<Value> = None;
    // As tall as the screen below allows, up to LIST_H.
    let list_h = (ui.ctx().content_rect().bottom() - ui.cursor().top() - 24.0).clamp(160.0, LIST_H);
    egui::ScrollArea::vertical().id_salt(("type-details-list", variable)).max_height(list_h).min_scrolled_height(list_h).auto_shrink([false, true]).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            if variable {
                variable_tab(ui, &c, &details, &mut hover, &mut change);
            } else {
                details_tab(ui, &c, &details, &mut hover, &mut change);
            }
        },
    );
    if hover != ui.ctx().data(|d| d.get_temp::<Preview>(hover_id())) {
        ui.ctx().request_repaint();
    }
    ui.ctx().data_mut(|d| match hover {
        Some(h) => {
            d.insert_temp(hover_id(), h);
        }
        None => d.remove::<Preview>(hover_id()),
    });
    if let Some(props) = change {
        crate::type_tool::apply(app, ui.ctx(), props);
    }
}

// ------------------------------------------------------------------ Details

/// A feature's state as the style draws it.
pub(crate) fn feature_shown_on(c: &CharStyle, tag: &str) -> bool {
    match tag {
        "liga" => c.ligatures,
        "dlig" => c.discretionary_ligatures,
        "kern" => c.kerning == Kerning::Metrics,
        _ => c.features.iter().find(|f| f.tag == tag).map_or(default_on(tag), |f| f.value > 0),
    }
}

/// `type.setStyle` props turning a feature on or off. A feature back at its default is removed
/// rather than stored, so the style keeps only real choices.
pub(crate) fn feature_props(tag: &str, on: bool) -> Value {
    if tag == "kern" {
        return json!({ "kerning": if on { "metrics" } else { "off" } });
    }
    let value = if on == default_on(tag) { Value::Null } else { json!(u16::from(on)) };
    json!({ "features": { tag: value } })
}

/// The style with feature `tag` forced on or off (for previews).
fn with_feature(c: &CharStyle, tag: &str, on: bool) -> CharStyle {
    let mut s = c.clone();
    match tag {
        "liga" => s.ligatures = on,
        "dlig" => s.discretionary_ligatures = on,
        "kern" => {
            s.kerning = if on { Kerning::Metrics } else { Kerning::Off };
            s.kern = 0.0;
        }
        _ => {
            s.features.retain(|f| f.tag != tag);
            s.features.push(FontFeature { tag: tag.into(), value: u16::from(on) });
        }
    }
    s
}

/// Text that shows off a feature: a fixed one for the registered features whose effect needs
/// context (fractions, ordinals, kerning…), else the characters the font substitutes.
fn feature_text(tag: &str, d: &FaceDetails) -> String {
    let fixed = match tag {
        "frac" => "1/2 3/4 7/8 15/16",
        "zero" => "0 10 2020",
        "ordn" => "1a 2o 3rd No",
        "sups" | "subs" | "sinf" => "x2 H2O 10-3",
        "dnom" | "numr" => "1234567890",
        "tnum" | "pnum" | "lnum" | "onum" => "$1,111.18 90.7%",
        "case" => "(H-[A]·{X}@¿)",
        "cpsp" => "CAPITAL SPACING",
        "calt" => "-> --> :+ 12:30 x×y",
        "liga" => "fi fl ff ffi",
        "dlig" => "ct st sp Th",
        "kern" => "AVATAR Toyota LTA",
        "smcp" | "c2sc" | "pcap" | "c2pc" | "unic" => "Small Capitals",
        "titl" => "TITLING",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.into();
    }
    match d.feature(tag) {
        Some(f) if !f.sample.is_empty() => f.sample.clone(),
        _ => FALLBACK_SAMPLE.into(),
    }
}

/// Off-then-on preview of a feature.
fn feature_preview(c: &CharStyle, tag: &str, d: &FaceDetails) -> Preview {
    Preview {
        text: feature_text(tag, d),
        samples: vec![(tl!("Off").to_string(), with_feature(c, tag, false)), (tl!("On").to_string(), with_feature(c, tag, true))],
        across: false,
        wrap: false,
    }
}

/// Registered names of features outside the fixed rows (raw tag when unknown).
fn feature_label(tag: &str) -> String {
    let s = match tag {
        "smcp" => tl!("Small capitals"),
        "c2sc" => tl!("Capitals to small capitals"),
        "pcap" => tl!("Petite capitals"),
        "c2pc" => tl!("Capitals to petite capitals"),
        "unic" => tl!("Unicase"),
        "dnom" => tl!("Denominators"),
        "numr" => tl!("Numerators"),
        "sinf" => tl!("Scientific inferiors"),
        "hist" => tl!("Historical forms"),
        "hlig" => tl!("Historical ligatures"),
        "ornm" => tl!("Ornaments"),
        "nalt" => tl!("Alternate annotation forms"),
        "mgrk" => tl!("Mathematical Greek"),
        "lnum" => tl!("Lining figures"),
        "onum" => tl!("Oldstyle figures"),
        "pnum" => tl!("Proportional figures"),
        "tnum" => tl!("Tabular figures"),
        "afrc" => tl!("Alternative fractions"),
        "cswh" => tl!("Contextual swash"),
        "jalt" => tl!("Justification alternates"),
        "ital" => tl!("Italics"),
        "rtbd" => tl!("Right bounds"),
        "lfbd" => tl!("Left bounds"),
        "opbd" => tl!("Optical bounds"),
        "palt" | "halt" => tl!("Proportional alternate widths"),
        "pwid" => tl!("Proportional widths"),
        "fwid" => tl!("Full widths"),
        "hwid" => tl!("Half widths"),
        "ruby" => tl!("Ruby notation forms"),
        "expt" => tl!("Expert forms"),
        "jp78" | "jp83" | "jp90" | "jp04" | "smpl" | "trad" | "tnam" => tl!("Alternate CJK forms"),
        _ => return tag.to_string(),
    };
    s.to_string()
}

/// A row: its label on the left (wrapping), the control's rect on the right. Returns (row rect,
/// control rect).
fn row_rects(ui: &mut egui::Ui, label: &str, control_w: f32, enabled: bool) -> (Rect, Rect) {
    let t = Tokens::get(ui.ctx());
    let w = ui.available_width();
    let label_w = (w - control_w - 10.0).max(40.0);
    let col = if enabled { t.text } else { t.text_faint };
    let g = ui.painter().layout(label.to_string(), egui::FontId::proportional(12.0), col, label_w);
    let h = (g.size().y + 10.0).max(ROW_H);
    let (r, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    ui.painter().galley(pos2(r.left(), r.center().y - g.size().y / 2.0), g, col);
    let control = Rect::from_min_size(pos2(r.right() - control_w, r.center().y - 12.0), vec2(control_w, 24.0));
    (r, control)
}

/// A segmented control in `rect`: one segment per label (a "✓" label draws a check mark),
/// `selected` raised. Returns (clicked segment, hovered segment).
fn segments(
    ui: &mut egui::Ui,
    id: egui::Id,
    rect: Rect,
    labels: &[String],
    tips: &[String],
    selected: Option<usize>,
    enabled: &[bool],
) -> (Option<usize>, Option<usize>) {
    let t = Tokens::get(ui.ctx());
    let n = labels.len().max(1);
    ui.painter().rect_filled(rect, t.radius_sm, t.field);
    ui.painter().rect_stroke(rect, t.radius_sm, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    let seg_w = rect.width() / n as f32;
    let (mut clicked, mut hovered) = (None, None);
    for (i, label) in labels.iter().enumerate() {
        // The whole segment reacts, so the pointer is always over one of them (no dead gap
        // between neighbours); only the highlight is inset.
        let hit = Rect::from_min_size(pos2(rect.left() + seg_w * i as f32, rect.top()), vec2(seg_w, rect.height()));
        let r = hit.shrink(2.0);
        let on = enabled.get(i).copied().unwrap_or(true);
        let resp = ui.interact(hit, id.with(i), if on { Sense::click() } else { Sense::hover() });
        let is_sel = selected == Some(i);
        if is_sel {
            crate::widgets::surface(ui, r, t.hover, true);
        } else if on && resp.hovered() {
            ui.painter().rect_filled(r, t.radius_sm, t.hover.gamma_multiply(0.6));
        }
        let col = match (on, is_sel) {
            (false, _) => t.text_faint.gamma_multiply(0.6),
            (true, true) => t.text,
            (true, false) => t.text_dim,
        };
        if label == "✓" {
            let c = r.center();
            ui.painter().line_segment([c + vec2(-5.0, 0.0), c + vec2(-1.5, 3.5)], Stroke::new(1.4, col));
            ui.painter().line_segment([c + vec2(-1.5, 3.5), c + vec2(5.5, -4.0)], Stroke::new(1.4, col));
        } else if label == "—" {
            let c = r.center();
            ui.painter().line_segment([c + vec2(-6.0, 0.0), c + vec2(6.0, 0.0)], Stroke::new(1.4, col));
        } else {
            let g = ui.painter().layout_no_wrap(label.clone(), egui::FontId::proportional(11.5), col);
            let g = if g.size().x > r.width() - 4.0 { ui.painter().layout(label.clone(), egui::FontId::proportional(10.0), col, r.width() - 2.0) } else { g };
            ui.painter().galley(r.center() - g.size() / 2.0, g, col);
        }
        let tip = tips.get(i).cloned().unwrap_or_else(|| label.clone());
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, on, is_sel, &tip));
        let resp = if tip.is_empty() { resp } else { resp.on_hover_text(tip) };
        if resp.contains_pointer() {
            hovered = Some(i);
        }
        if on && resp.clicked() {
            clicked = Some(i);
        }
    }
    (clicked, hovered)
}

/// A feature row with Figma's [— | ✓] switch; disabled (greyed) when the face lacks it.
fn feature_row(ui: &mut egui::Ui, c: &CharStyle, d: &FaceDetails, tag: &str, label: &str, hover: &mut Option<Preview>, change: &mut Option<Value>) {
    let enabled = d.has_feature(tag);
    let (row, control) = row_rects(ui, label, TOGGLE_W, enabled);
    let on = feature_shown_on(c, tag);
    let tips = [format!("{label}: {}", tl!("Off")), format!("{label}: {}", tl!("On"))];
    let (clicked, _) =
        segments(ui, egui::Id::new(("type-details-feature", tag)), control, &["—".into(), "✓".into()], &tips, Some(usize::from(on)), &[enabled, enabled]);
    if let Some(i) = clicked {
        *change = Some(feature_props(tag, i == 1));
    }
    if enabled && ui.rect_contains_pointer(row) {
        *hover = Some(feature_preview(c, tag, d));
    }
}

/// A row of mutually exclusive options under its label, full width: (caption, tooltip, the style
/// with the option, its props, available, selected).
struct Choice {
    caption: String,
    tip: String,
    style: CharStyle,
    props: Value,
    available: bool,
    selected: bool,
}

fn choice_row(ui: &mut egui::Ui, id: &str, label: &str, text: &str, choices: Vec<Choice>, hover: &mut Option<Preview>, change: &mut Option<Value>) {
    let t = Tokens::get(ui.ctx());
    let any = choices.iter().skip(1).any(|c| c.available);
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
    ui.painter().text(r.left_center(), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(12.0), if any { t.text } else { t.text_faint });
    let (seg, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
    let labels: Vec<String> = choices.iter().map(|c| c.caption.clone()).collect();
    let tips: Vec<String> = choices.iter().map(|c| c.tip.clone()).collect();
    let enabled: Vec<bool> = choices.iter().map(|c| c.available).collect();
    let selected = choices.iter().position(|c| c.selected);
    let (clicked, hovered) = segments(ui, egui::Id::new(("type-details-choice", id)), seg, &labels, &tips, selected, &enabled);
    ui.add_space(6.0);
    if let Some(ch) = clicked.and_then(|i| choices.get(i)) {
        *change = Some(ch.props.clone());
    }
    if let Some(ch) = hovered.and_then(|i| choices.get(i)) {
        *hover = Some(Preview { text: text.into(), samples: vec![(ch.tip.clone(), ch.style.clone())], across: true, wrap: false });
    }
}

/// Options that each turn on one feature of a set and off the others (figure style, figure
/// position): `options` are (tag, caption, tooltip); the first choice is "none of them".
fn exclusive(c: &CharStyle, d: &FaceDetails, none_tip: &str, options: &[(&str, &str, &str)]) -> Vec<Choice> {
    let mut clear = c.clone();
    clear.features.retain(|f| !options.iter().any(|(t, _, _)| *t == f.tag));
    let none_props: serde_json::Map<String, Value> = options.iter().map(|(t, _, _)| (t.to_string(), Value::Null)).collect();
    let mut v = vec![Choice {
        caption: "—".into(),
        tip: none_tip.into(),
        style: clear.clone(),
        props: json!({ "features": none_props }),
        available: true,
        selected: !options.iter().any(|(t, _, _)| feature_shown_on(c, t)),
    }];
    for (tag, caption, tip) in options {
        let mut props = none_props.clone();
        props.insert(tag.to_string(), json!(1));
        let mut style = clear.clone();
        style.features.push(FontFeature { tag: tag.to_string(), value: 1 });
        v.push(Choice {
            caption: caption.to_string(),
            tip: tip.to_string(),
            style,
            props: json!({ "features": props }),
            available: d.has_feature(tag),
            selected: feature_shown_on(c, tag),
        });
    }
    v
}

fn heading(ui: &mut egui::Ui, title: &str) {
    let t = Tokens::get(ui.ctx());
    ui.add_space(4.0);
    crate::widgets::hairline(ui);
    ui.add_space(6.0);
    ui.label(egui::RichText::new(title).font(crate::theme::semibold(12.0)).color(t.text));
    ui.add_space(2.0);
}

fn details_tab(ui: &mut egui::Ui, c: &CharStyle, d: &FaceDetails, hover: &mut Option<Preview>, change: &mut Option<Value>) {
    heading(ui, tl!("Letter case"));
    let caps = |caps: Caps| CharStyle { caps, ..c.clone() };
    let case = vec![
        Choice {
            caption: "—".into(),
            tip: tl!("Normal").into(),
            style: caps(Caps::Normal),
            props: json!({"caps": "normal"}),
            available: true,
            selected: c.caps == Caps::Normal,
        },
        Choice {
            caption: "AG".into(),
            tip: tl!("All Caps").into(),
            style: caps(Caps::AllCaps),
            props: json!({"caps": "allCaps"}),
            available: true,
            selected: c.caps == Caps::AllCaps,
        },
        Choice {
            caption: "Aɢ".into(),
            tip: tl!("Small Caps").into(),
            style: caps(Caps::SmallCaps),
            props: json!({"caps": "smallCaps"}),
            available: true,
            selected: c.caps == Caps::SmallCaps,
        },
    ];
    choice_row(ui, "case", tl!("Case"), "Typography Details", case, hover, change);
    feature_row(ui, c, d, "case", tl!("Case-sensitive forms"), hover, change);
    feature_row(ui, c, d, "cpsp", tl!("Capital spacing"), hover, change);

    heading(ui, tl!("Numbers"));
    let nums = "$1,111.18 90.7% 2026";
    let style =
        exclusive(c, d, tl!("Default"), &[("pnum", tl!("Proportional"), tl!("Proportional figures")), ("tnum", tl!("Tabular"), tl!("Tabular figures"))]);
    choice_row(ui, "num-style", tl!("Figure spacing"), nums, style, hover, change);
    let forms = exclusive(c, d, tl!("Default"), &[("lnum", tl!("Lining"), tl!("Lining figures")), ("onum", tl!("Oldstyle"), tl!("Oldstyle figures"))]);
    choice_row(ui, "num-form", tl!("Figure style"), nums, forms, hover, change);
    let pos =
        exclusive(c, d, tl!("Normal"), &[("subs", "A₂", tl!("Subscript")), ("sups", "A²", tl!("Superscript")), ("sinf", "A₍", tl!("Scientific inferiors"))]);
    choice_row(ui, "num-pos", tl!("Position"), "x2 H2O 10-3", pos, hover, change);
    feature_row(ui, c, d, "frac", tl!("Fractions"), hover, change);
    feature_row(ui, c, d, "zero", tl!("Slashed zero"), hover, change);

    heading(ui, tl!("Letterforms"));
    feature_row(ui, c, d, "liga", tl!("Standard Ligatures"), hover, change);
    feature_row(ui, c, d, "dlig", tl!("Discretionary Ligatures"), hover, change);
    feature_row(ui, c, d, "calt", tl!("Contextual Alternates"), hover, change);
    feature_row(ui, c, d, "swsh", tl!("Swash"), hover, change);
    feature_row(ui, c, d, "titl", tl!("Titling Alternates"), hover, change);
    feature_row(ui, c, d, "ordn", tl!("Ordinals"), hover, change);

    heading(ui, tl!("Stylistic sets"));
    feature_row(ui, c, d, "salt", tl!("Stylistic Alternates"), hover, change);
    for n in 1..=20 {
        let tag = format!("ss{n:02}");
        if let Some(f) = d.feature(&tag) {
            let label = f.name.clone().unwrap_or_else(|| crate::i18n::fmt(tl!("Stylistic set {n}"), &[("n", &n.to_string())]));
            feature_row(ui, c, d, &tag, &label, hover, change);
        }
    }
    let variants: Vec<_> = (1..=99).filter_map(|n| d.feature(&format!("cv{n:02}")).map(|f| (n, f))).collect();
    if !variants.is_empty() {
        heading(ui, tl!("Character variants"));
        for (n, f) in variants {
            let label = f.name.clone().unwrap_or_else(|| crate::i18n::fmt(tl!("Character variant {n}"), &[("n", &n.to_string())]));
            feature_row(ui, c, d, &f.tag, &label, hover, change);
        }
    }

    heading(ui, tl!("Horizontal spacing"));
    feature_row(ui, c, d, "kern", tl!("Kerning pairs"), hover, change);

    const SHOWN: &[&str] = &[
        "case", "cpsp", "pnum", "tnum", "lnum", "onum", "subs", "sups", "sinf", "frac", "zero", "liga", "dlig", "calt", "swsh", "titl", "ordn", "salt", "kern",
    ];
    let more: Vec<_> = d
        .features
        .iter()
        .filter(|f| !SHOWN.contains(&f.tag.as_str()) && !INTERNAL.contains(&f.tag.as_str()))
        .filter(|f| !(f.tag.len() == 4 && (f.tag.starts_with("ss") || f.tag.starts_with("cv")) && f.tag[2..].bytes().all(|b| b.is_ascii_digit())))
        .collect();
    if !more.is_empty() {
        heading(ui, tl!("More features"));
        for f in more {
            let label = feature_label(&f.tag);
            let label = if label == f.tag { label } else { format!("{label}  ({})", f.tag) };
            feature_row(ui, c, d, &f.tag, &label, hover, change);
        }
    }
    ui.add_space(6.0);
}

// ------------------------------------------------------------------ Variable

/// Axis label: the registered axes in the interface language, else the font's own name, else
/// Google's axis registry name, else the tag.
pub(crate) fn axis_name(tag: &str, font_name: Option<&str>) -> String {
    let registered = match tag {
        "wght" => Some(tl!("Weight")),
        "wdth" => Some(tl!("Width")),
        "slnt" => Some(tl!("Slant")),
        "ital" => Some(tl!("Italic")),
        "opsz" => Some(tl!("Optical size")),
        _ => None,
    };
    if let Some(r) = registered {
        return r.to_string();
    }
    if let Some(n) = font_name {
        return n.to_string();
    }
    registry_name(tag).map_or_else(|| tag.to_string(), str::to_string)
}

/// Display names from the Google Fonts axis registry (github.com/googlefonts/axisregistry, Apache
/// 2.0) for fonts that don't name their axes.
fn registry_name(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "GRAD" => "Grade",
        "XOPQ" => "Thick Stroke",
        "XTRA" => "Counter Width",
        "YOPQ" => "Thin Stroke",
        "YTAS" => "Ascender Height",
        "YTDE" => "Descender Depth",
        "YTFI" => "Figure Height",
        "YTLC" => "Lowercase Height",
        "YTUC" => "Uppercase Height",
        "CASL" => "Casual",
        "CRSV" => "Cursive",
        "MONO" => "Monospace",
        "SOFT" => "Softness",
        "WONK" => "Wonky",
        "FILL" => "Fill",
        "SPAC" => "Spacing",
        "ROND" => "Roundness",
        "SHRP" => "Sharpness",
        "BNCE" => "Bounce",
        "INFM" => "Informality",
        "FLAR" => "Flare",
        "VOLM" => "Volume",
        "CTRS" => "Contrast",
        "BLED" => "Bleed",
        "EDPT" => "Extrusion Depth",
        "EHLT" => "Edge Highlight",
        "HEXP" => "Hyper Expansion",
        "MORF" => "Morph",
        "SCAN" => "Scanlines",
        "XROT" => "Rotation in X",
        "YROT" => "Rotation in Y",
        "ZROT" => "Rotation in Z",
        "YEAR" => "Year",
        "ELGR" => "Element Grid",
        "ELSH" => "Element Shape",
        "ELXP" => "Element Expansion",
        _ => return None,
    })
}

/// The value the layout draws an axis at: the style's own, else its weight on `wght`, the type
/// size on `opsz` (automatic optical size), the axis default otherwise; within the range.
pub(crate) fn axis_drawn(c: &CharStyle, tag: &str, min: f32, default: f32, max: f32) -> f32 {
    let v = c.variations.iter().find(|v| v.axis == tag).map(|v| v.value).unwrap_or_else(|| match tag {
        "wght" => f32::from(c.weight),
        "opsz" => c.size_pt,
        "ital" if c.italic => max,
        _ => default,
    });
    if v.is_finite() { v.clamp(min, max) } else { default.clamp(min, max) }
}

fn with_axis(c: &CharStyle, tag: &str, v: f32) -> CharStyle {
    let mut s = c.clone();
    s.variations.retain(|x| x.axis != tag);
    s.variations.push(FontVariation { axis: tag.into(), value: v });
    s
}

/// Registered axes first in Figma's order, then the others in the font's order.
fn axis_order(tag: &str) -> usize {
    ["slnt", "ital", "wdth", "wght", "GRAD", "opsz"].iter().position(|t| *t == tag).unwrap_or(usize::MAX)
}

fn variable_tab(ui: &mut egui::Ui, c: &CharStyle, d: &FaceDetails, hover: &mut Option<Preview>, change: &mut Option<Value>) {
    let t = Tokens::get(ui.ctx());
    let mut axes: Vec<_> = d.axes.iter().filter(|a| a.min < a.max).collect();
    if axes.is_empty() {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(tl!("This font has no variable axes.")).color(t.text_dim));
        ui.add_space(8.0);
        return;
    }
    axes.sort_by_key(|a| axis_order(&a.tag));
    if !d.instances.is_empty() {
        instance_row(ui, c, d, hover, change);
    }
    // Axes the font marks hidden (Roboto Flex's parametric ones) wait in the collapsed group, as
    // in Figma, rather than not being reachable at all.
    let (main, extra): (Vec<_>, Vec<_>) = axes.into_iter().partition(|a| !a.hidden && axis_order(&a.tag) != usize::MAX);
    for a in main {
        axis_row(ui, c, a, hover, change);
    }
    if !extra.is_empty() && crate::props_layout::section(ui, "type-details-axes", tl!("Additional axes")) {
        for a in extra {
            axis_row(ui, c, a, hover, change);
        }
    }
    ui.add_space(6.0);
    if c.variations.iter().any(|v| d.axis(&v.axis).is_some()) && crate::widgets::secondary_button(ui, tl!("Reset All Axes"), ui.available_width()).clicked() {
        let m: serde_json::Map<String, Value> = d.axes.iter().map(|a| (a.tag.clone(), Value::Null)).collect();
        *change = Some(json!({ "variations": m }));
    }
    ui.add_space(6.0);
}

/// The face's named instances: picking one sets every axis to its coordinates.
fn instance_row(ui: &mut egui::Ui, c: &CharStyle, d: &FaceDetails, hover: &mut Option<Preview>, change: &mut Option<Value>) {
    let t = Tokens::get(ui.ctx());
    let matches = |coords: &[(String, f32)]| {
        coords.iter().all(|(tag, v)| d.axis(tag).is_none_or(|a| tag == "opsz" || (axis_drawn(c, tag, a.min, a.default, a.max) - v).abs() < 0.5))
    };
    let custom = tl!("Custom").to_string();
    let mut current =
        d.instances.iter().position(|i| matches(&i.coords)).map_or(custom.clone(), |i| d.instances.get(i).map(|x| x.name.clone()).unwrap_or_default());
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(tl!("Instance")).color(t.text_dim));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut opts: Vec<(String, &str)> = d.instances.iter().map(|i| (i.name.clone(), i.name.as_str())).collect();
            if current == custom {
                opts.insert(0, (custom.clone(), custom.as_str()));
            }
            let (picked, hovered) = crate::widgets::dropdown_hovered(ui, "type-details-instance", &mut current, &opts, 180.0);
            if picked && let Some(inst) = d.instances.iter().find(|i| i.name == current) {
                let m: serde_json::Map<String, Value> = inst.coords.iter().filter(|(tag, _)| tag != "opsz").map(|(tag, v)| (tag.clone(), json!(v))).collect();
                *change = Some(json!({ "variations": m }));
            }
            if let Some(inst) = hovered.and_then(|h| d.instances.iter().find(|i| i.name == h)) {
                let mut s = c.clone();
                for (tag, v) in inst.coords.iter().filter(|(tag, _)| tag != "opsz") {
                    s = with_axis(&s, tag, *v);
                }
                *hover =
                    Some(Preview { text: AXIS_SAMPLE.into(), samples: vec![(String::new(), c.clone()), (inst.name.clone(), s)], across: true, wrap: false });
            }
        });
    });
    ui.add_space(crate::theme::ROW_GAP);
}

fn axis_row(ui: &mut egui::Ui, c: &CharStyle, a: &photocraft_text::AxisDetail, hover: &mut Option<Preview>, change: &mut Option<Value>) {
    let t = Tokens::get(ui.ctx());
    let tag = a.tag.as_str();
    let auto = tag == "opsz" && !c.variations.iter().any(|v| v.axis == "opsz");
    let drawn = axis_drawn(c, tag, a.min, a.default, a.max);
    let mut v = drawn;
    let top = ui.cursor().top();
    let mut set: Option<Value> = None;
    ui.push_id(("type-details-axis", tag), |ui| {
        ui.horizontal(|ui| {
            let name = axis_name(tag, a.name.as_deref());
            let resp = ui.add(egui::Label::new(egui::RichText::new(&name).color(t.text_dim)).truncate());
            resp.on_hover_text(format!(
                "{name} ({tag}) {}–{}, {} {}",
                crate::widgets::fmt_num(a.min.into()),
                crate::widgets::fmt_num(a.max.into()),
                tl!("Default"),
                crate::widgets::fmt_num(a.default.into())
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let at_default = !c.variations.iter().any(|x| x.axis == tag);
                let reset = ui.add_enabled_ui(!at_default, |ui| crate::icons::button(ui, "undo-2", 20.0, false, tl!("Reset"))).inner;
                if reset.clicked() {
                    set = Some(Value::Null);
                }
                if crate::widgets::value_field(ui, &mut v, a.min..=a.max, "", 64.0).changed() {
                    set = Some(json!(round_axis(v, a.min, a.max)));
                }
            });
        });
        if crate::widgets::slider(ui, &mut v, a.min..=a.max, None).changed() {
            set = Some(json!(round_axis(v, a.min, a.max)));
        }
        if tag == "opsz" {
            let mut on = auto;
            if crate::widgets::checkbox(ui, &mut on, "Set optical size automatically").changed() {
                set = Some(if on { Value::Null } else { json!(round_axis(drawn, a.min, a.max)) });
            }
        }
    });
    ui.add_space(crate::theme::ROW_GAP + 2.0);
    let row = Rect::from_min_max(pos2(ui.min_rect().left(), top), pos2(ui.min_rect().right(), ui.cursor().top()));
    if ui.rect_contains_pointer(row) {
        let lo = with_axis(c, tag, a.min);
        let hi = with_axis(c, tag, a.max);
        *hover = Some(Preview {
            text: AXIS_SAMPLE.into(),
            samples: vec![(crate::widgets::fmt_num(a.min.into()), lo), (crate::widgets::fmt_num(a.max.into()), hi)],
            across: true,
            wrap: false,
        });
    }
    if let Some(value) = set {
        let mut m = serde_json::Map::new();
        m.insert(tag.to_string(), value);
        *change = Some(json!({ "variations": m }));
    }
}

/// Whole units on wide axes (weight 100–900), hundredths on narrow ones (ital 0–1).
pub(crate) fn round_axis(v: f32, min: f32, max: f32) -> f32 {
    let v = if max - min >= 10.0 { v.round() } else { (v * 100.0).round() / 100.0 };
    v.clamp(min, max)
}

// ------------------------------------------------------------------ preview

#[derive(Clone, Default)]
struct Cache(VecDeque<(u64, Option<TextureHandle>)>);
const CACHE_LIMIT: usize = 48;

/// The preview box: each sample rendered by the text engine into its slot, white ink tinted with
/// the theme's text colour, the caption under it.
fn preview(ui: &mut egui::Ui, p: &Preview, d: &FaceDetails, doc_size: f32) {
    let t = Tokens::get(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), PREVIEW_H), Sense::hover());
    ui.painter().rect_filled(rect, t.radius_sm, t.field);
    let n = p.samples.len().max(1) as f32;
    let inner = rect.shrink2(vec2(14.0, 10.0));
    let captions = p.samples.iter().any(|(c, _)| !c.is_empty());
    for (i, (caption, style)) in p.samples.iter().enumerate() {
        let slot = if p.across {
            Rect::from_min_size(pos2(inner.left() + inner.width() / n * i as f32, inner.top()), vec2(inner.width() / n, inner.height()))
        } else {
            Rect::from_min_size(pos2(inner.left(), inner.top() + inner.height() / n * i as f32), vec2(inner.width(), inner.height() / n))
        };
        let (ink, cap) = if captions && p.across {
            (Rect::from_min_max(slot.min, pos2(slot.right(), slot.bottom() - 16.0)), Some(pos2(slot.center().x, slot.bottom() - 6.0)))
        } else if captions {
            (Rect::from_min_max(pos2(slot.left() + 34.0, slot.top()), slot.max), Some(pos2(slot.left(), slot.center().y)))
        } else {
            (slot, None)
        };
        if let (Some(at), false) = (cap, caption.is_empty()) {
            let align = if p.across { egui::Align2::CENTER_CENTER } else { egui::Align2::LEFT_CENTER };
            ui.painter().text(at, align, caption, egui::FontId::proportional(10.5), t.text_faint);
        }
        let style = preview_style(style, d, doc_size);
        if let Some(tex) = sample_texture(ui.ctx(), &p.text, &style, ink.size(), p.wrap) {
            let size = tex.size_vec2() / ui.ctx().pixels_per_point().ceil().max(1.0);
            let at = if p.wrap || !p.across && captions { pos2(ink.left(), ink.center().y - size.y / 2.0) } else { ink.center() - size / 2.0 };
            ui.painter().image(tex.id(), Rect::from_min_size(at, size), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), t.text);
        }
    }
}

/// The style a sample draws with: no baseline shift or leading, and the optical size the
/// document draws at (the type size), not the preview's.
fn preview_style(c: &CharStyle, d: &FaceDetails, doc_size: f32) -> CharStyle {
    let mut s = c.clone();
    s.baseline_shift_pt = 0.0;
    s.leading_pt = None;
    s.underline = false;
    s.strikethrough = false;
    if let Some(a) = d.axis("opsz")
        && !s.variations.iter().any(|v| v.axis == "opsz")
        && doc_size.is_finite()
    {
        s.variations.push(FontVariation { axis: "opsz".into(), value: doc_size.clamp(a.min, a.max) });
    }
    s
}

fn sample_texture(ctx: &egui::Context, text: &str, style: &CharStyle, slot: egui::Vec2, wrap: bool) -> Option<TextureHandle> {
    let scale = ctx.pixels_per_point().ceil().clamp(1.0, 4.0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (text, format!("{style:?}"), slot.x.to_bits(), slot.y.to_bits(), scale.to_bits(), wrap, photocraft_text::fonts::shared_generation()).hash(&mut h);
    let key = h.finish();
    let id = egui::Id::new("type-details-previews");
    let cached = ctx.data_mut(|d| d.get_temp_mut_or_default::<Cache>(id).0.iter().find(|(k, _)| *k == key).map(|(_, t)| t.clone()));
    if let Some(t) = cached {
        return t;
    }
    let img = {
        let mut engine = photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner);
        render(&mut engine, text, style, [(slot.x * scale) as u32, (slot.y * scale) as u32], wrap)
    };
    let tex = img.map(|img| ctx.load_texture("type-details-preview", img, TextureOptions::LINEAR));
    ctx.data_mut(|d| {
        let cache = d.get_temp_mut_or_default::<Cache>(id);
        if cache.0.len() >= CACHE_LIMIT {
            cache.0.pop_front();
        }
        cache.0.push_back((key, tex.clone()));
    });
    tex
}

/// Renders `text` in `style` to fit a `slot` (pixels): as large as fits, up to a size that fills
/// the slot's height, wrapped to its width when `wrap`. White ink with coverage as alpha.
pub(crate) fn render(engine: &mut photocraft_text::TextEngine, text: &str, style: &CharStyle, slot: [u32; 2], wrap: bool) -> Option<ColorImage> {
    let [w, h] = slot;
    if text.is_empty() || w < 4 || h < 4 || w > 4096 || h > 4096 {
        return None;
    }
    let layout_at = |engine: &mut photocraft_text::TextEngine, size: f32| {
        let mut s = style.clone();
        s.size_pt = size;
        let layer = TextLayer {
            text: text.into(),
            font_family: s.font_family.clone(),
            size_pt: size,
            runs: vec![TextRun { len: text.len(), style: s }],
            shape: if wrap { TextShape::Box { x: 0.0, y: 0.0, width: w as f32, height: h as f32 * 8.0 } } else { TextShape::Point },
            ..Default::default()
        };
        let layout = engine.layout(&layer, 72.0);
        let b = layout.bounds();
        (layout, b)
    };
    // Wrapped text keeps a reading size; a single line fills the height.
    let mut size = if wrap { h as f32 / 3.6 } else { h as f32 * 0.62 };
    let (mut layout, mut bounds) = layout_at(engine, size);
    for _ in 0..3 {
        let [x0, y0, x1, y1] = bounds?;
        let (bw, bh) = (x1 - x0, y1 - y0);
        let fit = if wrap { h as f32 * 0.96 / bh.max(1.0) } else { (w as f32 * 0.98 / bw.max(1.0)).min(h as f32 * 0.96 / bh.max(1.0)) };
        if !fit.is_finite() || fit >= 1.0 {
            break;
        }
        size = (size * fit * 0.98).max(1.0);
        (layout, bounds) = layout_at(engine, size);
    }
    let [x0, y0, x1, y1] = bounds?;
    let (bw, bh) = (x1 - x0, y1 - y0);
    if !(bw.is_finite() && bh.is_finite()) || bw <= 0.0 || bh <= 0.0 {
        return None;
    }
    let (out_w, out_h) = ((bw.ceil() as u32 + 2).min(w), (bh.ceil() as u32 + 2).min(h));
    let transform = Affine::translate(f64::from(1.0 - x0), f64::from(1.0 - y0));
    // Reject oversized ink before the rasterizer allocates (malformed installed fonts).
    let ink = photocraft_text::render::ink_rect(&layout, &transform);
    if ink.is_empty() || ink.width() > out_w * 4 || ink.height() > out_h * 4 {
        return None;
    }
    let rendered = photocraft_text::render::rasterize(&layout, &transform, PixelFormat::new(ColorMode::Rgb, SampleType::U8, true), style_antialias());
    let (ow, oh) = (out_w as usize, out_h as usize);
    let mut rgba = vec![[0u8; 4]; ow * oh];
    rendered.surface.read_rgba8_into(photocraft_geom::Rect::new(0, 0, out_w as i32, out_h as i32), &mut rgba);
    Some(ColorImage::from_rgba_unmultiplied([ow, oh], &rgba.into_iter().flat_map(|p| [255, 255, 255, p[3]]).collect::<Vec<_>>()))
}

fn style_antialias() -> photocraft_doc::text::AntiAlias {
    photocraft_doc::text::AntiAlias::Smooth
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use photocraft_doc::LayerContent;

    fn app_with_type() -> (PhotocraftApp, u64) {
        // The window waits for the font scan; finish it first.
        drop(photocraft_text::shared().lock());
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 400, "height": 200})).unwrap();
        let id = app.run("type.create", json!({"x": 10, "y": 80, "text": "Ra 0123", "font": "Inter", "size": 24})).unwrap()["layer"].as_u64().unwrap();
        (app, id)
    }

    fn style(app: &PhotocraftApp, id: u64) -> CharStyle {
        match &app.session.active().unwrap().doc.layer(photocraft_doc::LayerId(id)).unwrap().content {
            LayerContent::Text(t) => t.char_runs()[0].style.clone(),
            _ => panic!("not a type layer"),
        }
    }

    fn harness(app: PhotocraftApp, tab: usize) -> Harness<'static, PhotocraftApp> {
        let mut h = Harness::builder().with_size(vec2(340.0, 1400.0)).build_ui_state(
            move |ui, app: &mut PhotocraftApp| {
                app.ui.type_panels.details_tab = tab;
                if ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                    ui.set_width(WIDTH);
                    body(app, ui);
                }
            },
            app,
        );
        PhotocraftApp::setup_context(&h.ctx, Default::default());
        h.run_steps(4);
        h
    }

    /// Scrolls a control into the list's view, then clicks it.
    fn press(h: &mut Harness<'static, PhotocraftApp>, label: &str) {
        h.get_by_label(label).scroll_to_me();
        h.run_steps(3);
        h.get_by_label(label).click();
        h.run();
    }

    /// Each feature row's [— | ✓] switch sets or clears the feature on the type target; a feature
    /// back at its default is removed, not stored.
    #[test]
    fn details_switches_set_and_clear_features() {
        let (app, id) = app_with_type();
        let mut h = harness(app, 0);
        press(&mut h, "Single-story a: On");
        assert!(style(h.state(), id).features.iter().any(|f| f.tag == "cv11" && f.value == 1));
        h.run();
        press(&mut h, "Single-story a: Off");
        assert!(!style(h.state(), id).features.iter().any(|f| f.tag == "cv11"), "back to the default: removed");
        // Contextual alternates are on by default: off stores 0, on removes it again.
        press(&mut h, "Contextual Alternates: Off");
        assert!(style(h.state(), id).features.iter().any(|f| f.tag == "calt" && f.value == 0));
        h.run();
        press(&mut h, "Contextual Alternates: On");
        assert!(!style(h.state(), id).features.iter().any(|f| f.tag == "calt"));
        // Ligatures and kerning go to their own fields.
        press(&mut h, "Kerning pairs: Off");
        assert_eq!(style(h.state(), id).kerning, Kerning::Off);
        // Figure spacing is one choice of two features.
        press(&mut h, "Tabular figures");
        let st = style(h.state(), id);
        assert!(st.features.iter().any(|f| f.tag == "tnum" && f.value == 1) && !st.features.iter().any(|f| f.tag == "pnum"));
        h.run();
        press(&mut h, "Proportional figures");
        let st = style(h.state(), id);
        assert!(st.features.iter().any(|f| f.tag == "pnum") && !st.features.iter().any(|f| f.tag == "tnum"));
        h.run();
        press(&mut h, "All Caps");
        assert_eq!(style(h.state(), id).caps, Caps::AllCaps);
    }

    /// Hovering a feature previews it off and on, in the target's face; leaving shows the pangram.
    #[test]
    fn hovering_a_feature_previews_it() {
        let (app, _) = app_with_type();
        let mut h = harness(app, 0);
        h.get_by_label("Single-story a: On").scroll_to_me();
        h.run_steps(3);
        h.get_by_label("Single-story a: On").hover();
        h.run_steps(2);
        let p: Preview = h.ctx.data(|d| d.get_temp(hover_id())).expect("a hover preview");
        assert_eq!(p.samples.len(), 2);
        assert!(p.text.contains('a'), "the characters the feature changes: {:?}", p.text);
        assert!(!feature_shown_on(&p.samples[0].1, "cv11") && feature_shown_on(&p.samples[1].1, "cv11"));
    }

    /// Moving across a segmented control always hovers one of its segments, enabled or not: the
    /// boundary between two has no dead gap.
    #[test]
    fn segments_leave_no_hover_gap() {
        let rect = Rect::from_min_size(pos2(10.0, 10.0), vec2(270.0, 24.0));
        let labels: Vec<String> = ["—", "Lining", "Oldstyle"].iter().map(|s| s.to_string()).collect();
        let mut h = Harness::builder().with_size(vec2(300.0, 60.0)).build_ui_state(
            move |ui, hovered: &mut Option<usize>| {
                // Lining and Oldstyle disabled, as for a face without them (Inter): hover-only
                // widgets get no interaction-radius slack, so any gap between them showed.
                *hovered = segments(ui, egui::Id::new("seg-test"), rect, &labels, &labels, Some(0), &[true, false, false]).1;
            },
            None,
        );
        let mut found = Vec::new();
        // Slowly, as a person does: tooltips have time to open on the way.
        for x in rect.left() as i32 + 1..rect.right() as i32 {
            h.event(egui::Event::PointerMoved(pos2(x as f32 + 0.5, rect.center().y)));
            h.run_steps(12);
            found.push((x, *h.state()));
        }
        let gaps: Vec<_> = found.iter().filter(|(_, h)| h.is_none()).collect();
        assert!(gaps.is_empty(), "no segment hovered at x = {gaps:?}");
        assert_eq!(found.first().and_then(|f| f.1), Some(0));
        assert_eq!(found.last().and_then(|f| f.1), Some(2));
    }

    /// A face without axes says so instead of showing empty sliders.
    #[test]
    fn variable_tab_without_axes_says_so() {
        let (mut app, id) = app_with_type();
        let d = photocraft_text::shared().lock().unwrap().fonts.face_details(&style(&app, id));
        if !d.axes.is_empty() {
            // An installed variable Inter: switch to the bundled monospace family, which is static.
            app.run("type.setStyle", json!({"layer": id, "font": "JetBrains Mono"})).unwrap();
        }
        let h = harness(app, 1);
        h.get_by_label("This font has no variable axes.");
    }

    #[test]
    fn helpers() {
        let c = CharStyle { weight: 700, size_pt: 30.0, ..Default::default() };
        assert_eq!(axis_drawn(&c, "wght", 100.0, 400.0, 900.0), 700.0);
        assert_eq!(axis_drawn(&c, "opsz", 14.0, 14.0, 32.0), 30.0, "automatic optical size is the type size");
        assert_eq!(axis_drawn(&c, "opsz", 8.0, 14.0, 24.0), 24.0, "within the range");
        assert_eq!(axis_drawn(&c, "GRAD", -200.0, 0.0, 150.0), 0.0);
        assert_eq!(axis_name("XOPQ", Some("Parametric Thick Stroke")), "Parametric Thick Stroke");
        assert_eq!(axis_name("XOPQ", None), "Thick Stroke", "the axis registry when the font has no name");
        assert_eq!(axis_name("ZZZZ", None), "ZZZZ");
        assert_eq!(axis_name("wght", Some("Gewicht")), "Weight", "registered axes in the interface language");
        assert_eq!(round_axis(650.4, 100.0, 900.0), 650.0);
        assert_eq!(round_axis(0.333, 0.0, 1.0), 0.33);
        assert_eq!(feature_props("ss01", true), json!({"features": {"ss01": 1}}));
        assert_eq!(feature_props("ss01", false), json!({"features": {"ss01": null}}));
        assert_eq!(feature_props("calt", false), json!({"features": {"calt": 0}}));
        assert_eq!(feature_props("kern", false), json!({"kerning": "off"}));
        assert!(feature_shown_on(&c, "calt") && !feature_shown_on(&c, "ss01") && feature_shown_on(&c, "liga"));
    }

    /// The preview is real text from the text engine, sized to its slot; nothing to draw is no
    /// image, never a panic.
    #[test]
    fn preview_renders_the_face_into_its_slot() {
        let mut e = photocraft_text::TextEngine::new();
        let c = CharStyle { font_family: "Inter".into(), ..Default::default() };
        let img = render(&mut e, "Ra", &c, [200, 80], false).expect("an image");
        assert!(img.size[0] <= 200 && img.size[1] <= 80 && img.size[1] > 40, "{:?}", img.size);
        assert!(img.pixels.iter().any(|p| p.a() > 200));
        let wrapped = render(&mut e, PANGRAM, &c, [270, 96], true).expect("wrapped");
        assert!(wrapped.size[0] <= 270 && wrapped.size[1] <= 96);
        let tnum = CharStyle { features: vec![FontFeature { tag: "tnum".into(), value: 1 }], ..c.clone() };
        assert_ne!(
            render(&mut e, "1111", &c, [200, 80], false).map(|i| i.size),
            render(&mut e, "1111", &tnum, [200, 80], false).map(|i| i.size),
            "tabular ones are wider"
        );
        assert!(render(&mut e, "", &c, [200, 80], false).is_none());
        assert!(render(&mut e, "Ra", &c, [0, 0], false).is_none());
        assert!(render(&mut e, "Ra", &c, [100_000, 80], false).is_none());
        let huge = CharStyle { size_pt: f32::NAN, variations: vec![FontVariation { axis: "wght".into(), value: f32::INFINITY }], ..c };
        let _ = render(&mut e, "Ra", &huge, [200, 80], false);
    }
}
