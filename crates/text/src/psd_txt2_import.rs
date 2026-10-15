//! Import Photoshop's document-wide text model when the local model is unavailable.
//! Most field names follow the public ag-psd EngineData2 map. Native files and
//! typed runtime observations establish that paragraph /6 is a ParagraphSheet
//! parent reference, not the local EngineData default character-style index.

use photocraft_doc::TextLayer;
use photocraft_doc::text::{Orientation, TextShape};

use crate::engine_data::Value as E;

fn dict(items: impl IntoIterator<Item = (&'static str, E)>) -> E {
    E::Dict(items.into_iter().map(|(key, value)| (key.into(), value)).collect())
}

fn index(value: Option<&E>) -> Option<usize> {
    let number = value?.as_f64()?;
    (number.is_finite() && number >= 0.0 && number.fract() == 0.0 && number < usize::MAX as f64).then_some(number as usize)
}

// A boundary inside a surrogate pair is not a character boundary. Reject the entire
// fallback before applying anything rather than assigning part of a character another font.
fn run_lengths(text: &str, runs: &[E], length: impl Fn(&E) -> Option<&E>) -> Option<Vec<usize>> {
    if runs.is_empty() {
        return None;
    }
    let total = text.encode_utf16().count();
    let mut characters = text.chars();
    let (mut boundary, mut end) = (0usize, 0usize);
    let mut lengths = Vec::with_capacity(runs.len());
    for run in runs {
        let count = index(length(run))?;
        end = end.checked_add(count)?;
        if end > total {
            return None;
        }
        while boundary < end {
            boundary = boundary.checked_add(characters.next()?.len_utf16())?;
        }
        if boundary != end {
            return None;
        }
        lengths.push(count);
    }
    (end == total).then_some(lengths)
}

pub(super) fn local_model_present(tysh: &super::TySh) -> bool {
    let Some(engine) = super::engine_data(&tysh.text) else { return false };
    let Some(text) = engine.path(&["EngineDict", "Editor", "Text"]).and_then(E::as_str) else { return false };
    // A deliberately blank local engine is authoritative even if Txt contains characters.
    if text.trim().is_empty() {
        return true;
    }
    ["StyleRun", "ParagraphRun"].into_iter().all(|key| {
        let Some(run) = engine.path(&["EngineDict", key]) else { return false };
        let Some(runs) = run.get("RunArray").and_then(E::as_array) else { return false };
        let Some(lengths) = run.get("RunLengthArray").and_then(E::as_array) else { return false };
        if runs.len() != lengths.len()
            || runs.iter().any(|run| !matches!(run.get(if key == "StyleRun" { "StyleSheet" } else { "ParagraphSheet" }), Some(E::Dict(_))))
        {
            return false;
        }
        run_lengths(text, lengths, |value| Some(value)).is_some()
    })
}

fn resources<'a>(tree: &'a E, key: &str) -> &'a [E] {
    tree.path(&["0", key, "0"]).and_then(E::as_array).unwrap_or_default()
}

fn named_index(set: &[E], sheet: &E) -> Option<usize> {
    let name = sheet.get("0").and_then(E::as_str).filter(|name| !name.is_empty())?;
    set.iter().position(|entry| entry.path(&["0", "0"]).and_then(E::as_str) == Some(name))
}

fn named<'a>(set: &'a [E], sheet: &E) -> Option<&'a E> {
    set.get(named_index(set, sheet)?)?.get("0")
}

fn resolved_paragraph(set: &[E], parents: &[Option<usize>], sheet: &E, defaults: &E) -> Option<E> {
    let mut levels = vec![sheet.get("5")];
    let mut parent = match sheet.get("6") {
        Some(E::Int(parent)) => Some(usize::try_from(*parent).ok()?),
        Some(_) => return None,
        None => named_index(set, sheet),
    };
    let mut depth = 0usize;
    while let Some(at) = parent {
        // The shared validator checks every named sheet, including unused sheets.
        // Keep this traversal independently bounded before applying any model data.
        if depth >= set.len() {
            return None;
        }
        depth += 1;
        let E::Dict(_) = set.get(at)?.get("0")? else { return None };
        levels.push(set.get(at)?.path(&["0", "5"]));
        parent = *parents.get(at)?;
    }
    levels.push(Some(defaults));
    Some(super::merged(&levels))
}

