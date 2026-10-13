//! What one face offers for fine typography (the Type Details window): its variation axes with
//! the font's own names, its named instances, and its OpenType layout features with the names
//! the font gives them (`ss01` "Open digits", `cv11` "Single-story a") and the characters each
//! feature changes, so a preview can show exactly those.

use std::collections::{BTreeMap, BTreeSet};

use skrifa::raw::tables::gsub::{SingleSubst, SubstitutionSubtables};
use skrifa::raw::tables::layout::{CoverageTable, FeatureParams};
use skrifa::raw::{ReadError, TableProvider, types::GlyphId16};
use skrifa::{FontRef, MetadataProvider, string::StringId};

/// Most characters kept as a feature's sample.
const SAMPLE_CHARS: usize = 24;
/// Most features read from one table, most lookups followed per feature, and most glyphs read
/// from one coverage table: fonts are input, and a malformed one can claim anything.
const MAX_FEATURES: usize = 512;
const MAX_LOOKUPS: usize = 64;
const MAX_COVERAGE: usize = 4096;

/// A variation axis of the face, from its `fvar` table.
#[derive(Clone, Debug, PartialEq)]
pub struct AxisDetail {
    /// Four-character tag (`wght`, `GRAD`).
    pub tag: String,
    /// The font's own (English, else first) name for the axis ("Parametric Thick Stroke"), if it
    /// has one.
    pub name: Option<String>,
    pub min: f32,
    pub default: f32,
    pub max: f32,
    /// The font asks for the axis to stay out of user interfaces (`fvar` flag 0x0001).
    pub hidden: bool,
}

/// A named instance ("Bold Condensed"): a full set of axis coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceDetail {
    pub name: String,
    /// User-space coordinates, one per axis in [`FaceDetails::axes`] order: (tag, value).
    pub coords: Vec<(String, f32)>,
}

/// An OpenType layout feature the face has (in `GSUB` or `GPOS`).
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureDetail {
    pub tag: String,
    /// The font's UI name for a stylistic set or character variant (`ss01`–`ss20`,
    /// `cv01`–`cv99` feature parameters), if it gives one.
    pub name: Option<String>,
    /// Characters the feature substitutes, in a readable order (letters and figures first); empty
    /// for positioning features and for substitutions only reachable through context.
    pub sample: String,
}

/// Everything [`crate::FontDb::face_details`] reads from one face.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FaceDetails {
    pub axes: Vec<AxisDetail>,
    pub instances: Vec<InstanceDetail>,
    /// Sorted by tag, each tag once.
    pub features: Vec<FeatureDetail>,
}

impl FaceDetails {
    /// Whether the face has the layout feature `tag`.
    pub fn has_feature(&self, tag: &str) -> bool {
        self.features.binary_search_by(|f| f.tag.as_str().cmp(tag)).is_ok()
    }

    /// The face's feature `tag`, if it has it.
    pub fn feature(&self, tag: &str) -> Option<&FeatureDetail> {
        self.features.binary_search_by(|f| f.tag.as_str().cmp(tag)).ok().and_then(|i| self.features.get(i))
    }

    /// The face's axis `tag`, if it has it.
    pub fn axis(&self, tag: &str) -> Option<&AxisDetail> {
        self.axes.iter().find(|a| a.tag == tag)
    }
}

/// Reads [`FaceDetails`] from a parsed face. Unreadable parts are left out, never an error: a
/// font with a broken `GSUB` still has its axes.
pub(crate) fn read(font: &FontRef) -> FaceDetails {
    let name = |id: StringId| font.localized_strings(id).english_or_first().map(|s| s.to_string()).filter(|s| !s.trim().is_empty());
    let axes: Vec<AxisDetail> = font
        .axes()
        .iter()
        .filter(|a| a.min_value().is_finite() && a.default_value().is_finite() && a.max_value().is_finite() && a.min_value() <= a.max_value())
        .map(|a| AxisDetail {
            tag: a.tag().to_string(),
            name: name(a.name_id()),
            min: a.min_value(),
            default: a.default_value().clamp(a.min_value(), a.max_value()),
            max: a.max_value(),
            hidden: a.is_hidden(),
        })
        .collect();
    let all_axes: Vec<String> = font.axes().iter().map(|a| a.tag().to_string()).collect();
    let instances = font
        .named_instances()
        .iter()
        .filter_map(|inst| {
            let coords: Vec<(String, f32)> = all_axes.iter().cloned().zip(inst.user_coords()).filter(|(_, v)| v.is_finite()).collect();
            Some(InstanceDetail { name: name(inst.subfamily_name_id())?, coords })
        })
        .collect();
    let mut features: BTreeMap<String, FeatureDetail> = BTreeMap::new();
    if let Ok(gsub) = font.gsub() {
        let _ = gsub_features(font, &gsub, &name, &mut features);
    }
    if let Ok(gpos) = font.gpos()
        && let Ok(list) = gpos.feature_list()
    {
        for record in list.feature_records().iter().take(MAX_FEATURES) {
            let tag = record.feature_tag().to_string();
            features.entry(tag.clone()).or_insert(FeatureDetail { tag, name: None, sample: String::new() });
        }
    }
    FaceDetails { axes, instances, features: features.into_values().collect() }
}

