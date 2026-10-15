//! The current text model and a cache-independent view for the document-wide text engine.
//! Numeric field names follow the public ag-psd EngineData2 map and Photoshop-authored
//! corpus observations. Existing resources and unmodelled properties remain in place.

use std::collections::HashSet;

use photocraft_doc::TextLayer;
use photocraft_doc::text::{AntiAlias, CharStyle, Orientation, TextShape};

use crate::engine_data::{self as ed, Value as E};

fn dict(items: impl IntoIterator<Item = (&'static str, E)>) -> E {
    E::Dict(items.into_iter().map(|(key, value)| (key.into(), value)).collect())
}

fn dictionary(value: Option<&E>) -> E {
    value.filter(|value| matches!(value, E::Dict(_))).cloned().unwrap_or_else(E::dict)
}

fn set_path(value: &mut E, path: &[&str], replacement: E) {
    let Some((key, rest)) = path.split_first() else { return };
    if rest.is_empty() {
        value.set(key, replacement);
        return;
    }
    if !matches!(value.get(key), Some(E::Dict(_))) {
        value.set(key, E::dict());
    }
    if let Some(child) = value.get_mut(key) {
        set_path(child, rest, replacement);
    }
}

fn merge(target: &mut E, source: &E) {
    if let E::Dict(items) = source {
        for (key, value) in items {
            target.set(key, value.clone());
        }
    }
}

fn array_at(value: &E, path: &[&str]) -> Vec<E> {
    value.path(path).and_then(E::as_array).unwrap_or_default().to_vec()
}

/// Global paragraph /6 references another ParagraphSheet, not a character style.
/// Validate the entire named-sheet graph, including sheets no current run references.
/// Iterative traversal keeps even hostile deep or cyclic input off the call stack.
pub(super) fn paragraph_parents(sheets: &[E]) -> Result<Vec<Option<usize>>, String> {
    let mut parents = Vec::with_capacity(sheets.len());
    for (at, entry) in sheets.iter().enumerate() {
        let Some(sheet @ E::Dict(_)) = entry.get("0") else {
            return Err(format!("paragraph style {at}: its named sheet is not a dictionary"));
        };
        let parent = match sheet.get("6") {
            None => None,
            Some(value) => {
                let parent = match value {
                    E::Int(index) => usize::try_from(*index).ok(),
                    _ => None,
                }
                .filter(|index| *index < sheets.len())
                .ok_or_else(|| format!("paragraph style {at}: its parent reference is invalid or out of range"))?;
                Some(parent)
            }
        };
        parents.push(parent);
    }
    let mut states = vec![0u8; parents.len()];
    let mut path = Vec::new();
    for start in 0..parents.len() {
        path.clear();
        let mut current = Some(start);
        while let Some(at) = current {
            match states.get(at).copied() {
                Some(2) => break,
                Some(1) => return Err(format!("paragraph style ancestry contains a cycle at style {at}")),
                Some(0) => {}
                _ => return Err("paragraph style ancestry contains an invalid parent reference".into()),
            }
            if let Some(state) = states.get_mut(at) {
                *state = 1;
            }
            path.push(at);
            current = parents.get(at).copied().flatten();
        }
        for &at in &path {
            if let Some(state) = states.get_mut(at) {
                *state = 2;
            }
        }
    }
    Ok(parents)
}

fn paragraph_sheets(resources: &E) -> Result<&[E], String> {
    match resources.get("6") {
        None => Ok(&[]),
        Some(set @ E::Dict(_)) => match set.get("0") {
            None => Ok(&[]),
            Some(E::Array(sheets)) => Ok(sheets),
            _ => Err("the named paragraph style resources are not an array".into()),
        },
        _ => Err("the paragraph style resources are not a dictionary".into()),
    }
}

fn validate_paragraph_run_parents(objects: &[E], sheet_count: usize) -> Result<(), String> {
    for (object_at, object) in objects.iter().enumerate() {
        let runs = object.path(&["0", "5", "0"]).and_then(E::as_array).unwrap_or_default();
        for (run_at, run) in runs.iter().enumerate() {
            let Some(parent) = run.path(&["0", "0", "6"]) else { continue };
            let valid = match parent {
                E::Int(index) => usize::try_from(*index).ok().is_some_and(|index| index < sheet_count),
                _ => false,
            };
            if !valid {
                return Err(format!("text object {object_at}, paragraph run {run_at}: its parent reference is invalid or out of range"));
            }
        }
    }
    Ok(())
}

pub(super) fn repair_factory_paragraph_parent(resources: &mut E) {
    let Some(E::Array(sheets)) = resources.get_mut("6").and_then(|set| set.get_mut("0")) else { return };
    let Some(sheet) = sheets.first_mut().and_then(|entry| entry.get_mut("0")) else { return };
    // Our original fresh writer made Normal RGB resource 0 its own parent. Native live
    // type editing then recursively copied that sheet until the Photoshop stack overflowed.
    // Repair only that proven factory signature; every other invalid graph is an error.
    if sheet.get("0").and_then(E::as_str) == Some("Normal RGB")
        && matches!(sheet.get("5"), Some(E::Dict(_)))
        && sheet.get("6") == Some(&E::Int(0))
        && let E::Dict(fields) = sheet
    {
        fields.retain(|(key, _)| key != "6");
    }
}

fn version() -> E {
    dict([("0", E::Int(7)), ("1", E::Int(11)), ("2", E::Int(0)), ("3", E::String("Photoshop".into()))])
}

fn font_index(fonts: &mut Vec<E>, name: &str) -> usize {
    if let Some(index) = fonts.iter().position(|font| font.path(&["0", "0", "0"]).and_then(E::as_str) == Some(name)) {
        return index;
    }
    let index = fonts.len();
    fonts.push(dict([("0", dict([("99", E::Name("CoolTypeFont".into())), ("0", dict([("0", E::String(name.into())), ("1", E::Int(0)), ("2", E::Int(1))]))]))]));
    index
}

fn paint(color: &E) -> E {
    dict([
        ("99", E::Name("SimplePaint".into())),
        ("0", dict([("0", color.get("Type").cloned().unwrap_or(E::Int(1))), ("1", color.get("Values").cloned().unwrap_or_else(|| E::Array(Vec::new())))])),
    ])
}

fn char_properties(style: &CharStyle, font: usize, k: f32, template: &E) -> E {
    let local = super::style_sheet_data(style, font, k);
    let mut properties = dictionary(Some(template));
    for (local_key, global_key) in [
        ("Font", "0"),
        ("FontSize", "1"),
        ("FauxBold", "2"),
        ("FauxItalic", "3"),
        ("AutoLeading", "4"),
        ("Leading", "5"),
        ("HorizontalScale", "6"),
        ("VerticalScale", "7"),
        ("Tracking", "8"),
        ("BaselineShift", "9"),
        ("FontCaps", "12"),
        ("Ligatures", "18"),
        ("DLigatures", "19"),
        ("FillFlag", "56"),
    ] {
        if let Some(value) = local.get(local_key) {
            properties.set(global_key, value.clone());
        }
    }
    properties.set("11", E::Int(super::txt2_mode(style)));
    properties.set("15", E::Int(i64::from(style.strikethrough)));
    properties.set("16", E::Int(i64::from(style.underline)));
    if let Some(color) = local.get("FillColor") {
        properties.set("53", paint(color));
    }
    for (key, tag) in super::TXT2_FEATURES {
        // The shaper enables calt when it is unspecified; Photoshop's inherited default may
        // be off. Materialize the effective value without changing native document defaults.
        let enabled = style.features.iter().any(|feature| feature.tag == tag && feature.value != 0)
            || (tag == "calt" && !style.features.iter().any(|feature| feature.tag == tag));
        properties.set(key, E::Bool(enabled));
    }
    let mut sets = 0i64;
    for feature in &style.features {
        if feature.value != 0
            && let Some(number) = feature.tag.strip_prefix("ss").and_then(|number| number.parse::<u8>().ok()).filter(|number| (1..=20).contains(number))
        {
            sets |= 1i64 << (number - 1);
        }
    }
    properties.set(super::TXT2_STYLISTIC_SETS, E::Int(sets));
    properties
}

fn para_properties(style: &photocraft_doc::text::ParagraphStyle, k: f32, template: &E) -> E {
    let local = super::paragraph_properties(style, k);
    let mut properties = dictionary(Some(template));
    for (local_key, global_key) in [
        ("Justification", "0"),
        ("FirstLineIndent", "1"),
        ("StartIndent", "2"),
        ("EndIndent", "3"),
        ("SpaceBefore", "4"),
        ("SpaceAfter", "5"),
        ("AutoLeading", "7"),
        ("AutoHyphenate", "9"),
    ] {
        if let Some(value) = local.get(local_key) {
            properties.set(global_key, value.clone());
        }
    }
    // LeadingType is not represented by ParagraphStyle. Keep the current local value rather
    // than replacing Photoshop's leading mode with paragraph_properties' writer default.
    if properties.get("8").is_none() {
        properties.set("8", E::Int(0));
    }
    properties
}

/// Functional Normal paragraph defaults observed independently in Photoshop. In particular,
/// a fresh text engine needs more than its three spacing arrays before native layout is safe.
/// These contain no text, font, glyph, or image assets; existing file values always win.
fn normal_para_defaults() -> E {
    let mut defaults = dict([
        ("0", E::Int(0)),
        ("6", E::Int(1)),
        ("7", E::Real(1.2)),
        ("8", E::Int(0)),
        ("9", E::Bool(true)),
        ("10", E::Int(6)),
        ("11", E::Int(2)),
        ("12", E::Int(2)),
        ("13", E::Int(0)),
        ("14", E::Real(36.0)),
        ("15", E::Bool(true)),
        ("16", E::Real(0.5)),
        ("17", E::Array([0.8, 1.0, 1.33].into_iter().map(E::Real).collect())),
        ("18", E::Array([0.0, 0.0, 0.0].into_iter().map(E::Real).collect())),
        ("19", E::Array([1.0, 1.0, 1.0].into_iter().map(E::Real).collect())),
        ("20", E::Int(6)),
        ("21", E::Bool(false)),
        ("22", E::Int(0)),
        ("23", E::Bool(true)),
        ("24", E::Int(0)),
        ("25", E::Int(0)),
        ("26", E::Bool(false)),
        ("27", E::Name("nil".into())),
        ("28", E::Name("nil".into())),
        ("29", E::Bool(false)),
        ("30", E::dict()),
        ("31", E::Real(36.0)),
        ("32", E::dict()),
        ("33", E::Int(0)),
        ("34", E::Int(7)),
        ("35", E::Int(1)),
        ("36", E::Name("nil".into())),
        ("37", E::Int(0)),
        ("38", E::Bool(false)),
        ("39", E::Int(0)),
        ("40", E::Int(2)),
        ("41", E::Int(0)),
        ("47", E::Bool(false)),
        ("48", E::Name("nil".into())),
    ]);
    for key in ["1", "2", "3", "4", "5"] {
        defaults.set(key, E::Real(0.0));
    }
    defaults
}

#[derive(Default)]
struct LocalStyles {
    characters: Vec<(usize, E)>,
    paragraphs: Vec<(usize, E)>,
}

fn named_sheet<'a>(set: &'a [E], sheet: &E) -> Option<&'a E> {
    if let Some(parent) = sheet.get("Parent").and_then(E::as_i64).and_then(|parent| usize::try_from(parent).ok()).and_then(|parent| set.get(parent)) {
        return Some(parent);
    }
    let name = sheet.get("Name").and_then(E::as_str)?;
    set.iter().find(|candidate| candidate.get("Name").and_then(E::as_str) == Some(name))
}