fn character_defaults(tree: &E, styles: &[E]) -> E {
    super::merged(&[tree.path(&["1", "2"]), styles.first().and_then(|entry| entry.path(&["0", "6"]))])
}

fn resolved_character_properties(sheet: &E, named: Option<&E>, defaults: &E) -> E {
    super::merged(&[sheet.get("6"), named.and_then(|sheet| sheet.get("6")), Some(defaults)])
}

/// Resolve the properties used by the global-only flags overlay without replacing local type.
/// Photoshop can put calt, automatic kerning and stylistic sets only in Normal/default sheets.
pub(super) fn resolved_character_flags(tree: &E, model: &E) -> Option<Vec<E>> {
    let text = model.get("0").and_then(E::as_str)?;
    let runs = model.path(&["6", "0"]).and_then(E::as_array)?;
    run_lengths(text, runs, |run| run.get("1"))?;
    let styles = resources(tree, "5");
    let defaults = character_defaults(tree, styles);
    runs.iter()
        .map(|run| {
            let sheet @ E::Dict(_) = run.path(&["0", "0"])? else { return None };
            let properties = resolved_character_properties(sheet, named(styles, sheet), &defaults);
            Some(dict([("0", dict([("0", dict([("6", properties)]))])), ("1", run.get("1")?.clone())]))
        })
        .collect()
}

fn character_properties(global: &E) -> E {
    let mut local = E::dict();
    for (global_key, local_key) in [
        ("0", "Font"),
        ("1", "FontSize"),
        ("2", "FauxBold"),
        ("3", "FauxItalic"),
        ("4", "AutoLeading"),
        ("5", "Leading"),
        ("6", "HorizontalScale"),
        ("7", "VerticalScale"),
        ("8", "Tracking"),
        ("9", "BaselineShift"),
        ("12", "FontCaps"),
        ("13", "FontBaseline"),
        ("18", "Ligatures"),
        ("19", "DLigatures"),
        ("35", "BaselineDirection"),
        ("38", "Language"),
        ("52", "NoBreak"),
        ("56", "FillFlag"),
    ] {
        if let Some(value) = global.get(global_key) {
            local.set(local_key, value.clone());
        }
    }
    for (global_key, local_key) in [("15", "Strikethrough"), ("16", "Underline")] {
        if let Some(value) = global.get(global_key).and_then(E::as_bool) {
            local.set(local_key, E::Bool(value));
        }
    }
    if let Some(color) = global.path(&["53", "0"]) {
        local.set(
            "FillColor",
            dict([("Type", color.get("0").cloned().unwrap_or(E::Int(1))), ("Values", color.get("1").cloned().unwrap_or_else(|| E::Array(Vec::new())))]),
        );
    }
    // The global automatic mode is applied by apply_txt2 after importing the model.
    // Global /10 is undocumented; it must not be guessed to be a manual pair amount.
    local.set("AutoKerning", E::Bool(true));
    local
}

fn paragraph_properties(global: &E) -> E {
    let mut local = E::dict();
    for (global_key, local_key) in [
        ("0", "Justification"),
        ("1", "FirstLineIndent"),
        ("2", "StartIndent"),
        ("3", "EndIndent"),
        ("4", "SpaceBefore"),
        ("5", "SpaceAfter"),
        ("7", "AutoLeading"),
        ("8", "LeadingType"),
        ("9", "AutoHyphenate"),
        ("10", "HyphenatedWordSize"),
        ("11", "PreHyphen"),
        ("12", "PostHyphen"),
        ("14", "Zone"),
        ("17", "WordSpacing"),
        ("18", "LetterSpacing"),
        ("19", "GlyphSpacing"),
    ] {
        if let Some(value) = global.get(global_key) {
            local.set(local_key, value.clone());
        }
    }
    local
}

fn finite_properties(properties: &E, fields: &[(&str, f32)]) -> bool {
    fields.iter().all(|(key, scale)| {
        properties.get(key).is_none_or(|value| value.as_f64().is_some_and(|number| number.is_finite() && (number as f32 * scale).is_finite()))
    })
}

