//! The public Txt2 importer must not apply exclusive flags after rejecting its full model.

use photocraft_doc::TextLayer;
use photocraft_doc::text::{CharStyle, FontFeature, Kerning, TextRun};
use photocraft_psd::descriptor::{Id, Value as D};
use photocraft_text::engine_data::{self, Value as E};
use photocraft_text::psd::{apply_txt2_with_dpi, build_tysh, parse_tysh, write_tysh};

fn dict(items: impl IntoIterator<Item = (&'static str, E)>) -> E {
    E::Dict(items.into_iter().map(|(key, value)| (key.into(), value)).collect())
}

fn layer() -> TextLayer {
    let style = CharStyle {
        font_family: "Inter".into(),
        postscript_name: Some("Inter-Regular".into()),
        size_pt: 18.0,
        kerning: Kerning::Metrics,
        features: vec![FontFeature { tag: "calt".into(), value: 1 }],
        ..Default::default()
    };
    let mut layer = TextLayer { text: "AB".into(), runs: vec![TextRun { len: 2, style }], ..Default::default() };
    layer.sync_summary();
    layer
}

#[derive(Clone, Copy)]
enum Local {
    Valid,
    Missing,
    Malformed,
    Incomplete,
}

fn tysh(layer: &TextLayer, local: Local) -> Vec<u8> {
    let mut tysh = parse_tysh(&build_tysh(layer, 144.0, None)).unwrap();
    tysh.text.items.retain(|(key, _)| !key.is("TextIndex"));
    tysh.text.items.push((Id::new("TextIndex"), D::Integer(0)));
    if !matches!(local, Local::Valid) {
        tysh.text.items.retain(|(key, _)| !key.is("EngineData"));
    }
    match local {
        Local::Malformed => tysh.text.items.push((Id::new("EngineData"), D::RawData(b"<< /broken [".to_vec()))),
        Local::Incomplete => tysh.text.items.push((
            Id::new("EngineData"),
            D::RawData(engine_data::write(&dict([("EngineDict", dict([("Editor", dict([("Text", E::String("AB\r".into()))]))]))]))),
        )),
        Local::Valid | Local::Missing => {}
    }
    write_tysh(&tysh)
}

#[derive(Clone, Copy)]
enum Invalid {
    Cycle,
    Parent,
    Coverage,
    Font,
}

fn global(invalid: Option<Invalid>) -> E {
    let character =
        dict([("0", E::Int(if matches!(invalid, Some(Invalid::Font)) { 99 } else { 0 })), ("1", E::Real(36.0)), ("11", E::Int(2)), ("20", E::Bool(false))]);
    let paragraph = dict([("0", E::Int(2))]);
    let mut named = dict([("0", E::String("Custom root".into())), ("5", paragraph.clone())]);
    if matches!(invalid, Some(Invalid::Cycle)) {
        named.set("6", E::Int(0));
    }
    let paragraph_run = dict([
        (
            "0",
            dict([(
                "0",
                dict([("0", E::String(String::new())), ("5", paragraph), ("6", E::Int(if matches!(invalid, Some(Invalid::Parent)) { 99 } else { 0 }))]),
            )]),
        ),
        ("1", E::Int(3)),
    ]);
    let character_run = dict([
        ("0", dict([("0", dict([("0", E::String(String::new())), ("6", character.clone())]))])),
        ("1", E::Int(if matches!(invalid, Some(Invalid::Coverage)) { 2 } else { 3 })),
    ]);
    let font = dict([("0", dict([("0", dict([("0", E::String("CourierNewPSMT".into()))]))]))]);
    let normal_character = dict([("0", dict([("0", E::String("Normal RGB".into())), ("6", character.clone())]))]);
    dict([
        (
            "0",
            dict([
                ("1", dict([("0", E::Array(vec![font]))])),
                ("5", dict([("0", E::Array(vec![normal_character]))])),
                ("6", dict([("0", E::Array(vec![dict([("0", named)])]))])),
            ]),
        ),
        (
            "1",
            dict([
                ("2", character),
                ("3", dict([("0", E::Int(0))])),
                (
                    "1",
                    E::Array(vec![dict([(
                        "0",
                        dict([
                            ("0", E::String("AB\r".into())),
                            ("5", dict([("0", E::Array(vec![paragraph_run]))])),
                            ("6", dict([("0", E::Array(vec![character_run]))])),
                        ]),
                    )])]),
                ),
            ]),
        ),
    ])
}

fn character_sheet(tree: &mut E) -> &mut E {
    let E::Array(objects) = tree.get_mut("1").unwrap().get_mut("1").unwrap() else { panic!() };
    let E::Array(runs) = objects[0].get_mut("0").unwrap().get_mut("6").unwrap().get_mut("0").unwrap() else { panic!() };
    runs[0].get_mut("0").unwrap().get_mut("0").unwrap()
}

fn normal_character(tree: &mut E) -> &mut E {
    let E::Array(styles) = tree.get_mut("0").unwrap().get_mut("5").unwrap().get_mut("0").unwrap() else { panic!() };
    styles[0].get_mut("0").unwrap().get_mut("6").unwrap()
}

fn omit_inline_flags(tree: &mut E) {
    let E::Dict(properties) = character_sheet(tree).get_mut("6").unwrap() else { panic!() };
    properties.retain(|(key, _)| !matches!(key.as_str(), "11" | "20" | "23" | "25" | "85"));
}

#[test]
fn invalid_global_only_fallback_is_atomic_through_public_api() {
    for local in [Local::Missing, Local::Malformed, Local::Incomplete] {
        for invalid in [Invalid::Cycle, Invalid::Parent, Invalid::Coverage, Invalid::Font] {
            for explicit in [false, true] {
                let mut layer = layer();
                if !explicit {
                    layer.runs[0].style.features.clear();
                }
                let before = layer.clone();
                apply_txt2_with_dpi(&mut layer, &tysh(&before, local), &global(Some(invalid)), 144.0);
                assert_eq!(layer, before, "rejected full global data must not leak kerning or feature overrides");
            }
        }
    }
}

#[test]
fn valid_local_type_keeps_its_model_and_accepts_global_only_flags() {
    for explicit in [false, true] {
        let mut layer = layer();
        if !explicit {
            layer.runs[0].style.features.clear();
        }
        let local = tysh(&layer, Local::Valid);
        apply_txt2_with_dpi(&mut layer, &local, &global(None), 144.0);
        let style = &layer.char_runs()[0].style;
        assert_eq!(style.postscript_name.as_deref(), Some("Inter-Regular"));
        assert_eq!(style.size_pt, 18.0);
        assert_eq!(style.kerning, Kerning::Optical);
        // The existing overlay preserves explicit local features and supplies absent ones.
        assert!(style.features.iter().any(|feature| feature.tag == "calt" && feature.value == u16::from(explicit)));
    }
}

#[test]
fn accepted_global_only_model_still_recovers_its_model_and_flags() {
    for local in [Local::Missing, Local::Malformed, Local::Incomplete] {
        let mut layer = layer();
        let source = tysh(&layer, local);
        apply_txt2_with_dpi(&mut layer, &source, &global(None), 144.0);
        let style = &layer.char_runs()[0].style;
        assert_eq!(style.postscript_name.as_deref(), Some("CourierNewPSMT"));
        assert_eq!(style.size_pt, 18.0);
        assert_eq!(style.kerning, Kerning::Optical);
        assert!(style.features.iter().any(|feature| feature.tag == "calt" && feature.value == 0));
    }
}

#[test]
fn valid_local_type_inherits_global_flags_from_document_and_normal_defaults() {
    for normal_only in [false, true] {
        let mut layer = layer();
        layer.runs[0].style.features.clear();
        let local = tysh(&layer, Local::Valid);
        let mut global = global(None);
        omit_inline_flags(&mut global);
        for key in ["25", "85"] {
            let value = if key == "25" { E::Bool(true) } else { E::Int(4) };
            global.get_mut("1").unwrap().get_mut("2").unwrap().set(key, value.clone());
            normal_character(&mut global).set(key, value);
        }
        if normal_only {
            let E::Dict(defaults) = global.get_mut("1").unwrap().get_mut("2").unwrap() else { panic!() };
            defaults.retain(|(key, _)| !matches!(key.as_str(), "11" | "20" | "25" | "85"));
        }
        apply_txt2_with_dpi(&mut layer, &local, &global, 144.0);
        let style = &layer.char_runs()[0].style;
        assert_eq!(style.postscript_name.as_deref(), Some("Inter-Regular"));
        assert_eq!(style.size_pt, 18.0);
        assert_eq!(style.kerning, Kerning::Optical);
        for (tag, value) in [("calt", 0), ("swsh", 1), ("ss03", 1)] {
            assert!(style.features.iter().any(|feature| feature.tag == tag && feature.value == value), "missing inherited {tag}={value}");
        }
    }
}

#[test]
fn valid_local_global_flags_respect_named_inline_and_explicit_local_precedence() {
    for inline in [false, true] {
        for explicit_local in [false, true] {
            let mut layer = layer();
            if !explicit_local {
                layer.runs[0].style.features.clear();
            }
            let local = tysh(&layer, Local::Valid);
            let mut global = global(None);
            omit_inline_flags(&mut global);
            let E::Array(styles) = global.get_mut("0").unwrap().get_mut("5").unwrap().get_mut("0").unwrap() else { panic!() };
            styles.push(dict([(
                "0",
                dict([
                    ("0", E::String("Accent".into())),
                    ("6", dict([("0", E::Int(0)), ("1", E::Real(200.0)), ("11", E::Int(1)), ("20", E::Bool(true)), ("23", E::Bool(true)), ("85", E::Int(2))])),
                ]),
            )]));
            character_sheet(&mut global).set("0", E::String("Accent".into()));
            if inline {
                let properties = character_sheet(&mut global).get_mut("6").unwrap();
                properties.set("11", E::Int(0));
                properties.set("20", E::Bool(false));
            }
            apply_txt2_with_dpi(&mut layer, &local, &global, 144.0);
            let style = &layer.char_runs()[0].style;
            assert_eq!(style.postscript_name.as_deref(), Some("Inter-Regular"));
            assert_eq!(style.size_pt, 18.0);
            assert_eq!(style.kerning, if inline { Kerning::Off } else { Kerning::Metrics });
            let calt = u16::from(explicit_local || !inline);
            assert!(style.features.iter().any(|feature| feature.tag == "calt" && feature.value == calt));
            assert!(style.features.iter().any(|feature| feature.tag == "frac" && feature.value == 1));
            assert!(style.features.iter().any(|feature| feature.tag == "ss02" && feature.value == 1));
        }
    }
}