fn local_styles(layer: &TextLayer) -> LocalStyles {
    let Some(engine) = layer.psd_raw.as_deref().and_then(|raw| super::parse_tysh(raw)).and_then(|tysh| super::engine_data(&tysh.text)) else {
        return LocalStyles::default();
    };
    let text = format!("{}\r", layer.text.replace('\n', "\r"));
    if engine.path(&["EngineDict", "Editor", "Text"]).and_then(E::as_str) != Some(text.as_str()) {
        return LocalStyles::default();
    }
    let resources = engine.get("ResourceDict").or_else(|| engine.get("DocumentResources"));
    let styles = resources.and_then(|resources| resources.get("StyleSheetSet")).and_then(E::as_array).unwrap_or_default();
    let paragraphs = resources.and_then(|resources| resources.get("ParagraphSheetSet")).and_then(E::as_array).unwrap_or_default();
    let normal = |key: &str, set: &[E]| {
        resources
            .and_then(|resources| resources.get(key))
            .and_then(E::as_i64)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < set.len())
            .unwrap_or(0)
    };
    let normal_style = styles.get(normal("TheNormalStyleSheet", styles));
    let normal_para = paragraphs.get(normal("TheNormalParagraphSheet", paragraphs));
    let total = text.encode_utf16().count();
    let lengths = |run: &str| array_at(&engine, &["EngineDict", run, "RunLengthArray"]);
    let runs = |run: &str| array_at(&engine, &["EngineDict", run, "RunArray"]);
    let length = |value: &E| {
        value.as_f64().filter(|length| length.is_finite() && *length >= 0.0 && length.fract() == 0.0).map_or(0, |length| (length as usize).min(total))
    };
    let mut resolved = LocalStyles::default();
    let mut defaults = Vec::new();
    let mut end = 0usize;
    for (run, count) in runs("ParagraphRun").iter().zip(lengths("ParagraphRun")) {
        end = end.saturating_add(length(&count)).min(total);
        let Some(sheet) = run.get("ParagraphSheet") else { continue };
        let named = named_sheet(paragraphs, sheet);
        resolved.paragraphs.push((
            end,
            super::merged(&[sheet.get("Properties"), named.and_then(|sheet| sheet.get("Properties")), normal_para.and_then(|sheet| sheet.get("Properties"))]),
        ));
        defaults.push([Some(sheet), named].into_iter().flatten().find_map(|sheet| {
            sheet.get("DefaultStyleSheet").and_then(E::as_i64).and_then(|index| usize::try_from(index).ok()).and_then(|index| styles.get(index))
        }));
    }
    let mut paragraph = 0usize;
    let mut end = 0usize;
    for (run, count) in runs("StyleRun").iter().zip(lengths("StyleRun")) {
        while resolved.paragraphs.get(paragraph).is_some_and(|(para_end, _)| end >= *para_end) && paragraph + 1 < resolved.paragraphs.len() {
            paragraph += 1;
        }
        end = end.saturating_add(length(&count)).min(total);
        let Some(sheet) = run.get("StyleSheet") else { continue };
        let named = named_sheet(styles, sheet);
        let para_default = defaults.get(paragraph).copied().flatten();
        resolved
            .characters
            .push((end, super::merged(&[Some(sheet), named, para_default, normal_style].map(|sheet| sheet.and_then(|sheet| sheet.get("StyleSheetData"))))));
    }
    resolved
}

fn local_at<'a>(runs: &'a [(usize, E)], at: usize, cursor: &mut usize) -> Option<&'a E> {
    while runs.get(*cursor).is_some_and(|(end, _)| at >= *end) && *cursor + 1 < runs.len() {
        *cursor += 1;
    }
    runs.get(*cursor).filter(|(end, _)| at < *end).map(|(_, properties)| properties)
}

fn local_piece<'a>(runs: &'a [(usize, E)], at: usize, remaining: usize, cursor: &mut usize) -> (usize, Option<&'a E>) {
    let properties = local_at(runs, at, cursor);
    let length = runs.get(*cursor).filter(|(end, _)| at < *end).map_or(remaining, |(end, _)| remaining.min(end - at));
    (length, properties)
}

fn preserve_local(target: &mut E, local: Option<&E>, fields: &[(&str, &str)]) {
    for &(local_key, global_key) in fields {
        if let Some(value) = local.and_then(|local| local.get(local_key)) {
            target.set(global_key, value.clone());
        }
    }
}

fn run_template(runs: &[E], index: usize) -> E {
    dictionary(runs.get(index).or_else(|| runs.last()))
}

fn sheet(run: &mut E, key: &str, properties: E) {
    if run.path(&["0", "0", "0"]).is_none() {
        set_path(run, &["0", "0", "0"], E::String(String::new()));
    }
    let default = if key == "6" { "5" } else { "6" };
    if run.path(&["0", "0", default]).is_none() {
        set_path(run, &["0", "0", default], E::Int(0));
    }
    set_path(run, &["0", "0", key], properties);
}