fn box_shape(tree: &E, object_index: usize) -> Option<TextShape> {
    let frame = super::txt2_frame(tree, object_index)?;
    if frame.path(&["2", "0"]).and_then(E::as_i64) != Some(1) {
        return None;
    }
    let points = frame.path(&["1", "0"]).and_then(E::as_array)?;
    if points.len() < 8 || points.len() % 2 != 0 {
        return None;
    }
    let (mut left, mut top, mut right, mut bottom) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for point in points.chunks_exact(2) {
        let (x, y) = (point.first()?.as_f64()?, point.get(1)?.as_f64()?);
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        (left, top, right, bottom) = (left.min(x), top.min(y), right.max(x), bottom.max(y));
    }
    // Box text is an axis-aligned rectangular path. Do not turn arbitrary text-on-path
    // curves into a bounding box, or rewrite their geometry while importing styles.
    if points.chunks_exact(2).any(|point| {
        let x = point.first().and_then(E::as_f64).unwrap_or(f64::NAN);
        let y = point.get(1).and_then(E::as_f64).unwrap_or(f64::NAN);
        (x != left && x != right) || (y != top && y != bottom)
    }) {
        return None;
    }
    let bounds = [left as f32, top as f32, (right - left) as f32, (bottom - top) as f32];
    (bounds.iter().all(|value| value.is_finite()) && bounds[2] > 0.0 && bounds[3] > 0.0).then_some(TextShape::Box {
        x: bounds[0],
        y: bounds[1],
        width: bounds[2],
        height: bounds[3],
    })
}

