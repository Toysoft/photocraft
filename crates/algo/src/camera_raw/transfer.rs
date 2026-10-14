//! Table-driven transfer curves for the per-pixel stages.
//!
//! A develop decodes and re-encodes every channel of every pixel several times (white balance,
//! Calibration, Whites and Blacks, …), and `powf` dominated those stages. A [`Table`] samples a
//! curve on `[2⁻¹⁶, 1)` at [`STEPS`] points per octave, found from the bits of the input: in
//! every octave the mantissa is linear in the value, so its top bits pick the segment and the
//! rest are the position inside it. Equal *relative* steps keep the linear interpolation exact
//! to about `|g (g − 1)| / (8 · STEPS²)` of the value for a power law `x^g` (below 1.5·10⁻⁶ for
//! every gamma from 0.25 to 4, a tenth of a 16-bit level), including near black, where a
//! uniform table fails on curves like `x^(1/2.4)`. Inputs outside the table (0 and below, the
//! deep shadows under 2⁻¹⁶, 1 and above, NaN) take the exact curve.

use std::sync::{Mutex, OnceLock};

use crate::photo_util::{linear_to_srgb, srgb_to_linear};

/// Octaves the table spans below 1.
const OCTAVES: i32 = 16;
/// Mantissa bits that pick the segment within an octave.
const BITS: u32 = 10;
/// Segments per octave.
const STEPS: usize = 1 << BITS;
/// Smallest tabulated input (2⁻¹⁶).
const LOW: f32 = 1.0 / 65536.0;
/// Mantissa bits that give the position within a segment.
const FRAC_BITS: u32 = 23 - BITS;

/// A curve sampled on `[2⁻¹⁶, 1]` (see the module docs).
pub(crate) struct Table {
    v: Box<[f32]>,
}

impl Table {
    /// Samples `f` (evaluated in `f64`) at the segment ends.
    pub(crate) fn new(f: impl Fn(f64) -> f64) -> Self {
        let mut v = Vec::with_capacity(OCTAVES as usize * STEPS + 1);
        for e in -OCTAVES..0 {
            let base = 2f64.powi(e);
            for m in 0..STEPS {
                v.push(f(base * (1.0 + m as f64 / STEPS as f64)) as f32);
            }
        }
        v.push(f(1.0) as f32);
        Table { v: v.into_boxed_slice() }
    }

    /// `f(x)` for `x` in the table's range, otherwise `exact(x)`.
    #[inline]
    pub(crate) fn get(&self, x: f32, exact: impl Fn(f32) -> f32) -> f32 {
        if !(LOW..1.0).contains(&x) {
            return exact(x);
        }
        let b = x.to_bits();
        // The exponent is −16 … −1 here, so the octave index is 0 … 15.
        let i = (((b >> 23) as i32 - 127 + OCTAVES) as usize) * STEPS + ((b >> FRAC_BITS) as usize & (STEPS - 1));
        let t = (b & ((1 << FRAC_BITS) - 1)) as f32 * (1.0 / (1u32 << FRAC_BITS) as f32);
        let (a, c) = (self.v[i], self.v[i + 1]);
        a + (c - a) * t
    }
}

/// A transfer curve and its inverse.
pub(crate) struct Curves {
    pub(crate) decode: Table,
    pub(crate) encode: Table,
}

fn srgb() -> &'static Curves {
    static SRGB: OnceLock<Curves> = OnceLock::new();
    SRGB.get_or_init(|| Curves {
        decode: Table::new(|v| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }),
        encode: Table::new(|v| if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }),
    })
}

/// [`srgb_to_linear`] through the table.
#[inline]
pub(crate) fn srgb_decode(v: f32) -> f32 {
    srgb().decode.get(v, srgb_to_linear)
}

/// [`linear_to_srgb`] through the table.
#[inline]
pub(crate) fn srgb_encode(v: f32) -> f32 {
    srgb().encode.get(v, linear_to_srgb)
}

/// Distinct gammas kept; documents use a handful (ProPhoto's 1.8, Adobe RGB's 2.2, 1.0).
const GAMMAS: usize = 16;

/// The power-law curves for `gamma` (`x^gamma` and `x^(1/gamma)`), built once per gamma and
/// kept for the life of the process; `None` once [`GAMMAS`] other gammas are kept (callers then
/// use the exact curve).
pub(crate) fn power(gamma: f32) -> Option<&'static Curves> {
    static KEPT: Mutex<Vec<(u32, &'static Curves)>> = Mutex::new(Vec::new());
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, c)) = kept.iter().find(|(g, _)| *g == gamma.to_bits()) {
        return Some(c);
    }
    if kept.len() >= GAMMAS {
        return None;
    }
    let g = f64::from(gamma);
    let curves: &'static Curves = Box::leak(Box::new(Curves { decode: Table::new(|v| v.powf(g)), encode: Table::new(|v| v.powf(1.0 / g)) }));
    kept.push((gamma.to_bits(), curves));
    Some(curves)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Largest difference from `exact` over every 16-bit level and a dense sweep of the
    /// shadows, where the curves bend most.
    fn worst(table: impl Fn(f32) -> f32, exact: impl Fn(f32) -> f32) -> f32 {
        let levels = (0..=65535).map(|i| i as f32 / 65535.0);
        let shadows = (0..=200_000).map(|i| i as f32 / 200_000.0 * 0.01);
        let edges = [0.0, LOW * 0.5, LOW, 0.5, 0.999_999_94, 1.0, 1.5, -0.25, 7.0];
        levels.chain(shadows).chain(edges).map(|v| (table(v) - exact(v)).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn srgb_tables_match_the_exact_curves() {
        assert!(worst(srgb_decode, srgb_to_linear) < 4e-7);
        assert!(worst(srgb_encode, linear_to_srgb) < 4e-7);
    }

    #[test]
    fn power_tables_match_powf_from_gamma_half_to_four() {
        for gamma in [0.5f32, 1.0, 1.8, 563.0 / 256.0, 2.4, 4.0] {
            let c = power(gamma).expect("room for a few gammas");
            let dec = |v: f32| v.max(0.0).powf(gamma);
            let enc = |v: f32| v.max(0.0).powf(1.0 / gamma);
            assert!(worst(|v| c.decode.get(v, dec), dec) < 2e-6, "decode {gamma}");
            assert!(worst(|v| c.encode.get(v, enc), enc) < 2e-6, "encode {gamma}");
        }
    }

    #[test]
    fn a_gamma_is_built_once() {
        assert!(std::ptr::eq(power(1.8).expect("kept"), power(1.8).expect("kept")));
    }
}