/// Adds the `GSUB` features, their UI names and their substituted characters to `out`.
fn gsub_features(
    font: &FontRef,
    gsub: &skrifa::raw::tables::gsub::Gsub,
    name: &dyn Fn(StringId) -> Option<String>,
    out: &mut BTreeMap<String, FeatureDetail>,
) -> Result<(), ReadError> {
    let list = gsub.feature_list()?;
    let lookups = gsub.lookup_list().ok();
    let reverse = reverse_cmap(font);
    let data = list.offset_data();
    // Glyphs per tag; one tag can have several records (one per script/language).
    let mut glyphs: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
    for record in list.feature_records().iter().take(MAX_FEATURES) {
        let tag = record.feature_tag().to_string();
        let entry = out.entry(tag.clone()).or_insert(FeatureDetail { tag: tag.clone(), name: None, sample: String::new() });
        let Ok(feature) = record.feature(data) else { continue };
        if entry.name.is_none() {
            entry.name = match feature.feature_params() {
                Some(Ok(FeatureParams::StylisticSet(p))) => name(p.ui_name_id()),
                Some(Ok(FeatureParams::CharacterVariant(p))) => name(p.feat_ui_label_name_id()),
                _ => None,
            };
        }
        let Some(lookups) = &lookups else { continue };
        let set = glyphs.entry(tag).or_default();
        for index in feature.lookup_list_indices().iter().take(MAX_LOOKUPS) {
            let Ok(lookup) = lookups.lookups().get(usize::from(index.get())) else { continue };
            let Ok(subtables) = lookup.subtables() else { continue };
            collect_coverage(&subtables, set);
        }
    }
    for (tag, set) in glyphs {
        if let Some(entry) = out.get_mut(&tag) {
            entry.sample = sample(set.iter().filter_map(|g| reverse.get(g).copied()));
        }
    }
    Ok(())
}

/// The first glyphs each directly substituting subtable acts on. Contextual subtables are skipped:
/// what they change depends on the surrounding text, so their coverage would mislead.
fn collect_coverage(subtables: &SubstitutionSubtables, set: &mut BTreeSet<u16>) {
    let mut add = |c: Result<CoverageTable, ReadError>| {
        if let Ok(c) = c {
            set.extend(c.iter().take(MAX_COVERAGE).map(|g: GlyphId16| g.to_u16()));
        }
    };
    match subtables {
        SubstitutionSubtables::Single(s) => s.iter().flatten().for_each(|t| match t {
            SingleSubst::Format1(t) => add(t.coverage()),
            SingleSubst::Format2(t) => add(t.coverage()),
        }),
        SubstitutionSubtables::Multiple(s) => s.iter().flatten().for_each(|t| add(t.coverage())),
        SubstitutionSubtables::Alternate(s) => s.iter().flatten().for_each(|t| add(t.coverage())),
        SubstitutionSubtables::Ligature(s) => s.iter().flatten().for_each(|t| add(t.coverage())),
        _ => {}
    }
}

/// Nominal glyph → its lowest character, for the Basic Multilingual Plane (enough for samples).
fn reverse_cmap(font: &FontRef) -> BTreeMap<u16, char> {
    let mut out = BTreeMap::new();
    for (cp, gid) in font.charmap().mappings().take(1 << 17) {
        let (Some(c), Ok(g)) = (char::from_u32(cp), u16::try_from(gid.to_u32())) else { continue };
        if !c.is_control() && !c.is_whitespace() {
            out.entry(g).or_insert(c);
        }
    }
    out
}