fn frame(layer: &TextLayer) -> E {
    let boxed = matches!(layer.shape, TextShape::Box { .. });
    let sentinel = if boxed { -2 } else { -1 };
    let mut data = dict([
        ("2", E::Array([1.0, 0.0, 0.0, 1.0, 0.0, 0.0].into_iter().map(E::Real).collect())),
        ("6", E::Array(vec![E::Int(sentinel), E::Int(sentinel)])),
        ("11", dict([("4", E::Int(sentinel)), ("18", E::Real(sentinel as f64)), ("22", E::Real(0.025))])),
    ]);
    if boxed {
        data.set("0", E::Int(1));
    }
    if layer.orientation == Orientation::Vertical {
        data.set("1", E::Int(2));
    }
    let mut path = dict([("2", data)]);
    if let TextShape::Box { x, y, width, height } = layer.shape {
        let (left, top, right, bottom) = (f64::from(x), f64::from(y), f64::from(x + width), f64::from(y + height));
        // Four straight cubic segments: each endpoint is repeated as its adjacent control point.
        let points = [
            [left, top],
            [left, top],
            [right, top],
            [right, top],
            [right, top],
            [right, top],
            [right, bottom],
            [right, bottom],
            [right, bottom],
            [right, bottom],
            [left, bottom],
            [left, bottom],
            [left, bottom],
            [left, bottom],
            [left, top],
            [left, top],
        ];
        path.set("1", dict([("0", E::Array(points.into_iter().flatten().map(E::Real).collect()))]));
    }
    dict([("0", path)])
}

fn finite_array(value: Option<&E>, count: Option<usize>) -> bool {
    value
        .and_then(E::as_array)
        .is_some_and(|values| count.is_none_or(|count| values.len() == count) && values.iter().all(|value| value.as_f64().is_some_and(f64::is_finite)))
}

fn valid_frame(frame: &E) -> bool {
    let Some(path) = frame.get("0") else { return false };
    let Some(data @ E::Dict(_)) = path.get("2") else { return false };
    if path.get("0").is_some() && !finite_array(path.get("0"), Some(2)) {
        return false;
    }
    if data.get("2").is_some() && !finite_array(data.get("2"), Some(6)) {
        return false;
    }
    match data.get("0").and_then(E::as_i64).unwrap_or(0) {
        0 => match data.get("6") {
            Some(range) => finite_array(Some(range), Some(2)),
            // Photoshop's current native writer can omit the optional point text range.
            // Accept only its observed complete point-path defaults, never a bare frame.
            None => {
                data.path(&["11", "4"]).and_then(E::as_f64) == Some(-1.0)
                    && data.path(&["11", "18"]).and_then(E::as_f64) == Some(-1.0)
                    && data.path(&["11", "22"]).and_then(E::as_f64) == Some(0.025)
            }
        },
        1 | 2 => path.path(&["1", "0"]).and_then(E::as_array).is_some_and(|points| !points.is_empty()) && finite_array(path.path(&["1", "0"]), None),
        _ => false,
    }
}

fn referenced_frame<'a>(reference: &E, frames: &'a [E]) -> Option<&'a E> {
    reference.as_i64().and_then(|index| usize::try_from(index).ok()).and_then(|index| frames.get(index)).filter(|frame| valid_frame(frame))
}

fn old_frame(object: &E, frames: &[E]) -> Option<E> {
    let direct = object
        .path(&["1", "0"])
        .and_then(E::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|reference| reference.get("0"))
        .find_map(|reference| referenced_frame(reference, frames));
    let cached = || {
        object
            .path(&["1", "2"])
            .and_then(E::as_array)
            .unwrap_or_default()
            .iter()
            .flat_map(|cache| cache.get("6").and_then(E::as_array).unwrap_or_default())
            .filter_map(|reference| reference.get("10"))
            .find_map(|reference| referenced_frame(reference, frames))
    };
    direct.or_else(cached).cloned()
}

fn has_view(object: &E, frames: &[E]) -> bool {
    object.path(&["1", "5"]).and_then(E::as_i64) == Some(0)
        && object.path(&["1", "0"]).and_then(E::as_array).is_some_and(|refs| {
            !refs.is_empty() && refs.iter().all(|reference| reference.get("0").and_then(|reference| referenced_frame(reference, frames)).is_some())
        })
}

fn has_coverage(model: &E, key: &str, expected: usize) -> bool {
    let Some(runs) = model.path(&[key, "0"]).and_then(E::as_array) else { return false };
    !runs.is_empty()
        && runs.iter().try_fold(0usize, |total, run| {
            let property = if key == "5" { "5" } else { "6" };
            if !matches!(run.path(&["0", "0", property]), Some(E::Dict(_))) {
                return None;
            }
            let length = run.get("1").and_then(E::as_i64).and_then(|length| usize::try_from(length).ok())?;
            total.checked_add(length)
        }) == Some(expected)
}

fn object_current(object: &E, frames: &[E], layer: &TextLayer) -> bool {
    let text = format!("{}\r", layer.text.replace('\n', "\r"));
    let count = text.encode_utf16().count();
    object.path(&["0", "0"]).and_then(E::as_str) == Some(text.as_str())
        && has_view(object, frames)
        && object.get("0").is_some_and(|model| has_coverage(model, "5", count) && has_coverage(model, "6", count))
}

/// Exactly the objects the builder cannot preserve unchanged. The exporter uses the same
/// decision before canonicalizing their local frame anchors; the two paths must not diverge.
pub(super) fn incomplete_objects(objects: &[(i32, &TextLayer)], previous: Option<&[u8]>) -> Vec<i32> {
    let previous = previous.and_then(super::parse_txt2);
    let slots = previous.as_ref().map(|tree| array_at(tree, &["1", "1"])).unwrap_or_default();
    let frames = previous.as_ref().map(|tree| array_at(tree, &["0", "8", "0"])).unwrap_or_default();
    objects
        .iter()
        .filter_map(|&(index, layer)| {
            if !(0..=super::MAX_TEXT_INDEX).contains(&index) {
                return None;
            }
            slots.get(index as usize).is_none_or(|object| !object_current(object, &frames, layer)).then_some(index)
        })
        .collect()
}