/// Returns the fully resolved global style runs for the existing kerning/feature overlay.
/// Conversion is atomic: stale, partial, or invalid global data cannot replace local type.
pub(super) fn apply(layer: &mut TextLayer, tysh: &super::TySh, tree: &E, model: &E, object_index: usize, dpi: f32) -> Option<Vec<E>> {
    if local_model_present(tysh) {
        return None;
    }
    let text = model.get("0").and_then(E::as_str)?;
    if text.strip_suffix('\r').unwrap_or(text).replace('\r', "\n") != layer.text {
        return None;
    }
    let photocraft_psd::descriptor::Value::Text(current) = tysh.text.get("Txt ")? else { return None };
    if current.to_string_lossy().trim_end_matches('\0').replace('\r', "\n") != layer.text {
        return None;
    }
    let paragraphs = model.path(&["5", "0"]).and_then(E::as_array)?;
    let styles = model.path(&["6", "0"]).and_then(E::as_array)?;
    let paragraph_lengths = run_lengths(text, paragraphs, |run| run.get("1"))?;
    let style_lengths = run_lengths(text, styles, |run| run.get("1"))?;
    let k = 72.0 / if dpi.is_finite() && dpi > 0.0 { dpi } else { 72.0 };
    if !k.is_finite() {
        return None;
    }
    let fonts = resources(tree, "1");
    let style_set = resources(tree, "5");
    // Earlier PhotoCraft output gave named Normal RGB resource 0 a self-parent.
    // Photoshop can omit local EngineData on save, so repair the proven factory
    // edge before recovering styles. Clone only this resource table: the input
    // tree, opaque resources, and valid local model must remain authoritative.
    let mut paragraph_resources = E::dict();
    if let Some(set) = tree.path(&["0", "6"]) {
        paragraph_resources.set("6", set.clone());
    }
    super::complete_txt2::repair_factory_paragraph_parent(&mut paragraph_resources);
    let paragraph_set = match paragraph_resources.get("6") {
        None => &[],
        Some(set @ E::Dict(_)) => match set.get("0") {
            None => &[],
            Some(E::Array(sheets)) => sheets.as_slice(),
            _ => return None,
        },
        _ => return None,
    };
    let paragraph_parents = super::complete_txt2::paragraph_parents(paragraph_set).ok()?;
    let base_character = character_defaults(tree, style_set);
    let base_paragraph = super::merged(&[tree.path(&["1", "3"]), paragraph_set.first().and_then(|entry| entry.path(&["0", "5"]))]);
    let mut paragraph_runs = Vec::with_capacity(paragraphs.len());
    for run in paragraphs {
        let sheet @ E::Dict(_) = run.path(&["0", "0"])? else { return None };
        let properties = resolved_paragraph(paragraph_set, &paragraph_parents, sheet, &base_paragraph)?;
        if !finite_properties(&properties, &[("1", k), ("2", k), ("3", k), ("4", k), ("5", k), ("7", 1.0)]) {
            return None;
        }
        paragraph_runs.push(dict([("ParagraphSheet", dict([("Properties", paragraph_properties(&properties))]))]));
    }
    let (mut paragraph, mut paragraph_end, mut at) = (0usize, paragraph_lengths.first().copied().unwrap_or(0), 0usize);
    let mut character_runs = Vec::with_capacity(styles.len());
    let mut resolved_runs = Vec::with_capacity(styles.len());
    let mut resolved_lengths = Vec::with_capacity(styles.len());
    for (run, length) in styles.iter().zip(&style_lengths) {
        let sheet @ E::Dict(_) = run.path(&["0", "0"])? else { return None };
        let own = named(style_set, sheet);
        let mut remaining = *length;
        while remaining > 0 {
            while at >= paragraph_end && paragraph + 1 < paragraph_lengths.len() {
                paragraph += 1;
                paragraph_end = paragraph_end.checked_add(*paragraph_lengths.get(paragraph)?)?;
            }
            let length = remaining.min(paragraph_end.checked_sub(at)?);
            if length == 0 {
                return None;
            }
            let properties = resolved_character_properties(sheet, own, &base_character);
            let font = index(properties.get("0")).and_then(|index| fonts.get(index)).and_then(|font| font.path(&["0", "0", "0"])).and_then(E::as_str)?;
            if font.is_empty()
                || properties.get("1").and_then(E::as_f64).is_none_or(|size| size <= 0.0)
                || !finite_properties(&properties, &[("1", k), ("5", k), ("6", 1.0), ("7", 1.0), ("8", 1.0), ("9", k)])
            {
                return None;
            }
            if let Some(color) = properties.path(&["53", "0", "1"])
                && color
                    .as_array()
                    .is_none_or(|values| values.iter().any(|value| value.as_f64().is_none_or(|value| !value.is_finite() || !(value as f32).is_finite())))
            {
                return None;
            }
            character_runs.push(dict([("StyleSheet", dict([("StyleSheetData", character_properties(&properties))]))]));
            resolved_runs.push(dict([("0", dict([("0", dict([("6", properties)]))])), ("1", E::Int(i64::try_from(length).ok()?))]));
            resolved_lengths.push(length);
            at = at.checked_add(length)?;
            remaining -= length;
        }
    }
    let lengths =
        |lengths: &[usize]| -> Option<E> { Some(E::Array(lengths.iter().map(|length| i64::try_from(*length).ok().map(E::Int)).collect::<Option<Vec<_>>>()?)) };
    let engine = dict([
        (
            "EngineDict",
            dict([
                ("Editor", dict([("Text", E::String(text.into()))])),
                ("StyleRun", dict([("RunArray", E::Array(character_runs)), ("RunLengthArray", lengths(&resolved_lengths)?)])),
                ("ParagraphRun", dict([("RunArray", E::Array(paragraph_runs)), ("RunLengthArray", lengths(&paragraph_lengths)?)])),
            ]),
        ),
        (
            "ResourceDict",
            dict([(
                "FontSet",
                E::Array(fonts.iter().map(|font| dict([("Name", E::String(font.path(&["0", "0", "0"]).and_then(E::as_str).unwrap_or("").into()))])).collect()),
            )]),
        ),
    ]);
    let current_text = layer.text.clone();
    super::apply_engine_data(layer, &engine, Some(&current_text), k);
    if let Some(shape) = box_shape(tree, object_index) {
        layer.shape = shape;
    }
    if tysh.text.get("Ornt").is_none() && super::txt2_frame(tree, object_index).and_then(|frame| frame.path(&["2", "1"])).and_then(E::as_i64) == Some(2) {
        layer.orientation = Orientation::Vertical;
    }
    if tysh.text.get("AntA").is_none()
        && let Some(antialias) = model.path(&["10", "0"]).and_then(E::as_f64).and_then(super::engine_antialias)
    {
        layer.antialias = antialias;
    }
    Some(resolved_runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_doc::text::{CharStyle, TextAlign, TextRun};
    use photocraft_geom::Affine;
    use photocraft_psd::descriptor::Value as D;

    fn font(name: &str) -> E {
        dict([("0", dict([("0", dict([("0", E::String(name.into()))]))]))])
    }

    fn sheet(name: &str, size: f64, font: i64) -> E {
        dict([("0", dict([("0", E::String(name.into())), ("6", dict([("0", E::Int(font)), ("1", E::Real(size))]))]))])
    }

    fn run(length: i64, properties: E, paragraph: bool, name: &str, default: i64) -> E {
        let property = if paragraph { "5" } else { "6" };
        let mut sheet = dict([("0", E::String(name.into())), (property, properties)]);
        if paragraph {
            sheet.set("6", E::Int(default));
        }
        dict([("0", dict([("0", sheet)])), ("1", E::Int(length))])
    }

    fn tree(text: &str, styles: Vec<E>, paragraphs: Vec<E>) -> E {
        dict([
            (
                "0",
                dict([
                    ("1", dict([("0", E::Array(vec![font("Inter-Regular"), font("CourierNewPSMT")]))])),
                    ("5", dict([("0", E::Array(vec![sheet("Normal", 24.0, 0), sheet("Accent", 40.0, 1), sheet("Large", 60.0, 0)]))])),
                    (
                        "6",
                        dict([(
                            "0",
                            E::Array(vec![
                                paragraph_sheet("Normal RGB", dict([("0", E::Int(2)), ("2", E::Real(8.0))]), None),
                                paragraph_sheet("Body", E::dict(), Some(0)),
                                paragraph_sheet("Other", dict([("0", E::Int(1))]), Some(0)),
                            ]),
                        )]),
                    ),
                ]),
            ),
            (
                "1",
                dict([
                    ("2", dict([("0", E::Int(0)), ("1", E::Real(24.0)), ("8", E::Real(85.0)), ("11", E::Int(1)), ("20", E::Bool(false))])),
                    ("3", dict([("0", E::Int(1)), ("4", E::Real(6.0))])),
                    (
                        "1",
                        E::Array(vec![dict([(
                            "0",
                            dict([("0", E::String(format!("{text}\r"))), ("5", dict([("0", E::Array(paragraphs))])), ("6", dict([("0", E::Array(styles))]))]),
                        )])]),
                    ),
                ]),
            ),
        ])
    }

    fn local(text: &str) -> (TextLayer, super::super::TySh) {
        let mut layer = TextLayer {
            text: text.into(),
            transform: Affine::translate(17.0, 29.0),
            runs: vec![TextRun { len: text.len(), style: CharStyle { font_family: "Inter".into(), size_pt: 11.0, ..Default::default() } }],
            ..Default::default()
        };
        layer.sync_summary();
        let mut tysh = super::super::parse_tysh(&super::super::build_tysh(&layer, 144.0, None)).unwrap();
        tysh.text.items.retain(|(key, _)| !key.is("EngineData"));
        (layer, tysh)
    }

    fn paragraph_sheet(name: &str, properties: E, parent: Option<i64>) -> E {
        let mut sheet = dict([("0", E::String(name.into())), ("5", properties)]);
        if let Some(parent) = parent {
            sheet.set("6", E::Int(parent));
        }
        dict([("0", sheet)])
    }

    fn set_paragraphs(tree: &mut E, paragraphs: Vec<E>) {
        tree.get_mut("0").unwrap().set("6", dict([("0", E::Array(paragraphs))]));
    }

    fn import(layer: &mut TextLayer, tysh: &super::super::TySh, tree: &E) {
        super::super::apply_txt2_with_dpi(layer, &super::super::write_tysh(tysh), tree, 144.0);
    }

    #[test]
    fn sparse_global_runs_resolve_named_styles_and_document_defaults() {
        let (mut layer, tysh) = local("AB");
        let tree = tree("AB", vec![run(3, E::dict(), false, "Accent", 0)], vec![run(3, E::dict(), true, "Body", 1)]);
        import(&mut layer, &tysh, &tree);
        let style = &layer.char_runs()[0].style;
        assert_eq!(style.postscript_name.as_deref(), Some("CourierNewPSMT"));
        assert_eq!(style.size_pt, 20.0);
        assert_eq!(style.tracking, 85.0);
        assert!(style.features.iter().any(|feature| feature.tag == "calt" && feature.value == 0));
        let paragraph = &layer.paragraph_runs()[0].style;
        assert_eq!(paragraph.align, TextAlign::Center);
        assert_eq!(paragraph.start_indent_pt, 4.0);
        assert_eq!(paragraph.space_before_pt, 3.0);
        assert_eq!(layer.transform, tysh.transform);
    }

    #[test]
    fn paragraph_parent_references_never_select_a_character_style() {
        let (mut layer, tysh) = local("AB");
        let mut tree = tree("AB", vec![run(3, E::dict(), false, "", 0)], vec![run(3, E::dict(), true, "", 1)]);
        set_paragraphs(
            &mut tree,
            vec![
                paragraph_sheet("Normal RGB", dict([("0", E::Int(2)), ("2", E::Real(8.0))]), None),
                paragraph_sheet("Body", dict([("4", E::Real(12.0))]), Some(0)),
            ],
        );
        tree.get_mut("0").unwrap().get_mut("6").unwrap().set("1", E::Array(vec![dict([("0", E::Int(1))]), dict([("0", E::Int(0))])]));
        import(&mut layer, &tysh, &tree);
        let style = &layer.char_runs()[0].style;
        assert_eq!(style.postscript_name.as_deref(), Some("Inter-Regular"));
        assert_eq!(style.size_pt, 12.0);
        let paragraph = &layer.paragraph_runs()[0].style;
        assert_eq!(paragraph.align, TextAlign::Center);
        assert_eq!(paragraph.start_indent_pt, 4.0);
        assert_eq!(paragraph.space_before_pt, 6.0);
    }

    #[test]
    fn invalid_paragraph_ancestry_cannot_partially_replace_the_model() {
        for parents in [vec![Some(0)], vec![Some(1), Some(0)], vec![Some(99)], vec![Some(-1)]] {
            let (mut layer, tysh) = local("AB");
            let before = layer.clone();
            let mut tree = tree("AB", vec![run(3, E::dict(), false, "Accent", 0)], vec![run(3, E::dict(), true, "", 0)]);
            set_paragraphs(&mut tree, parents.into_iter().map(|parent| paragraph_sheet("Body", E::dict(), parent)).collect());
            let model = tree.path(&["1", "1"]).and_then(E::as_array).unwrap()[0].get("0").unwrap();
            assert!(apply(&mut layer, &tysh, &tree, model, 0, 144.0).is_none());
            assert_eq!(layer, before);
        }
    }

    #[test]
    fn old_factory_parent_is_repaired_before_recovering_global_only_type() {
        for malformed in [false, true] {
            let (mut layer, mut tysh) = local("AB");
            if malformed {
                tysh.text.items.push((photocraft_psd::descriptor::Id::new("EngineData"), D::RawData(b"<< /broken [".to_vec())));
            }
            let mut tree = tree("AB", vec![run(3, E::dict(), false, "Accent", 0)], vec![run(3, E::dict(), true, "", 0)]);
            set_paragraphs(&mut tree, vec![paragraph_sheet("Normal RGB", dict([("0", E::Int(2)), ("2", E::Real(8.0))]), Some(0))]);
            let before = tree.clone();
            import(&mut layer, &tysh, &tree);
            assert_eq!(layer.char_runs()[0].style.postscript_name.as_deref(), Some("CourierNewPSMT"));
            assert_eq!(layer.char_runs()[0].style.size_pt, 20.0);
            assert_eq!(layer.paragraph_runs()[0].style.align, TextAlign::Center);
            assert_eq!(layer.paragraph_runs()[0].style.start_indent_pt, 4.0);
            assert_eq!(tree, before, "normalizing the temporary ancestry must not rewrite the original global model");
        }
    }

    #[test]
    fn paragraph_chain_resolves_leaf_overrides_before_ancestors_and_engine_defaults() {
        for named in [false, true] {
            let (mut layer, tysh) = local("AB");
            let mut paragraph = run(3, dict([("2", E::Real(20.0))]), true, "Leaf", 2);
            if named {
                let E::Dict(sheet) = paragraph.get_mut("0").unwrap().get_mut("0").unwrap() else { panic!() };
                sheet.retain(|(key, _)| key != "6");
            }
            let mut tree = tree("AB", vec![run(3, E::dict(), false, "", 0)], vec![paragraph]);
            set_paragraphs(
                &mut tree,
                vec![
                    paragraph_sheet("Normal RGB", dict([("0", E::Int(2)), ("2", E::Real(8.0))]), None),
                    paragraph_sheet("Body", dict([("2", E::Real(12.0)), ("4", E::Real(14.0))]), Some(0)),
                    paragraph_sheet("Leaf", dict([("5", E::Real(10.0))]), Some(1)),
                ],
            );
            import(&mut layer, &tysh, &tree);
            let paragraph = &layer.paragraph_runs()[0].style;
            assert_eq!(paragraph.align, TextAlign::Center);
            assert_eq!(paragraph.start_indent_pt, 10.0);
            assert_eq!(paragraph.space_before_pt, 7.0);
            assert_eq!(paragraph.space_after_pt, 5.0);
            assert_eq!(layer.char_runs()[0].style.size_pt, 12.0);
        }
    }

    #[test]
    fn invalid_inline_parent_and_unused_resource_cycles_are_rejected_atomically() {
        for parent in [E::Int(-1), E::Int(99), E::Real(0.0), E::Real(0.5), E::Real(f64::NAN), E::Bool(false)] {
            let (mut layer, tysh) = local("AB");
            let before = layer.clone();
            let mut paragraph = run(3, E::dict(), true, "", 0);
            paragraph.get_mut("0").unwrap().get_mut("0").unwrap().set("6", parent);
            let tree = tree("AB", vec![run(3, E::dict(), false, "Accent", 0)], vec![paragraph]);
            let model = tree.path(&["1", "1"]).and_then(E::as_array).unwrap()[0].get("0").unwrap();
            assert!(apply(&mut layer, &tysh, &tree, model, 0, 144.0).is_none());
            assert_eq!(layer, before);
        }
        let (mut layer, tysh) = local("AB");
        let before = layer.clone();
        let mut tree = tree("AB", vec![run(3, E::dict(), false, "Accent", 0)], vec![run(3, E::dict(), true, "", 0)]);
        set_paragraphs(&mut tree, vec![paragraph_sheet("Normal RGB", E::dict(), None), paragraph_sheet("Unused", E::dict(), Some(1))]);
        let model = tree.path(&["1", "1"]).and_then(E::as_array).unwrap()[0].get("0").unwrap();
        assert!(apply(&mut layer, &tysh, &tree, model, 0, 144.0).is_none());
        assert_eq!(layer, before);
    }

    #[test]
    fn valid_local_engine_is_authoritative_even_when_global_paragraphs_are_cyclic() {
        let (mut layer, _) = local("AB");
        let tysh = super::super::parse_tysh(&super::super::build_tysh(&layer, 144.0, None)).unwrap();
        let before = layer.clone();
        let mut tree = tree("AB", vec![run(3, E::dict(), false, "Accent", 0)], vec![run(3, E::dict(), true, "", 0)]);
        set_paragraphs(&mut tree, vec![paragraph_sheet("Normal RGB", E::dict(), Some(0))]);
        let model = tree.path(&["1", "1"]).and_then(E::as_array).unwrap()[0].get("0").unwrap();
        assert!(apply(&mut layer, &tysh, &tree, model, 0, 144.0).is_none());
        assert_eq!(layer, before);
    }

    #[test]
    fn normal_resource_sheets_supply_defaults_when_engine_copies_are_absent_or_partial() {
        for partial in [false, true] {
            let (mut layer, tysh) = local("AB");
            let mut paragraph = run(3, E::dict(), true, "", 0);
            let E::Dict(sheet) = paragraph.get_mut("0").unwrap().get_mut("0").unwrap() else { panic!() };
            sheet.retain(|(key, _)| key != "6");
            let mut tree = tree("AB", vec![run(3, E::dict(), false, "", 0)], vec![paragraph]);
            let E::Dict(engine) = tree.get_mut("1").unwrap() else { panic!() };
            engine.retain(|(key, _)| key != "2" && key != "3");
            if partial {
                engine.push(("2".into(), dict([("8", E::Real(17.0))])));
                engine.push(("3".into(), dict([("0", E::Int(1))])));
            }
            import(&mut layer, &tysh, &tree);
            let style = &layer.char_runs()[0].style;
            assert_eq!(style.postscript_name.as_deref(), Some("Inter-Regular"));
            assert_eq!(style.size_pt, 12.0);
            assert_eq!(style.tracking, if partial { 17.0 } else { 0.0 });
            let paragraph = &layer.paragraph_runs()[0].style;
            assert_eq!(paragraph.align, if partial { TextAlign::Right } else { TextAlign::Center });
            assert_eq!(paragraph.start_indent_pt, 4.0);
        }
    }

    #[test]
    fn paragraph_parent_indices_do_not_select_character_defaults_at_boundaries() {
        let (mut layer, tysh) = local("A\u{1f600}\nB");
        let tree = tree("A\u{1f600}\rB", vec![run(6, E::dict(), false, "", 0)], vec![run(4, E::dict(), true, "", 1), run(2, E::dict(), true, "", 2)]);
        import(&mut layer, &tysh, &tree);
        let styles = layer.char_runs();
        assert_eq!(styles.iter().map(|style| style.len).sum::<usize>(), layer.text.len());
        assert!(styles.iter().all(|style| style.style.size_pt == 12.0 && style.style.postscript_name.as_deref() == Some("Inter-Regular")));
        let paragraphs = layer.paragraph_runs();
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(paragraphs[0].style.align, TextAlign::Center);
        assert_eq!(paragraphs[1].style.align, TextAlign::Right);
    }

    #[test]
    fn invalid_global_coverage_or_font_never_partially_replaces_the_model() {
        for (length, font) in [(2, 0), (4, 0), (3, 999), (3, -1)] {
            let (mut layer, tysh) = local("AB");
            let before = layer.clone();
            let tree = tree("AB", vec![run(length, dict([("0", E::Int(font)), ("1", E::Real(50.0))]), false, "", 0)], vec![run(3, E::dict(), true, "Body", 1)]);
            let model = tree.path(&["1", "1"]).and_then(E::as_array).unwrap()[0].get("0").unwrap();
            assert!(apply(&mut layer, &tysh, &tree, model, 0, 144.0).is_none());
            assert_eq!(layer, before);
        }
    }

    #[test]
    fn nonfinite_global_properties_do_not_replace_the_model() {
        for key in ["1", "5", "6", "7", "8", "9"] {
            let (mut layer, tysh) = local("AB");
            let before = layer.clone();
            let tree = tree("AB", vec![run(3, dict([(key, E::Real(f64::INFINITY))]), false, "Accent", 0)], vec![run(3, E::dict(), true, "Body", 1)]);
            let model = tree.path(&["1", "1"]).and_then(E::as_array).unwrap()[0].get("0").unwrap();
            assert!(apply(&mut layer, &tysh, &tree, model, 0, 144.0).is_none());
            assert_eq!(layer, before);
        }
    }

    #[test]
    fn structurally_incomplete_local_engine_can_recover_from_a_complete_global_model() {
        let (mut layer, mut tysh) = local("AB");
        let engine = dict([("EngineDict", dict([("Editor", dict([("Text", E::String("AB\r".into()))]))]))]);
        tysh.text.items.push((photocraft_psd::descriptor::Id::new("EngineData"), D::RawData(crate::engine_data::write(&engine))));
        let tree = tree("AB", vec![run(3, E::dict(), false, "Accent", 0)], vec![run(3, E::dict(), true, "Body", 1)]);
        import(&mut layer, &tysh, &tree);
        assert_eq!(layer.char_runs()[0].style.size_pt, 20.0);
        assert_eq!(layer.char_runs()[0].style.postscript_name.as_deref(), Some("CourierNewPSMT"));
    }

    #[test]
    fn a_surrogate_split_and_noninteger_lengths_are_rejected() {
        for lengths in [[E::Int(1), E::Int(2)], [E::Real(1.5), E::Real(1.5)], [E::Real(f64::NAN), E::Int(3)]] {
            assert!(run_lengths("\u{1f600}\r", &lengths, |value| Some(value)).is_none());
        }
    }

    #[test]
    fn deliberate_blank_local_engine_is_not_replaced_by_descriptor_characters() {
        let (mut layer, mut tysh) = local("AB");
        tysh.text.items.push((
            photocraft_psd::descriptor::Id::new("EngineData"),
            D::RawData(crate::engine_data::write(&dict([("EngineDict", dict([("Editor", dict([("Text", E::String("\r".into()))]))]))]))),
        ));
        let tree = tree("AB", vec![run(3, E::dict(), false, "Accent", 0)], vec![run(3, E::dict(), true, "Body", 1)]);
        import(&mut layer, &tysh, &tree);
        assert_eq!(layer.char_runs()[0].style.size_pt, 11.0);
    }
}