/// A readable sample from substituted characters: ASCII letters and figures first, then other
/// letters and figures, then the rest, at most [`SAMPLE_CHARS`].
fn sample(chars: impl Iterator<Item = char>) -> String {
    let mut chars: Vec<char> = chars.collect::<BTreeSet<char>>().into_iter().collect();
    chars.sort_by_key(|c| {
        (
            if c.is_ascii_alphanumeric() {
                0
            } else if c.is_alphanumeric() {
                1
            } else {
                2
            },
            *c,
        )
    });
    chars.into_iter().take(SAMPLE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_puts_ascii_letters_and_figures_first() {
        assert_eq!(sample("é!a1Z".chars()), "1Zaé!");
        assert_eq!(sample("x".repeat(100).chars()), "x");
        assert_eq!(sample(('a'..='z').chain('A'..='Z')).chars().count(), SAMPLE_CHARS);
    }

    #[test]
    fn garbage_is_not_a_font() {
        assert!(FontRef::new(b"not a font").is_err());
    }

    fn style(family: &str) -> photocraft_doc::text::CharStyle {
        photocraft_doc::text::CharStyle { font_family: family.into(), ..Default::default() }
    }

    #[test]
    fn bundled_inter_lists_its_features_with_names_and_samples() {
        let mut e = crate::TextEngine::new();
        let d = e.fonts.face_details(&style("Inter"));
        for tag in ["kern", "dlig", "calt", "tnum", "frac", "cv11"] {
            assert!(d.has_feature(tag), "{tag} in {:?}", d.features.iter().map(|f| &f.tag).collect::<Vec<_>>());
        }
        assert!(d.features.windows(2).all(|w| w[0].tag < w[1].tag), "sorted, each tag once");
        let tnum = d.feature("tnum").unwrap();
        assert!(tnum.sample.contains('1'), "tabular figures change the figures: {:?}", tnum.sample);
        assert!(d.feature("kern").unwrap().sample.is_empty(), "positioning has no substitution sample");
        assert!(!d.has_feature("zzzz"));
        // The font's own names for stylistic sets and character variants.
        assert_eq!(d.feature("cv11").and_then(|f| f.name.as_deref()), Some("Single-story a"));
        assert_eq!(d.feature("ss01").and_then(|f| f.name.as_deref()), Some("Open digits"));
        assert!(d.feature("cv11").unwrap().sample.contains('a'));
        // An unknown family reads the default family; the result is cached and shared.
        let a = e.fonts.face_details(&style("No Such Family"));
        assert!(a.has_feature("kern"));
        assert!(std::sync::Arc::ptr_eq(&a, &e.fonts.face_details(&style("No Such Family"))));
    }

    /// Opt-in check against real variable fonts, which aren't in the repository: point
    /// `PHOTOCRAFT_TEST_ROBOTO_FLEX` at a Roboto Flex variable TTF (Google Fonts, OFL) and run
    /// with `--ignored`. The bundled-Inter test above covers the same code on every machine.
    #[test]
    #[ignore = "needs PHOTOCRAFT_TEST_ROBOTO_FLEX"]
    fn roboto_flex_names_its_axes_and_instances() {
        let path = std::env::var("PHOTOCRAFT_TEST_ROBOTO_FLEX").expect("set PHOTOCRAFT_TEST_ROBOTO_FLEX to a Roboto Flex variable TTF");
        let mut e = crate::TextEngine::new();
        let fam = e.fonts.register_font_data(std::fs::read(path).unwrap()).into_iter().next().unwrap();
        let d = e.fonts.face_details(&style(&fam));
        let xopq = d.axis("XOPQ").unwrap();
        assert_eq!(xopq.name.as_deref(), Some("Parametric Thick Stroke"));
        assert_eq!((xopq.min, xopq.default, xopq.max), (27.0, 96.0, 175.0));
        assert!(d.axes.len() >= 12, "{:?}", d.axes);
        let black = d.instances.iter().find(|i| i.name == "Black").unwrap();
        assert_eq!(black.coords.len(), d.axes.len());
        assert!(black.coords.iter().any(|(t, v)| t == "wght" && *v == 900.0));
    }
}