/// Build current models without Photoshop's disposable paragraph/glyph caches (`view /1, /2`).
/// Changed layers' TySh transforms must already contain their global frame anchors.
/// `preserve_frames` instead clones the original frame for layers that could not be made
/// independent of it. A missing original frame is an error, never an identity substitution.
pub(super) fn build(
    objects: &[(i32, &TextLayer)],
    previous: Option<&[u8]>,
    dpi: f32,
    changed: &HashSet<i32>,
    preserve_frames: &HashSet<i32>,
) -> Result<Vec<u8>, String> {
    if let Some((index, _)) = objects.iter().find(|(index, _)| !(0..=super::MAX_TEXT_INDEX).contains(index)) {
        return Err(format!("text object {index}: its index is outside 0..={}", super::MAX_TEXT_INDEX));
    }
    let previous_tree = previous.and_then(super::parse_txt2);
    let mut outer = dictionary(previous_tree.as_ref());
    if outer.get("98").is_none() {
        outer.set("98", dict([("0", E::Int(14))]));
    }
    let mut resources = dictionary(outer.get("0"));
    // This resource repair must run even when every object is current and unchanged.
    // It changes no text, frame or layout cache, and valid native hierarchies stay intact.
    repair_factory_paragraph_parent(&mut resources);
    paragraph_parents(paragraph_sheets(&resources)?)?;
    let mut engine = dictionary(outer.get("1"));
    let mut fonts = array_at(&resources, &["1", "0"]);
    let mut frames = array_at(&resources, &["8", "0"]);
    let mut slots = array_at(&engine, &["1"]);
    let k = 72.0 / if dpi.is_finite() && dpi > 0.0 { dpi } else { 72.0 };
    let first_style = objects.first().and_then(|(_, layer)| layer.char_runs().first().map(|run| run.style.clone())).unwrap_or_default();
    let first_font = font_index(&mut fonts, &super::postscript_for(&first_style));
    let base_char = engine.get("2").cloned().unwrap_or_else(|| char_properties(&first_style, first_font, k, &E::dict()));
    let mut base_para = normal_para_defaults();
    match engine.get("3") {
        Some(previous @ E::Dict(_)) => merge(&mut base_para, previous),
        _ => base_para = para_properties(&Default::default(), k, &base_para),
    }
    let mut rebuilding = false;

    for &(index, layer) in objects {
        if !(0..=super::MAX_TEXT_INDEX).contains(&index) {
            continue;
        }
        let at = index as usize;
        let text = format!("{}\r", layer.text.replace('\n', "\r"));
        let mut object = dictionary(slots.get(at));
        if !changed.contains(&index) && object_current(&object, &frames, layer) {
            continue;
        }
        rebuilding = true;
        if old_frame(&object, &frames).and_then(|frame| frame.path(&["0", "2", "0"]).and_then(E::as_i64)) == Some(2) {
            return Err(format!("text object {index}: editing its native path geometry is not supported"));
        }
        if object.path(&["0", "0"]).and_then(E::as_str) != Some(text.as_str())
            && let E::Dict(items) = &mut object
        {
            items.retain(|(key, _)| key != "21");
        }
        let retained = if preserve_frames.contains(&index) {
            Some(
                old_frame(&object, &frames)
                    .ok_or_else(|| format!("text object {index}: its native frame is missing or invalid; its current position cannot be preserved"))?,
            )
        } else {
            None
        };
        let mut model = dictionary(object.get("0"));
        let old_char = array_at(&model, &["6", "0"]);
        let old_para = array_at(&model, &["5", "0"]);
        let local = local_styles(layer);
        let char_runs = layer.char_runs();
        let mut styles = Vec::new();
        let mut char_at = 0usize;
        let mut char_cursor = 0usize;
        for (run_index, _, length) in super::pair_runs(&text, &char_runs) {
            let Some(current) = char_runs.get(run_index) else { continue };
            let font = font_index(&mut fonts, &super::postscript_for(&current.style));
            let mut remaining = length;
            while remaining > 0 {
                // Unsupported raw attributes can change inside a model run the importer
                // merged. Keep both the model's boundaries and the current local boundaries.
                let (length, properties) = local_piece(&local.characters, char_at, remaining, &mut char_cursor);
                let mut run = run_template(&old_char, styles.len());
                let mut template = dictionary(Some(&base_char));
                if let Some(old) = run.path(&["0", "0", "6"]) {
                    merge(&mut template, old);
                }
                preserve_local(&mut template, properties, &[("FontBaseline", "13"), ("BaselineDirection", "35"), ("Language", "38"), ("NoBreak", "52")]);
                sheet(&mut run, "6", char_properties(&current.style, font, k, &template));
                run.set("1", E::Int(i64::try_from(length).unwrap_or(i64::MAX)));
                styles.push(run);
                char_at = char_at.saturating_add(length);
                remaining -= length;
            }
        }
        let para_runs = layer.paragraph_runs();
        let mut paragraphs = Vec::new();
        let mut byte_at = 0usize;
        let mut para_at = 0usize;
        let mut para_cursor = 0usize;
        for (i, current) in para_runs.iter().enumerate() {
            let end = byte_at.saturating_add(current.len).min(layer.text.len());
            let length = layer.text.get(byte_at..end).map_or(0, |piece| piece.encode_utf16().count()) + usize::from(i + 1 == para_runs.len());
            byte_at = end;
            let mut remaining = length;
            while remaining > 0 {
                let (length, properties) = local_piece(&local.paragraphs, para_at, remaining, &mut para_cursor);
                let mut run = run_template(&old_para, paragraphs.len());
                let mut template = dictionary(Some(&base_para));
                if let Some(old) = run.path(&["0", "0", "5"]) {
                    merge(&mut template, old);
                }
                preserve_local(
                    &mut template,
                    properties,
                    &[
                        ("LeadingType", "8"),
                        ("HyphenatedWordSize", "10"),
                        ("PreHyphen", "11"),
                        ("PostHyphen", "12"),
                        ("Zone", "14"),
                        ("WordSpacing", "17"),
                        ("LetterSpacing", "18"),
                        ("GlyphSpacing", "19"),
                    ],
                );
                sheet(&mut run, "5", para_properties(&current.style, k, &template));
                run.set("1", E::Int(i64::try_from(length).unwrap_or(i64::MAX)));
                paragraphs.push(run);
                para_at = para_at.saturating_add(length);
                remaining -= length;
            }
        }
        if model.get("0").and_then(E::as_str) != Some(text.as_str()) {
            model.set("0", E::String(text));
        }
        set_path(&mut model, &["5", "0"], E::Array(paragraphs));
        set_path(&mut model, &["6", "0"], E::Array(styles));
        let mut rendering = dictionary(model.get("10"));
        rendering.set(
            "0",
            E::Int(match layer.antialias {
                AntiAlias::None => 0,
                AntiAlias::Crisp => 1,
                AntiAlias::Strong => 2,
                AntiAlias::Smooth => 3,
                AntiAlias::Sharp => 4,
                AntiAlias::Windows | AntiAlias::WindowsLcd => 3,
            }),
        );
        let fractional = layer
            .psd_raw
            .as_deref()
            .and_then(|raw| super::parse_tysh(raw))
            .and_then(|tysh| super::engine_data(&tysh.text))
            .and_then(|engine| engine.path(&["EngineDict", "UseFractionalGlyphWidths"]).and_then(E::as_bool))
            .unwrap_or(true);
        rendering.set("2", E::Bool(fractional));
        model.set("10", rendering);
        object.set("0", model);
        let frame_index = frames.len();
        frames.push(retained.unwrap_or_else(|| frame(layer)));
        let mut view = dictionary(object.get("1"));
        if let E::Dict(items) = &mut view {
            items.retain(|(key, _)| key != "1" && key != "2");
        }
        view.set("0", E::Array(vec![dict([("0", E::Int(i64::try_from(frame_index).unwrap_or(i64::MAX)))])]));
        if view.get("4").is_none() {
            view.set("4", version());
        }
        view.set("5", E::Int(0));
        object.set("1", view);
        if slots.len() <= at {
            slots.resize(at + 1, E::dict());
        }
        if let Some(slot) = slots.get_mut(at) {
            *slot = object;
        }
    }
    set_path(&mut resources, &["1", "0"], E::Array(fonts));
    set_path(&mut resources, &["8", "0"], E::Array(frames));
    if resources.get("5").is_none() {
        resources.set(
            "5",
            dict([
                ("0", E::Array(vec![dict([("0", dict([("0", E::String("Normal RGB".into())), ("6", base_char.clone())]))])])),
                ("1", E::Array(vec![dict([("0", E::Int(0))])])),
            ]),
        );
    }
    if resources.get("6").is_none() {
        resources.set(
            "6",
            dict([
                ("0", E::Array(vec![dict([("0", dict([("0", E::String("Normal RGB".into())), ("5", base_para.clone())]))])])),
                ("1", E::Array(vec![dict([("0", E::Int(0))])])),
            ]),
        );
    }
    if engine.get("0").is_none() {
        engine.set(
            "0",
            dict([
                ("1", E::Int(0)),
                ("2", E::Int(0)),
                ("3", E::Real(0.0)),
                ("4", E::Real(0.0)),
                ("5", E::Real(0.0)),
                ("6", E::Real(0.0)),
                ("7", E::Real(0.0)),
                ("8", E::Bool(true)),
                ("16", E::Bool(true)),
                ("17", E::Int(0)),
            ]),
        );
    }
    if engine.get("2").is_none() {
        engine.set("2", base_char);
    }
    if rebuilding {
        // Add only missing Normal defaults. Re-saving a complete untouched native block must
        // not rewrite it just because another Photoshop version used a sparser dictionary.
        if let Some(normal) = resources.get_mut("6").and_then(|styles| styles.get_mut("0")).and_then(|styles| match styles {
            E::Array(styles) => styles.first_mut(),
            _ => None,
        }) {
            let mut defaults = normal_para_defaults();
            if let Some(previous) = normal.path(&["0", "5"]) {
                merge(&mut defaults, previous);
            }
            set_path(normal, &["0", "5"], defaults);
        }
        engine.set("3", base_para);
    }
    if engine.get("4").is_none() {
        engine.set("4", version());
    }
    paragraph_parents(paragraph_sheets(&resources)?)?;
    validate_paragraph_run_parents(&slots, paragraph_sheets(&resources)?.len())?;
    engine.set("1", E::Array(slots));
    outer.set("0", resources);
    outer.set("1", engine);
    if let (Some(bytes), Some(old)) = (previous, previous_tree.as_ref())
        && *old == outer
    {
        return Ok(bytes.to_vec());
    }
    match outer {
        E::Dict(items) => Ok(ed::write_bare(&items)),
        _ => Err("the document text engine could not be serialized as a dictionary".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_data::ByteString;
    use photocraft_doc::text::{FontFeature, ParagraphRun, ParagraphStyle, TextAlign, TextRun};

    fn layer(text: &str) -> TextLayer {
        let style = CharStyle { font_family: "Inter".into(), size_pt: 48.0, ..Default::default() };
        let mut layer = TextLayer { text: text.into(), runs: vec![TextRun { len: text.len(), style }], ..Default::default() };
        layer.sync_summary();
        layer
    }

    fn attach_local(layer: &mut TextLayer, engine: &E) {
        let mut tysh = super::super::parse_tysh(&super::super::build_tysh(layer, 72.0, None)).unwrap();
        let (_, data) = tysh.text.items.iter_mut().find(|(key, _)| key.is("EngineData")).unwrap();
        *data = photocraft_psd::descriptor::Value::RawData(ed::write(engine));
        layer.psd_raw = Some(std::sync::Arc::new(super::super::write_tysh(&tysh)));
    }

    fn paragraph_graph_fixture(layer: &TextLayer, sheets: Vec<E>) -> Vec<u8> {
        let bytes = build(&[(0, layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&bytes).unwrap();
        set_path(&mut tree, &["0", "6", "0"], E::Array(sheets));
        let E::Dict(items) = tree else { panic!() };
        ed::write_bare(&items)
    }

    fn paragraph_sheet(name: &str, parent: Option<E>) -> E {
        let mut inner = dict([("0", E::String(name.into())), ("5", dict([("Opaque", E::ByteString(ByteString::new(vec![0, 255, 17])))]))]);
        if let Some(parent) = parent {
            inner.set("6", parent);
        }
        dict([("0", inner)])
    }

    fn assert_invalid_complete_txt2_index_is_an_error(index: i32) {
        let layer = layer("current");
        let previous = super::super::build_complete_txt2(&[(0, &layer)], None, 72.0, &HashSet::new(), &HashSet::new()).unwrap();
        for previous in [None, Some(previous.as_slice())] {
            for objects in [vec![(index, &layer)], vec![(0, &layer), (index, &layer)], vec![(index, &layer), (0, &layer)]] {
                let result = super::super::build_complete_txt2(&objects, previous, 72.0, &HashSet::new(), &HashSet::new());
                assert!(result.is_err(), "invalid text object index {index} must reject the entire public export instead of silently skipping its text");
            }
        }
    }

    #[test]
    fn complete_txt2_rejects_negative_object_indexes() {
        assert_invalid_complete_txt2_index_is_an_error(-1);
    }

    #[test]
    fn complete_txt2_rejects_indexes_above_its_limit() {
        assert_invalid_complete_txt2_index_is_an_error(super::super::MAX_TEXT_INDEX + 1);
    }

    #[test]
    fn complete_txt2_rejects_extreme_object_indexes() {
        assert_invalid_complete_txt2_index_is_an_error(i32::MAX);
    }

    #[test]
    fn complete_txt2_preserves_the_maximum_valid_object_index() {
        let layer = layer("current");
        let index = super::super::MAX_TEXT_INDEX;
        let output = super::super::build_complete_txt2(&[(index, &layer)], None, 72.0, &HashSet::new(), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&output).unwrap();
        let slots = tree.path(&["1", "1"]).and_then(E::as_array).unwrap();
        assert_eq!(slots.len(), index as usize + 1);
        assert_eq!(slots[index as usize].path(&["0", "0"]).and_then(E::as_str), Some("current\r"));
    }

    #[test]
    fn fresh_normal_paragraph_is_a_root_instead_of_its_own_parent() {
        let layer = layer("current");
        let output = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&output).unwrap();
        let normal = &tree.path(&["0", "6", "0"]).unwrap().as_array().unwrap()[0];
        assert!(normal.path(&["0", "6"]).is_none(), "a named paragraph sheet must not inherit from itself");
        let object = &tree.path(&["1", "1"]).unwrap().as_array().unwrap()[0];
        assert_eq!(
            object.path(&["0", "5", "0"]).unwrap().as_array().unwrap()[0].path(&["0", "0", "6"]),
            Some(&E::Int(0)),
            "a paragraph run still inherits the named Normal sheet"
        );
    }

    #[test]
    fn complete_noop_repairs_only_the_old_factory_normal_parent_reference() {
        let layer = layer("current");
        let previous = paragraph_graph_fixture(&layer, vec![paragraph_sheet("Normal RGB", Some(E::Int(0)))]);
        assert!(incomplete_objects(&[(0, &layer)], Some(&previous)).is_empty(), "the resource repair must not depend on rebuilding complete objects");
        let mut expected = super::super::parse_txt2(&previous).unwrap();
        let E::Array(sheets) = expected.get_mut("0").unwrap().get_mut("6").unwrap().get_mut("0").unwrap() else { panic!() };
        let E::Dict(fields) = sheets[0].get_mut("0").unwrap() else { panic!() };
        fields.retain(|(key, _)| key != "6");
        let output = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(super::super::parse_txt2(&output), Some(expected), "only the proven factory self-reference may change");
    }

    #[test]
    fn edited_export_repairs_the_old_factory_parent_and_preserves_its_properties() {
        let mut layer = layer("before");
        let previous = paragraph_graph_fixture(&layer, vec![paragraph_sheet("Normal RGB", Some(E::Int(0)))]);
        layer.text = "after".into();
        layer.runs[0].len = layer.text.len();
        let output = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&output).unwrap();
        let normal = &tree.path(&["0", "6", "0"]).unwrap().as_array().unwrap()[0];
        assert!(normal.path(&["0", "6"]).is_none());
        assert_eq!(normal.path(&["0", "5", "Opaque"]).and_then(E::as_raw_bytes), Some(&[0, 255, 17][..]));
        let object = &tree.path(&["1", "1"]).unwrap().as_array().unwrap()[0];
        assert_eq!(object.path(&["0", "0"]).and_then(E::as_str), Some("after\r"));
    }

    #[test]
    fn a_valid_native_paragraph_parent_chain_stays_byte_identical() {
        let layer = layer("current");
        let previous = paragraph_graph_fixture(&layer, vec![paragraph_sheet("Native Normal", None), paragraph_sheet("Derived", Some(E::Int(0)))]);
        let mut tree = super::super::parse_txt2(&previous).unwrap();
        // Native Photoshop corpus files order these display references child-first;
        // the parent itself still indexes the actual named-sheet array above.
        set_path(&mut tree, &["0", "6", "1"], E::Array(vec![dict([("0", E::Int(1))]), dict([("0", E::Int(0))])]));
        let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
        let E::Array(runs) = objects[0].get_mut("0").unwrap().get_mut("5").unwrap().get_mut("0").unwrap() else { panic!() };
        set_path(&mut runs[0], &["0", "0", "6"], E::Int(1));
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);
        assert_eq!(build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap(), previous);
    }

    #[test]
    fn deep_native_paragraph_ancestry_uses_bounded_iteration() {
        let mut sheets = Vec::new();
        for index in 0..4095 {
            sheets.push(paragraph_sheet("Derived", Some(E::Int(index + 1))));
        }
        sheets.push(paragraph_sheet("Root", None));
        let parents = paragraph_parents(&sheets).unwrap();
        assert_eq!(parents.len(), 4096);
        assert_eq!(parents[0], Some(1));
        assert_eq!(parents[4095], None);
    }

    #[test]
    fn indirect_and_unreferenced_paragraph_cycles_are_errors() {
        let layer = layer("current");
        for sheets in [
            vec![paragraph_sheet("Cycle Root", Some(E::Int(1))), paragraph_sheet("Cycle Child", Some(E::Int(0)))],
            vec![paragraph_sheet("Native Normal", None), paragraph_sheet("Unreferenced", Some(E::Int(1)))],
            vec![paragraph_sheet("Unknown Root", Some(E::Int(0)))],
            vec![dict([("0", dict([("0", E::String("Normal RGB".into())), ("5", E::Bool(false)), ("6", E::Int(0))]))])],
        ] {
            let previous = paragraph_graph_fixture(&layer, sheets);
            let error = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap_err();
            assert!(error.contains("paragraph") && error.contains("cycle"), "{error}");
        }
    }

    #[test]
    fn invalid_paragraph_parent_indices_are_errors_instead_of_roots() {
        let layer = layer("current");
        for parent in [E::Int(-1), E::Int(1), E::Real(0.0), E::Real(0.5), E::Name("nil".into())] {
            let previous = paragraph_graph_fixture(&layer, vec![paragraph_sheet("Native Normal", Some(parent))]);
            let error = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap_err();
            assert!(error.contains("paragraph") && error.contains("parent"), "{error}");
        }
    }

    #[test]
    fn paragraph_references_require_native_integer_tokens() {
        for parent in [E::Real(0.0), E::Real(f64::NAN), E::Real(f64::INFINITY), E::Real(f64::NEG_INFINITY), E::Bool(false)] {
            let sheets = [paragraph_sheet("Root", None), paragraph_sheet("Derived", Some(parent))];
            assert!(paragraph_parents(&sheets).is_err());
        }
    }

    #[test]
    fn complete_noop_rejects_invalid_paragraph_run_parent_references() {
        let layer = layer("current");
        let first = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        for parent in [E::Int(-1), E::Int(1), E::Real(0.0), E::Name("nil".into())] {
            let mut tree = super::super::parse_txt2(&first).unwrap();
            let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
            let E::Array(runs) = objects[0].get_mut("0").unwrap().get_mut("5").unwrap().get_mut("0").unwrap() else { panic!() };
            set_path(&mut runs[0], &["0", "0", "6"], parent);
            let E::Dict(items) = tree else { panic!() };
            let previous = ed::write_bare(&items);
            assert!(incomplete_objects(&[(0, &layer)], Some(&previous)).is_empty());
            let error = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap_err();
            assert!(error.contains("paragraph run") && error.contains("parent"), "{error}");
        }
    }

    #[test]
    fn a_paragraph_run_cannot_reference_an_empty_named_sheet_table() {
        let layer = layer("current");
        let previous = paragraph_graph_fixture(&layer, Vec::new());
        let error = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap_err();
        assert!(error.contains("paragraph run") && error.contains("parent"), "{error}");
    }

    #[test]
    fn unreferenced_global_objects_also_require_valid_paragraph_run_parents() {
        let layer = layer("current");
        let first = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&first).unwrap();
        let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
        let mut orphan = objects[0].clone();
        let E::Array(runs) = orphan.get_mut("0").unwrap().get_mut("5").unwrap().get_mut("0").unwrap() else { panic!() };
        set_path(&mut runs[0], &["0", "0", "6"], E::Int(7));
        objects.push(orphan);
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);
        let error = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap_err();
        assert!(error.contains("text object 1") && error.contains("paragraph run"), "{error}");
    }

    #[test]
    fn empty_global_slots_do_not_require_paragraph_parent_references() {
        let layer = layer("current");
        let first = build(&[(4, &layer)], None, 72.0, &HashSet::from([4]), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&first).unwrap();
        let objects = tree.path(&["1", "1"]).unwrap().as_array().unwrap();
        assert_eq!(objects.len(), 5);
        assert!(objects[..4].iter().all(|object| *object == E::dict()));
        assert_eq!(build(&[(4, &layer)], Some(&first), 72.0, &HashSet::new(), &HashSet::new()).unwrap(), first);
    }

    #[test]
    fn complete_inline_paragraph_runs_without_a_parent_remain_byte_identical() {
        let layer = layer("current");
        let first = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&first).unwrap();
        let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
        let E::Array(runs) = objects[0].get_mut("0").unwrap().get_mut("5").unwrap().get_mut("0").unwrap() else { panic!() };
        let E::Dict(fields) = runs[0].get_mut("0").unwrap().get_mut("0").unwrap() else { panic!() };
        fields.retain(|(key, _)| key != "6");
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);
        assert_eq!(build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap(), previous);
    }

    #[test]
    fn fresh_paragraph_defaults_include_the_native_layout_schema_without_overwriting_file_values() {
        let layer = layer("factory");
        let first = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&first).unwrap();
        let defaults = tree.path(&["1", "3"]).unwrap();
        let E::Dict(fields) = defaults else { panic!("paragraph defaults must be a dictionary") };
        assert_eq!(fields.len(), 44, "the sparse defaults and spacing-only subset hang Photoshop's fresh layout engine");
        let normal = &tree.path(&["0", "6", "0"]).unwrap().as_array().unwrap()[0];
        assert_eq!(normal.path(&["0", "5"]), Some(defaults));
        for (key, expected) in [("17", [0.8, 1.0, 1.33]), ("18", [0.0, 0.0, 0.0]), ("19", [1.0, 1.0, 1.0])] {
            assert_eq!(defaults.get(key).unwrap().as_array().unwrap().iter().map(|value| value.as_f64().unwrap()).collect::<Vec<_>>(), expected);
        }
        for key in ["27", "28", "36", "48"] {
            assert_eq!(defaults.get(key), Some(&E::Name("nil".into())));
        }
        assert_eq!(defaults.get("30"), Some(&E::dict()));
        assert_eq!(defaults.get("32"), Some(&E::dict()));
        assert_eq!(defaults.get("9").and_then(E::as_bool), Some(false), "the current model overrides the factory hyphenation flag");

        let custom = E::Array([0.9, 1.05, 1.5].into_iter().map(E::Real).collect());
        set_path(&mut tree, &["1", "3", "17"], custom.clone());
        set_path(&mut tree, &["1", "3", "Opaque"], E::ByteString(ByteString::new(vec![0, 255, 127])));
        let old_normal = tree.path(&["0", "6"]).unwrap().clone();
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);
        let saved = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let saved = super::super::parse_txt2(&saved).unwrap();
        assert_eq!(saved.path(&["1", "3", "17"]), Some(&custom));
        assert_eq!(saved.path(&["1", "3", "Opaque"]).and_then(E::as_raw_bytes), Some(&[0, 255, 127][..]));
        assert_eq!(saved.path(&["0", "6"]), Some(&old_normal), "native Normal properties must not be replaced by document defaults");
    }

    #[test]
    fn current_local_named_sheets_preserve_unmodeled_fields_at_utf16_run_boundaries() {
        let mut layer = layer("A\u{1f680}\nB");
        layer.runs = vec![TextRun { len: 6, style: layer.runs[0].style.clone() }, TextRun { len: 1, style: layer.runs[0].style.clone() }];
        layer.paragraphs = vec![
            ParagraphRun { len: 6, style: ParagraphStyle { align: TextAlign::Center, ..Default::default() } },
            ParagraphRun { len: 1, style: ParagraphStyle { align: TextAlign::Right, ..Default::default() } },
        ];
        let mut engine = super::super::build_engine_data(&layer, None, 72.0);
        let word = E::Array([0.7, 0.9, 1.2].into_iter().map(E::Real).collect());
        let letter = E::Array([0.1, 0.2, 0.3].into_iter().map(E::Real).collect());
        set_path(
            &mut engine,
            &["ResourceDict", "ParagraphSheetSet"],
            E::Array(vec![
                dict([
                    ("Name", E::String("Normal".into())),
                    ("Properties", dict([("LeadingType", E::Int(0)), ("GlyphSpacing", E::Array([1.0, 1.0, 1.0].into_iter().map(E::Real).collect()))])),
                ]),
                dict([
                    ("Name", E::String("Named".into())),
                    ("DefaultStyleSheet", E::Int(1)),
                    (
                        "Properties",
                        dict([
                            ("LeadingType", E::Int(1)),
                            ("HyphenatedWordSize", E::Int(12)),
                            ("PreHyphen", E::Int(3)),
                            ("PostHyphen", E::Int(4)),
                            ("Zone", E::Real(25.0)),
                            ("WordSpacing", word.clone()),
                            ("LetterSpacing", letter.clone()),
                        ]),
                    ),
                ]),
            ]),
        );
        set_path(
            &mut engine,
            &["ResourceDict", "StyleSheetSet"],
            E::Array(vec![
                dict([
                    ("Name", E::String("Normal".into())),
                    (
                        "StyleSheetData",
                        dict([("FontBaseline", E::Int(0)), ("BaselineDirection", E::Int(2)), ("Language", E::Int(0)), ("NoBreak", E::Bool(false))]),
                    ),
                ]),
                dict([
                    ("Name", E::String("Paragraph default".into())),
                    ("StyleSheetData", dict([("FontBaseline", E::Int(1)), ("Language", E::Int(9)), ("NoBreak", E::Bool(true))])),
                ]),
                dict([
                    ("Name", E::String("Named char".into())),
                    ("StyleSheetData", dict([("FontBaseline", E::Int(2)), ("Language", E::Int(7)), ("NoBreak", E::Bool(false))])),
                ]),
            ]),
        );
        set_path(
            &mut engine,
            &["EngineDict", "ParagraphRun", "RunArray"],
            E::Array(vec![
                dict([("ParagraphSheet", dict([("Parent", E::Int(1)), ("Properties", dict([("Justification", E::Int(0))]))]))]),
                dict([("ParagraphSheet", dict([("Name", E::String("Named".into())), ("Properties", dict([("LeadingType", E::Int(0))]))]))]),
            ]),
        );
        let local_runs = engine.get_mut("EngineDict").unwrap().get_mut("StyleRun").unwrap().get_mut("RunArray").unwrap();
        let E::Array(local_runs) = local_runs else { panic!() };
        local_runs[1].get_mut("StyleSheet").unwrap().set("Parent", E::Int(2));
        attach_local(&mut layer, &engine);

        let saved = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&saved).unwrap();
        let model = tree.path(&["1", "1"]).unwrap().as_array().unwrap()[0].get("0").unwrap();
        let para = model.path(&["5", "0"]).unwrap().as_array().unwrap();
        let properties = |index: usize| para[index].path(&["0", "0", "5"]).unwrap();
        assert_eq!((properties(0).get("8").and_then(E::as_i64), properties(1).get("8").and_then(E::as_i64)), (Some(1), Some(0)));
        assert_eq!(
            (properties(0).get("0").and_then(E::as_i64), properties(1).get("0").and_then(E::as_i64)),
            (Some(2), Some(1)),
            "the typed current model still wins for supported fields"
        );
        assert_eq!(properties(0).get("17"), Some(&word));
        assert_eq!(properties(1).get("18"), Some(&letter));
        assert_eq!(properties(0).get("10").and_then(E::as_i64), Some(12));
        assert_eq!(properties(0).get("14").and_then(E::as_f64), Some(25.0));
        let chars = model.path(&["6", "0"]).unwrap().as_array().unwrap();
        let properties = |index: usize| chars[index].path(&["0", "0", "6"]).unwrap();
        assert_eq!((properties(0).get("13").and_then(E::as_i64), properties(1).get("13").and_then(E::as_i64)), (Some(1), Some(2)));
        assert_eq!((properties(0).get("38").and_then(E::as_i64), properties(1).get("38").and_then(E::as_i64)), (Some(9), Some(7)));
        assert_eq!((properties(0).get("52").and_then(E::as_bool), properties(1).get("52").and_then(E::as_bool)), (Some(true), Some(false)));
        assert_eq!(properties(1).get("35").and_then(E::as_i64), Some(2));

        layer.text = "different".into();
        layer.runs = vec![TextRun { len: layer.text.len(), style: layer.runs[0].style.clone() }];
        layer.paragraphs.clear();
        let stale = local_styles(&layer);
        assert!(stale.characters.is_empty() && stale.paragraphs.is_empty(), "old local runs must not be applied to different current text");
    }

    #[test]
    fn merged_model_runs_split_at_current_local_utf16_character_and_paragraph_boundaries() {
        let mut layer = layer("A\u{1f680}\nB");
        let style = layer.runs[0].style.clone();
        let paragraph = ParagraphStyle { align: TextAlign::Center, ..Default::default() };
        layer.runs = vec![TextRun { len: 6, style: style.clone() }, TextRun { len: 1, style: style.clone() }];
        layer.paragraphs = vec![ParagraphRun { len: 6, style: paragraph.clone() }, ParagraphRun { len: 1, style: paragraph.clone() }];
        let mut engine = super::super::build_engine_data(&layer, None, 72.0);
        let E::Array(runs) = engine.get_mut("EngineDict").unwrap().get_mut("StyleRun").unwrap().get_mut("RunArray").unwrap() else { panic!() };
        set_path(&mut runs[0], &["StyleSheet", "StyleSheetData", "NoBreak"], E::Bool(true));
        set_path(&mut runs[1], &["StyleSheet", "StyleSheetData", "NoBreak"], E::Bool(false));
        let E::Array(runs) = engine.get_mut("EngineDict").unwrap().get_mut("ParagraphRun").unwrap().get_mut("RunArray").unwrap() else { panic!() };
        set_path(&mut runs[0], &["ParagraphSheet", "Properties", "LeadingType"], E::Int(1));
        set_path(&mut runs[1], &["ParagraphSheet", "Properties", "LeadingType"], E::Int(0));
        attach_local(&mut layer, &engine);
        // The importer can merge character runs which differ only in unsupported raw fields.
        // Also allow a typed paragraph run to span multiple current local paragraph sheets.
        layer.runs = vec![TextRun { len: layer.text.len(), style }];
        layer.paragraphs = vec![ParagraphRun { len: layer.text.len(), style: paragraph }];

        let saved = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&saved).unwrap();
        let model = tree.path(&["1", "1"]).unwrap().as_array().unwrap()[0].get("0").unwrap();
        let chars = model.path(&["6", "0"]).unwrap().as_array().unwrap();
        assert_eq!(
            chars.iter().map(|run| run.get("1").and_then(E::as_i64).unwrap()).collect::<Vec<_>>(),
            [4, 2],
            "split at UTF-16 units, not UTF-8 bytes or surrogate halves"
        );
        assert_eq!(chars.iter().map(|run| run.path(&["0", "0", "6", "52"]).and_then(E::as_bool).unwrap()).collect::<Vec<_>>(), [true, false]);
        let paras = model.path(&["5", "0"]).unwrap().as_array().unwrap();
        assert_eq!(paras.iter().map(|run| run.get("1").and_then(E::as_i64).unwrap()).collect::<Vec<_>>(), [4, 2]);
        assert_eq!(paras.iter().map(|run| run.path(&["0", "0", "5", "8"]).and_then(E::as_i64).unwrap()).collect::<Vec<_>>(), [1, 0]);
        assert!(
            paras.iter().all(|run| run.path(&["0", "0", "5", "0"]).and_then(E::as_i64) == Some(2)),
            "supported typed formatting still covers both split pieces"
        );
    }

    #[test]
    fn new_point_text_has_full_current_model_and_a_cacheless_view() {
        let layer = layer("TEST12");
        let bytes = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&bytes).unwrap();
        let object = &tree.path(&["1", "1"]).unwrap().as_array().unwrap()[0];
        assert_eq!(object.path(&["0", "0"]).and_then(E::as_str), Some("TEST12\r"));
        let style = object.path(&["0", "6", "0"]).unwrap().as_array().unwrap()[0].path(&["0", "0", "6"]).unwrap();
        assert_eq!(style.get("1").and_then(E::as_f64), Some(48.0));
        assert_eq!(style.get("11").and_then(E::as_i64), Some(1));
        assert!(style.get("53").is_some());
        assert!(object.path(&["0", "5", "0"]).unwrap().as_array().unwrap()[0].path(&["0", "0", "5", "7"]).is_some());
        assert!(object.path(&["1", "2"]).is_none());
        assert_eq!(object.path(&["0", "10", "0"]).and_then(E::as_i64), Some(3));
        assert_eq!(object.path(&["0", "10", "2"]).and_then(E::as_bool), Some(true));
        assert_eq!(object.path(&["1", "4", "3"]).and_then(E::as_str), Some("Photoshop"));
        assert!(has_view(object, &array_at(tree.get("0").unwrap(), &["8", "0"])));
        for key in ["5", "6"] {
            assert_eq!(tree.path(&["0", key, "1"]).unwrap().as_array().unwrap()[0].get("0").and_then(E::as_i64), Some(0));
        }
        assert_eq!(tree.path(&["1", "0", "16"]).and_then(E::as_bool), Some(true));
    }

    #[test]
    fn unchanged_complete_text_keeps_the_whole_block_byte_for_byte() {
        let layer = layer("native-like");
        let first = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        assert_eq!(build(&[(0, &layer)], Some(&first), 72.0, &HashSet::new(), &HashSet::new()).unwrap(), first);
        assert!(incomplete_objects(&[(0, &layer)], Some(&first)).is_empty());
    }

    #[test]
    fn native_point_frames_without_optional_text_ranges_survive_an_unchanged_save() {
        let layer = layer("native point");
        let first = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&first).unwrap();
        let E::Array(frames) = tree.get_mut("0").unwrap().get_mut("8").unwrap().get_mut("0").unwrap() else { panic!() };
        let E::Dict(data) = frames[0].get_mut("0").unwrap().get_mut("2").unwrap() else { panic!() };
        data.retain(|(key, _)| key != "6");
        assert!(valid_frame(&frames[0]), "Photoshop's own point-path defaults are valid without the optional range");
        let mut bad = frames[0].clone();
        set_path(&mut bad, &["0", "2", "11", "4"], E::Int(0));
        assert!(!valid_frame(&bad), "missing geometry must not become valid just because the frame exists");
        let mut bad = frames[0].clone();
        set_path(&mut bad, &["0", "2", "6"], E::Array(vec![E::Real(-1.0)]));
        assert!(!valid_frame(&bad), "an explicitly malformed range must not use the optional-range fallback");
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);
        assert!(incomplete_objects(&[(0, &layer)], Some(&previous)).is_empty());
        assert_eq!(build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap(), previous);
    }

    #[test]
    fn changed_type_keeps_opaque_resources_and_unknown_style_properties() {
        let mut layer = layer("Before");
        let bytes = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&bytes).unwrap();
        set_path(&mut tree, &["0", "Opaque"], E::ByteString(ByteString::new(b"001010".to_vec())));
        let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
        let E::Array(runs) = objects[0].get_mut("0").unwrap().get_mut("6").unwrap().get_mut("0").unwrap() else { panic!() };
        set_path(&mut runs[0], &["0", "0", "6", "Unknown"], E::Int(179));
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);
        layer.text = "After \u{1f680}".into();
        layer.runs[0].len = layer.text.len();
        layer.runs[0].style.size_pt = 49.0;
        layer.runs[0].style.features.push(FontFeature { tag: "ss02".into(), value: 1 });
        let saved = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let back = super::super::parse_txt2(&saved).unwrap();
        assert_eq!(back.path(&["0", "Opaque"]).and_then(E::as_raw_bytes), Some(&b"001010"[..]));
        let style =
            back.path(&["1", "1"]).unwrap().as_array().unwrap()[0].path(&["0", "6", "0"]).unwrap().as_array().unwrap()[0].path(&["0", "0", "6"]).unwrap();
        assert_eq!(style.get("Unknown").and_then(E::as_i64), Some(179));
        assert_eq!(style.get("1").and_then(E::as_f64), Some(49.0));
        assert_eq!(style.get("85").and_then(E::as_i64), Some(2));
    }

    #[test]
    fn box_frames_use_current_local_geometry_and_vertical_orientation() {
        let mut layer = layer("box");
        layer.shape = TextShape::Box { x: 10.0, y: 20.0, width: 120.0, height: 60.0 };
        layer.orientation = Orientation::Vertical;
        let frame = frame(&layer);
        assert_eq!(frame.path(&["0", "2", "0"]).and_then(E::as_i64), Some(1));
        assert_eq!(frame.path(&["0", "2", "1"]).and_then(E::as_i64), Some(2));
        let points = frame.path(&["0", "1", "0"]).unwrap().as_array().unwrap();
        assert_eq!(points.len(), 32);
        assert_eq!(points[0].as_f64(), Some(10.0));
        assert_eq!(points[1].as_f64(), Some(20.0));
        assert_eq!(points[4].as_f64(), Some(130.0));
        assert_eq!(points[13].as_f64(), Some(80.0));
    }

    #[test]
    fn an_unrepresentable_local_transform_keeps_the_original_frame_geometry() {
        let mut layer = layer("Before");
        let bytes = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&bytes).unwrap();
        let E::Array(frames) = tree.get_mut("0").unwrap().get_mut("8").unwrap().get_mut("0").unwrap() else { panic!() };
        set_path(&mut frames[0], &["0", "0"], E::Array(vec![E::Real(120.0), E::Real(110.0)]));
        let expected = frames[0].clone();
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);
        layer.text = "After".into();
        layer.runs[0].len = layer.text.len();
        let output = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::from([0]), &HashSet::from([0])).unwrap();
        let tree = super::super::parse_txt2(&output).unwrap();
        let frames = array_at(tree.get("0").unwrap(), &["8", "0"]);
        let object = &tree.path(&["1", "1"]).unwrap().as_array().unwrap()[0];
        assert_eq!(old_frame(object, &frames), Some(expected));
        assert_eq!(object.path(&["0", "0"]).and_then(E::as_str), Some("After\r"));
        assert!(object.path(&["1", "2"]).is_none());
    }

    #[test]
    fn missing_required_native_frame_is_an_error_instead_of_a_position_change() {
        let layer = layer("current");
        let error = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::from([0])).unwrap_err();
        assert!(error.contains("native frame is missing or invalid"));
    }

    #[test]
    fn an_incomplete_unchanged_model_is_rebuilt() {
        let layer = layer("current");
        let bytes = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&bytes).unwrap();
        let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
        set_path(&mut objects[0], &["0", "5", "0"], E::Array(Vec::new()));
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);
        assert_eq!(incomplete_objects(&[(0, &layer)], Some(&previous)), [0]);
        let repaired = build(&[(0, &layer)], Some(&previous), 72.0, &HashSet::new(), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&repaired).unwrap();
        let object = &tree.path(&["1", "1"]).unwrap().as_array().unwrap()[0];
        assert!(has_coverage(object.get("0").unwrap(), "5", 8));
    }

    #[test]
    fn changed_multiline_type_rebuilds_utf16_runs_and_drops_both_old_layout_caches() {
        let mut layer = layer("old");
        let bytes = build(&[(0, &layer)], None, 72.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let mut tree = super::super::parse_txt2(&bytes).unwrap();
        let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
        set_path(&mut objects[0], &["1", "1"], dict([("0", E::Array(vec![dict([("0", dict([("1", E::Int(1))])), ("1", E::Int(4))])]))]));
        set_path(&mut objects[0], &["1", "2"], E::Array(vec![dict([("OldGlyphCache", E::Int(79))])]));
        objects[0].set("21", dict([("1", E::Array(vec![E::Real(17.0)]))]));
        let E::Dict(items) = tree else { panic!() };
        let previous = ed::write_bare(&items);

        layer.text = "A\u{1f680}\nB\n".into();
        layer.runs = vec![
            TextRun { len: 1, style: CharStyle { font_family: "Inter".into(), size_pt: 48.0, ..Default::default() } },
            TextRun {
                len: layer.text.len() - 1,
                style: CharStyle { font_family: "Pretendard".into(), postscript_name: Some("Pretendard-Bold".into()), size_pt: 32.0, ..Default::default() },
            },
        ];
        layer.paragraphs = vec![
            ParagraphRun { len: 6, style: ParagraphStyle::default() },
            ParagraphRun { len: 2, style: ParagraphStyle { align: TextAlign::Right, ..Default::default() } },
            ParagraphRun { len: 0, style: ParagraphStyle { align: TextAlign::Center, ..Default::default() } },
        ];
        let output = build(&[(0, &layer)], Some(&previous), 144.0, &HashSet::from([0]), &HashSet::new()).unwrap();
        let tree = super::super::parse_txt2(&output).unwrap();
        let object = &tree.path(&["1", "1"]).unwrap().as_array().unwrap()[0];
        assert!(object.path(&["1", "1"]).is_none(), "paragraph cache lengths from the old text must be discarded");
        assert!(object.path(&["1", "2"]).is_none(), "old glyph positions must be discarded");
        assert!(object.get("21").is_none(), "legacy glyph positions must also be discarded after a text edit");
        assert_eq!(object.path(&["0", "0"]).and_then(E::as_str), Some("A\u{1f680}\rB\r\r"));
        let styles = object.path(&["0", "6", "0"]).unwrap().as_array().unwrap();
        assert_eq!(styles.iter().map(|run| run.get("1").and_then(E::as_i64).unwrap()).collect::<Vec<_>>(), [1, 6]);
        assert_eq!(styles[0].path(&["0", "0", "6", "1"]).and_then(E::as_f64), Some(96.0));
        assert_eq!(styles[1].path(&["0", "0", "6", "1"]).and_then(E::as_f64), Some(64.0));
        let paragraphs = object.path(&["0", "5", "0"]).unwrap().as_array().unwrap();
        assert_eq!(paragraphs.iter().map(|run| run.get("1").and_then(E::as_i64).unwrap()).collect::<Vec<_>>(), [4, 2, 1]);
        assert_eq!(paragraphs.iter().map(|run| run.path(&["0", "0", "5", "0"]).and_then(E::as_i64).unwrap()).collect::<Vec<_>>(), [0, 1, 2]);
    }
}
