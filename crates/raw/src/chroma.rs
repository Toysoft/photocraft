//! Colour noise reduction: Camera Raw's Detail › Noise Reduction › Color, Color Detail and Color
//! Smoothness, on linear RGB.
//!
//! Measured black-box from Photoshop 25.4 opening Nikon D4 NEFs through Camera Raw (35 frames,
//! ISO 100–12800, with sharpening and luminance noise reduction off, Color 0…100, Detail and
//! Smoothness 0 / 50 / 100, exposure −1) and synthetic D4 frames (known chroma steps, thin lines,
//! gratings; clean and with ISO 8000 noise; the ISO tag changed):
//!
//! * Luminance is never changed (luminance gratings come back unchanged at every setting).
//! * It acts on raw data before the tone curve: the scene-domain change at exposure −1
//!   correlates 0.98–0.996 with the one at exposure 0. Flat noisy areas come out at the
//!   linear-light mean of their colour, so the averaging happens in linear light.
//! * It is not a blur: weak colour differences (comparable to the noise) are smoothed over
//!   tens of pixels, strong ones stay sharp, and a colour edge that is also a luminance edge
//!   stays sharp even when its colour difference is weak. Thin colour lines and fine colour
//!   detail fade first (a 2 px line keeps 5 % at Color 25, a 16 px one 89 %).
//! * The strength follows the frame's noise: Camera Raw takes it from the camera and ISO (the
//!   same clean frame tagged ISO 100 instead of 8000 is smoothed far less, and at ISO 100 fine
//!   foliage colour survives that ISO 6400 loses). Clean-room, we estimate it from the image
//!   itself (below).
//! * Color sets how large a difference counts as noise; Detail protects the finest scales
//!   (2–8 px); Smoothness reaches coarser scales (blotches of 100+ px).
//!
//! The model: the chroma (`R − Y`, `B − Y` in linear light, `Y` the caller's luminance) is split
//! into a Laplacian pyramid (P. Burt, E. Adelson, *The Laplacian Pyramid as a Compact Image
//! Code*, IEEE Trans. Communications 1983) whose expansion step is a joint bilateral upsampling
//! (J. Kopf et al., *Joint Bilateral Upsampling*, SIGGRAPH 2007) guided by the luminance and by
//! the pixel's own colour, so colour from a coarser level crosses neither luminance edges nor
//! strong colour edges. Each level's detail is scaled by a Wiener-like gain `e / (e + t²)` (`e`
//! its energy over 3 × 3 pixels), the threshold `t` a multiple of that level's noise at the
//! pixel's luminance. The noise is a Poisson–Gaussian model (variance `a·Y + b`, A. Foi et al.,
//! *Practical Poissonian-Gaussian Noise Modeling and Fitting for Single-Image Raw-Data*, IEEE
//! TIP 2008) read from the finest level's detail in 16 luminance bands (a low quantile, so
//! texture does not count as noise) and fitted robustly (Theil–Sen); coarser levels' noise
//! follows by fixed ratios measured on flat noisy frames. On clean frames the finest levels'
//! thresholds fall further, as Camera Raw's do.
//!
//! Data is linear and relative to white (1 = the sensor's or the document's white), before any
//! exposure gain: the noise level is read in those units, so the result does not depend on
//! exposure. Fitted on 10 D4 frames; on 14 others (ISO 100–12800) the mean ΔE76 to Photoshop's
//! Color 25 open is 1.6 (no reduction: 4.3; PhotoCraft's previous chroma blur: 2.8).

use crate::par;

/// Camera Raw's Color, Color Detail and Color Smoothness sliders, 0…100.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorNoise {
    /// Color: 0 is off.
    pub amount: f32,
    /// Color Detail (default 50): higher keeps finer colour detail.
    pub detail: f32,
    /// Color Smoothness (default 50): higher also smooths larger colour blotches.
    pub smoothness: f32,
}

impl Default for ColorNoise {
    fn default() -> Self {
        ColorNoise { amount: 0.0, detail: 50.0, smoothness: 50.0 }
    }
}

impl ColorNoise {
    /// `true` when nothing is reduced (Color 0, or not a number).
    pub fn is_off(&self) -> bool {
        self.amount.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
    }

    fn clamped(&self) -> ColorNoise {
        let c = |v: f32, d: f32| if v.is_finite() { v.clamp(0.0, 100.0) } else { d };
        ColorNoise { amount: c(self.amount, 0.0), detail: c(self.detail, 50.0), smoothness: c(self.smoothness, 50.0) }
    }
}

/// Pyramid levels of the full-size frame (finest first). Level `l` holds detail at scales of
/// about `2^(l+1)` px.
const LEVELS: usize = 8;
/// Noise of each level's chroma detail relative to level 0, measured on flat noisy D4 frames
/// (demosaicked chroma noise is coarser than white noise: level 1 carries the most); beyond
/// the table it halves per level.
const NOISE_RATIO: [f32; LEVELS] = [1.0, 1.07, 0.73, 0.415, 0.21, 0.105, 0.053, 0.027];
/// Luminance range of the guided upsampling, in natural-log units: a luminance step of a few
/// percent already stops most colour from crossing it.
const LUMA_RANGE: f32 = 0.065;
/// Threshold multipliers per full-size level at Color 25, Detail 50, Smoothness 50, fitted to
/// Photoshop's opens (ΔE 1.38 against them on the fitting frames; no reduction: 4.6).
const BASE: [f32; LEVELS] = [11.4, 3.7, 2.3, 1.5, 1.07, 0.38, 0.38, 0.38];

/// Threshold multiplier of full-size level `l` (fractional: levels are interpolated) for the
/// sliders, before the level's noise ratio.
fn threshold(p: &ColorNoise, l: f32, noise: ChromaNoise) -> f32 {
    let at = |t: &[f32; LEVELS], l: f32| -> f32 {
        let l = l.clamp(0.0, (LEVELS - 1) as f32);
        let i = (l as usize).min(LEVELS - 2);
        let f = l - i as f32;
        t[i] * (1.0 - f) + t[i + 1] * f
    };
    let mut k = at(&BASE, l) * amount_scale(p.amount);
    // Clean frames: the finest levels' colour detail is mostly real (Camera Raw at ISO 100 keeps
    // foliage colour that it smooths away at ISO 6400), so their threshold falls with the noise.
    k *= fine_scale(noise).powf((1.0 - l / 2.0).max(0.0));
    // Detail: below 50 the finest levels (up to ~16 px) are smoothed harder, above 50 their
    // threshold falls so 2–8 px colour detail survives (Photoshop at Detail 100 keeps much of a
    // 2 px colour line that Detail 50 removes).
    let d = (p.detail - 50.0) / 50.0;
    k *= if d >= 0.0 { 2f32.powf(-DETAIL_UP * d * (1.0 - l / 3.0).max(0.0)) } else { 2f32.powf(-d * (1.0 - l / 4.0).clamp(0.0, 1.0)) };
    // Smoothness: how many levels take part (4 at 0, 6 at 50, 8 at 100; a weak colour step
    // spreads ±20, ±60 and over ±150 px in Photoshop), the last one partly.
    k * (reach(p) - l).clamp(0.0, 1.0)
}

/// Chroma range of the upsampling at full-size level `l`, in units of that level's noise:
/// colour differences well above it (strong colour edges) never mix, weaker ones do.
fn chroma_range(p: &ColorNoise, l: f32) -> f32 {
    let _ = l;
    CHROMA_RANGE * amount_scale(p.amount).max(1e-3)
}

/// [`chroma_range`] at Color 25.
const CHROMA_RANGE: f32 = 3.0;

/// Multiplier of the finest level's threshold for the frame's noise level: the chroma noise at
/// mid grey (Y 0.18 of white) relative to √Y, ~0.002 for a D4 at ISO 100, ~0.006 at ISO 2500,
/// ~0.01 at ISO 6400. 1 from about ISO 2500 up, down to 0.1 at base ISO (fitted to Photoshop's
/// opens of D4 frames at ISO 100–12800).
fn fine_scale(noise: ChromaNoise) -> f32 {
    let n = (noise.variance(0.18) / 0.18).sqrt();
    if !n.is_finite() {
        return 1.0;
    }
    (n / FINE_NOISE).powi(2).clamp(0.1, 1.0)
}

/// Noise level from which the finest levels get their full threshold.
const FINE_NOISE: f32 = 0.007;

/// How steeply Detail above 50 lowers the finest levels' threshold (2^-x at Detail 100).
const DETAIL_UP: f32 = 2.2;

/// The thresholds' scale for Color, 1 at the default 25: a power of Color / 25, 0.75 below 25
/// and 0.57 above (best fits to Photoshop's opens at Color 10, 50 and 100: Color 100 is 2.2 ×
/// Color 25, not 4 ×).
fn amount_scale(amount: f32) -> f32 {
    let a = amount.max(0.0) / 25.0;
    a.powf(if a <= 1.0 { 0.75 } else { 0.57 })
}

/// Full-size levels that take part (fractional), from Smoothness.
fn reach(p: &ColorNoise) -> f32 {
    4.0 + 4.0 * p.smoothness / 100.0
}

/// Noise of full-size level `l` relative to level 0.
fn noise_ratio(l: f32) -> f32 {
    let last = (LEVELS - 1) as f32;
    if l >= last {
        return NOISE_RATIO[LEVELS - 1] * 0.5f32.powf(l - last);
    }
    let l = l.max(0.0);
    let i = (l as usize).min(LEVELS - 2);
    let f = l - i as f32;
    // Geometric interpolation: the ratios fall by about half a level.
    NOISE_RATIO[i].powf(1.0 - f) * NOISE_RATIO[i + 1].powf(f)
}

/// The frame's chroma noise: variance `a·Y + b` of the finest level's detail at luminance `Y`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromaNoise {
    pub a: f32,
    pub b: f32,
}

impl ChromaNoise {
    fn variance(&self, y: f32) -> f32 {
        (self.a * y.max(0.0) + self.b).max(0.0)
    }

    #[cfg(test)]
    fn sigma(&self, y: f32) -> f32 {
        self.variance(y).sqrt()
    }
}

/// One channel of an image level.
#[derive(Clone)]
struct Plane {
    w: usize,
    h: usize,
    v: Vec<f32>,
}

impl Plane {
    fn at(&self, x: usize, y: usize) -> f32 {
        self.v.get(y * self.w + x).copied().unwrap_or(0.0)
    }
}

const K5: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];

/// Index `i` of a line of `n` samples, reflected about the ends (`-1` → 0, `n` → `n − 1`).
fn reflect(i: isize, n: usize) -> usize {
    let n = n as isize;
    if n <= 1 {
        return 0;
    }
    let mut i = i;
    loop {
        if i < 0 {
            i = -i - 1;
        } else if i >= n {
            i = 2 * n - i - 1;
        } else {
            return i as usize;
        }
    }
}

/// The next coarser level: a 5-tap binomial blur, then every second sample.
fn down(p: &Plane) -> Plane {
    let (w2, h2) = (p.w.div_ceil(2), p.h.div_ceil(2));
    // Horizontal pass at even columns, all rows.
    let mut tmp = vec![0.0f32; w2 * p.h];
    par::chunks_mut(&mut tmp, w2 * par::band_rows(w2), |b, chunk| {
        let y0 = b * par::band_rows(w2);
        for (r, row) in chunk.chunks_mut(w2).enumerate() {
            let src = &p.v[(y0 + r) * p.w..(y0 + r + 1) * p.w];
            for (i, o) in row.iter_mut().enumerate() {
                let x = 2 * i as isize;
                *o = K5.iter().enumerate().map(|(t, k)| k * src[reflect(x + t as isize - 2, p.w)]).sum();
            }
        }
    });
    let mut out = vec![0.0f32; w2 * h2];
    par::chunks_mut(&mut out, w2 * par::band_rows(w2), |b, chunk| {
        let j0 = b * par::band_rows(w2);
        for (r, row) in chunk.chunks_mut(w2).enumerate() {
            let y = 2 * (j0 + r) as isize;
            let rows: [usize; 5] = std::array::from_fn(|t| reflect(y + t as isize - 2, p.h) * w2);
            for (i, o) in row.iter_mut().enumerate() {
                *o = K5.iter().zip(rows).map(|(k, r0)| k * tmp[r0 + i]).sum();
            }
        }
    });
    Plane { w: w2, h: h2, v: out }
}

/// Coarse taps of fine coordinate `i` on a coarse line of `n` samples: three (index, weight)
/// pairs, a zero weight for an unused tap. Even samples sit on a coarse sample (tent
/// `1/3, 1, 1/3`), odd ones between two (`2/3, 2/3`).
fn taps(i: usize, n: usize) -> [(usize, f32); 3] {
    let last = n.saturating_sub(1);
    let c = i / 2;
    if i.is_multiple_of(2) {
        [(c.saturating_sub(1), 1.0 / 3.0), (c.min(last), 1.0), ((c + 1).min(last), 1.0 / 3.0)]
    } else {
        [(c.min(last), 2.0 / 3.0), ((c + 1).min(last), 2.0 / 3.0), (c.min(last), 0.0)]
    }
}

/// Plain expansion taps (zero insertion and the binomial kernel): even samples `(1, 6, 1) / 8`,
/// odd ones the mean of their two neighbours.
fn plain_taps(i: usize, n: usize) -> [(usize, f32); 3] {
    let last = n.saturating_sub(1);
    let k = i / 2;
    if i.is_multiple_of(2) {
        [(k.saturating_sub(1), 0.125), (k.min(last), 0.75), ((k + 1).min(last), 0.125)]
    } else {
        [(k.min(last), 0.5), ((k + 1).min(last), 0.5), (k.min(last), 0.0)]
    }
}

/// The plain expansion of coarse `c` at fine pixel (`x`, `y`).
fn plain_at(c: &Plane, x: usize, y: usize) -> f32 {
    let (tx, ty) = (plain_taps(x, c.w), plain_taps(y, c.h));
    ty.iter().map(|&(cy, wy)| wy * tx.iter().map(|&(cx, wx)| wx * c.at(cx, cy)).sum::<f32>()).sum()
}

/// The planes one level of the reconstruction works on (see [`reconstruct_level`]).
#[derive(Clone, Copy)]
struct Level<'a> {
    g: &'a [Plane; 2],
    g1: &'a [Plane; 2],
    out1: &'a [Plane; 2],
    lf: &'a Plane,
    lc: &'a Plane,
}

/// One level of the reconstruction: the coarse level's Gaussian `g1` and the result so far
/// `out1` are expanded to the finer size with the same guided weights: fine log-luminance `lf`
/// against the coarse `lc` (colour does not cross luminance edges) and the fine pixel's own
/// chroma against the coarse one within `rk · σ(Y)` (strong colour edges do not mix). The finer
/// Gaussian `g`'s detail over the expanded `g1` is added back scaled by a Wiener gain
/// `e / (e + t²)`, `t = k · σ(Y)`, `e` the detail's mean square over a 3 × 3 window.
fn reconstruct_level(lv: &Level<'_>, k: f32, rk: f32, noise: ChromaNoise) -> [Plane; 2] {
    let Level { g, g1, out1, lf, lc } = *lv;
    let (w, h) = (lf.w, lf.h);
    let inv = 1.0 / (2.0 * LUMA_RANGE * LUMA_RANGE);
    // The coarse level interleaved (luminance, Gaussian chroma, result chroma): one load a tap.
    let at = |p: &Plane, j: usize| p.v.get(j).copied().unwrap_or(0.0);
    let coarse: Vec<[f32; 5]> = par::map(lc.v.len(), |j| [at(lc, j), at(&g1[0], j), at(&g1[1], j), at(&out1[0], j), at(&out1[1], j)]);
    // Pass 1: expansions, detail, its energy and the noise variance.
    let mut px = vec![[0.0f32; 6]; w * h];
    // Each column's coarse taps, worked out once for the level.
    let column_taps: Vec<[(usize, f32); 3]> = (0..w).map(|x| taps(x, lc.w)).collect();
    par::chunks_mut(&mut px, w * par::band_rows(w), |band, chunk| {
        let y0 = band * par::band_rows(w);
        for (r, row) in chunk.chunks_mut(w).enumerate() {
            let y = y0 + r;
            let ty = taps(y, lc.h);
            for (x, o) in row.iter_mut().enumerate() {
                let tx = column_taps[x];
                let i = y * w + x;
                let l = at(lf, i);
                let (f0, f1) = (at(&g[0], i), at(&g[1], i));
                let var = noise.variance(l.exp());
                // Chroma range: coarse colour much unlike this pixel's own does not flow in.
                let tau2 = rk * rk * var;
                let cinv = if tau2 > 0.0 { 0.5 / tau2 } else { 0.0 };
                let mut s = [0.0f32; 4];
                let mut ws = 0.0f32;
                for &(cy, wy) in &ty {
                    if wy == 0.0 {
                        continue;
                    }
                    let base = cy * lc.w;
                    for &(cx, wx) in &tx {
                        if wx == 0.0 {
                            continue;
                        }
                        let [cl, a0, a1, o0, o1] = coarse.get(base + cx).copied().unwrap_or([0.0; 5]);
                        let d = l - cl;
                        let dc = (f0 - a0) * (f0 - a0) + (f1 - a1) * (f1 - a1);
                        // A floor keeps isolated pixels (no neighbour of similar luminance or
                        // colour) interpolated rather than undefined.
                        let wt = wy * wx * (neg_exp(d * d * inv + dc * cinv) + 1e-6);
                        s[0] += wt * a0;
                        s[1] += wt * a1;
                        s[2] += wt * o0;
                        s[3] += wt * o1;
                        ws += wt;
                    }
                }
                let s = if ws > 0.0 { s.map(|v| v / ws) } else { [0.0; 4] };
                let (d0, d1) = (f0 - s[0], f1 - s[1]);
                *o = [s[2], s[3], d0, d1, (d0 * d0 + d1 * d1) * 0.5, var];
            }
        }
    });
    let local = box_mean(&Plane { w, h, v: par::map(w * h, |i| px.get(i).map_or(0.0, |q| q[4])) }, ENERGY_RADIUS);
    // Pass 2: the gains.
    let k2 = k * k;
    let part = |c: usize| {
        par::map(w * h, |i| {
            let [u0, u1, d0, d1, _, var] = px.get(i).copied().unwrap_or([0.0; 6]);
            let e = at(&local, i);
            let gain = if e > 0.0 { e / (e + k2 * var) } else { 0.0 };
            if c == 0 { u0 + gain * d0 } else { u1 + gain * d1 }
        })
    };
    [Plane { w, h, v: part(0) }, Plane { w, h, v: part(1) }]
}

/// Radius of the window the detail energy is averaged over.
const ENERGY_RADIUS: usize = 1;

/// Mean over a `(2r + 1)²` window, edges reflected.
fn box_mean(p: &Plane, r: usize) -> Plane {
    let (w, h) = (p.w, p.h);
    if r == 0 {
        return p.clone();
    }
    let k = 1.0 / (2 * r + 1) as f32;
    let ri = r as isize;
    let rows = par::map(h, |y| {
        let src = p.v.get(y * w..(y + 1) * w).unwrap_or(&[]);
        (0..w).map(|x| (-ri..=ri).map(|t| src.get(reflect(x as isize + t, w)).copied().unwrap_or(0.0)).sum::<f32>() * k).collect::<Vec<f32>>()
    });
    let v = par::map(w * h, |i| {
        let (x, y) = (i % w, i / w);
        (-ri..=ri).map(|t| rows.get(reflect(y as isize + t, h)).and_then(|row| row.get(x)).copied().unwrap_or(0.0)).sum::<f32>() * k
    });
    Plane { w, h, v }
}

/// `exp(-x)` for `x ≥ 0` from a table (the range weights need two or three digits, and this is
/// the inner loop).
fn neg_exp(x: f32) -> f32 {
    const MAX: f32 = 16.0;
    const STEPS: usize = 1024;
    static TABLE: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| (0..=STEPS + 1).map(|i| (-(i as f32) * MAX / STEPS as f32).exp()).collect());
    if x.is_nan() || x >= MAX {
        return 0.0;
    }
    let f = x.max(0.0) * (STEPS as f32 / MAX);
    let i = f as usize;
    let (a, b) = (t.get(i).copied().unwrap_or(0.0), t.get(i + 1).copied().unwrap_or(0.0));
    a + (b - a) * (f - i as f32)
}

/// Fits the frame's chroma noise ([`ChromaNoise`]) to the finest pyramid level: the detail of
/// the chroma `c` over the plain expansion of its next level `c1`, against the luminance `y`
/// (and its log `ly`). 16 luminance bands of equal pixel count, in each the median detail
/// magnitude taken as the noise there (robust to the minority of pixels on edges and texture),
/// then a least-squares line through the variances weighted by relative error.
fn estimate(c: &[Plane; 2], c1: &[Plane; 2], y: &Plane, ly: &Plane) -> ChromaNoise {
    let (w, h) = (y.w, y.h);
    // A sample every third pixel each way is plenty for medians and keeps the sort short.
    let rows: Vec<usize> = (0..h).step_by(3).collect();
    let per_row = par::map(rows.len(), |k| {
        let yy = rows.get(k).copied().unwrap_or(0);
        let mut v = Vec::with_capacity(w.div_ceil(3));
        for xx in (0..w).step_by(3) {
            let i = yy * w + xx;
            let d0 = c[0].v.get(i).copied().unwrap_or(0.0) - plain_at(&c1[0], xx, yy);
            let d1 = c[1].v.get(i).copied().unwrap_or(0.0) - plain_at(&c1[1], xx, yy);
            let m = ((d0 * d0 + d1 * d1) * 0.5).sqrt();
            let (l, yv) = (ly.v.get(i).copied().unwrap_or(0.0), y.v.get(i).copied().unwrap_or(0.0));
            if m.is_finite() && l.is_finite() && yv.is_finite() {
                v.push((l, yv, m));
            }
        }
        v
    });
    let mut samples: Vec<(f32, f32, f32)> = per_row.into_iter().flatten().collect();
    if samples.is_empty() {
        return ChromaNoise { a: 0.0, b: 0.0 };
    }
    samples.sort_unstable_by(|p, q| p.0.total_cmp(&q.0));
    let n = samples.len();
    let (lo, hi) = (n / 50, n - n / 50);
    let bins = 16;
    let mut pts: Vec<(f32, f32)> = Vec::new();
    for k in 0..bins {
        let (s, e) = (lo + (hi - lo) * k / bins, lo + (hi - lo) * (k + 1) / bins);
        let Some(part) = samples.get(s..e).filter(|p| p.len() >= 200) else { continue };
        let mut ys: Vec<f32> = part.iter().map(|t| t.1).collect();
        let mut ms: Vec<f32> = part.iter().map(|t| t.2).collect();
        let sd = NOISE_SCALE * quantile(&mut ms, NOISE_QUANTILE);
        // Clipped highlights and flat synthetic areas have no detail at all: no information.
        if sd > 0.0 {
            pts.push((median(&mut ys), sd * sd));
        }
    }
    if pts.len() < 2 {
        let mut ms: Vec<f32> = samples.iter().map(|t| t.2).filter(|m| *m > 0.0).collect();
        let mut ys: Vec<f32> = samples.iter().map(|t| t.1).collect();
        let sd = 1.4826 * median(&mut ms);
        return ChromaNoise { a: sd * sd / median(&mut ys).max(1e-9), b: 0.0 };
    }
    fit_noise(&pts)
}

/// A robust line `v = a·y + b` through (luminance, variance) points: `a` the median of the
/// pairwise slopes, `b` the median offset (H. Theil 1950, P. K. Sen 1968), so a few bands where
/// texture or flat clipped areas dominate do not bend it. Without a usable line, photon noise
/// alone: `a` the median of `v / y`.
fn fit_noise(pts: &[(f32, f32)]) -> ChromaNoise {
    let mut slopes: Vec<f32> = Vec::new();
    for (i, p) in pts.iter().enumerate() {
        for q in pts.iter().skip(i + 1) {
            let dy = q.0 - p.0;
            if dy.abs() > 1e-12 {
                slopes.push((q.1 - p.1) / dy);
            }
        }
    }
    let a = median(&mut slopes);
    let mut offsets: Vec<f32> = pts.iter().map(|p| p.1 - a * p.0).collect();
    let b = median(&mut offsets);
    if a.is_finite() && b.is_finite() && a > 0.0 && b >= 0.0 {
        return ChromaNoise { a, b };
    }
    let mut ratios: Vec<f32> = pts.iter().filter(|p| p.0 > 0.0).map(|p| p.1 / p.0).collect();
    let a = median(&mut ratios);
    if a.is_finite() && a > 0.0 {
        return ChromaNoise { a, b: 0.0 };
    }
    let mut vs: Vec<f32> = pts.iter().map(|p| p.1).collect();
    ChromaNoise { a: 0.0, b: median(&mut vs).max(0.0) }
}

/// The detail quantile each luminance band's noise is read from: low, so the flattest pixels
/// speak (texture only adds detail), and [`NOISE_SCALE`] makes it the same number the median
/// gives on pure noise (the detail magnitude of two Gaussian channels is Rayleigh distributed).
const NOISE_QUANTILE: f32 = 0.2;
const NOISE_SCALE: f32 = 2.612;

/// The `q` quantile of `v` (reordered).
fn quantile(v: &mut [f32], q: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let k = ((v.len() as f32 * q) as usize).min(v.len() - 1);
    let (_, x, _) = v.select_nth_unstable_by(k, f32::total_cmp);
    *x
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let m = v.len() / 2;
    let (_, x, _) = v.select_nth_unstable_by(m, f32::total_cmp);
    *x
}

/// Luminance weights of linear ProPhoto RGB (ROMM primaries, D50 white): the Y row of its
/// matrix to XYZ.
pub fn prophoto_luminance() -> [f32; 3] {
    crate::color::rgb_to_xyz(crate::color::ROMM, crate::color::D50_XY)[1].map(|v| v as f32)
}

/// What a buffer handed to [`reduce_color_noise`] is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoiseFrame {
    /// Luminance weights of the RGB, kept exactly per pixel (normalised to sum 1).
    pub weights: [f32; 3],
    /// The buffer's size relative to the full frame: 1 for the frame itself, 0.25 for a preview
    /// a quarter the size. The pyramid and thresholds follow, so a reduced preview looks like
    /// the full-size result reduced.
    pub pixel_scale: f32,
    /// The full frame's noise when known (what a full-size call returned). Previews should pass
    /// it so they match the development; `None` estimates it from the buffer itself.
    pub noise: Option<ChromaNoise>,
}

impl NoiseFrame {
    /// A full-size linear ProPhoto frame, noise estimated from it.
    pub fn prophoto() -> Self {
        NoiseFrame { weights: prophoto_luminance(), pixel_scale: 1.0, noise: None }
    }
}

/// The normalised luminance weights when `data` is a usable `w × h` buffer of `channels`
/// samples a pixel with finite values; `None` otherwise.
fn valid(data: &[f32], channels: usize, w: usize, h: usize, weights: [f32; 3]) -> Option<[f32; 3]> {
    let n = w.checked_mul(h)?;
    let sum: f32 = weights.iter().sum();
    if n == 0 || channels < 3 || data.len() != n.checked_mul(channels)? || !sum.is_finite() || sum.abs() < 1e-6 {
        return None;
    }
    let weights = weights.map(|v| v / sum);
    (weights.iter().all(|v| v.is_finite()) && weights[1].abs() >= 1e-6 && data.iter().all(|v| v.is_finite())).then_some(weights)
}

/// Luminance, its natural log (the guide, floored at 1e-7) and the two chroma planes `R − Y`,
/// `B − Y` of a buffer [`valid`] accepted.
fn planes(data: &[f32], channels: usize, w: usize, h: usize, weights: [f32; 3]) -> (Plane, Plane, [Plane; 2]) {
    let n = w * h;
    let rgb_at = |i: usize| -> [f32; 3] {
        let q = data.get(i * channels..i * channels + 3).unwrap_or(&[0.0; 3]);
        [q[0], q[1], q[2]]
    };
    let lum = |q: [f32; 3]| weights[0] * q[0] + weights[1] * q[1] + weights[2] * q[2];
    let y = Plane { w, h, v: par::map(n, |i| lum(rgb_at(i))) };
    let ly = Plane { w, h, v: par::map(n, |i| y.v.get(i).map_or(0.0, |v| v.max(1e-7).ln())) };
    let chroma = |k: usize| par::map(n, |i| rgb_at(i)[k] - y.v.get(i).copied().unwrap_or(0.0));
    let c = [Plane { w, h, v: chroma(0) }, Plane { w, h, v: chroma(2) }];
    (y, ly, c)
}

/// The colour noise of a full-size linear frame (`w × h` pixels of `channels` samples, RGB
/// first) without reducing it: what [`reduce_color_noise`] would estimate and return, for
/// [`NoiseFrame::noise`] of reduced previews. `None` for a buffer it would not work on (empty,
/// mis-sized, non-finite, smaller than 4 × 4).
pub fn estimate_color_noise(data: &[f32], channels: usize, w: usize, h: usize, weights: [f32; 3]) -> Option<ChromaNoise> {
    let weights = valid(data, channels, w, h, weights)?;
    if w < 4 || h < 4 {
        return None;
    }
    let (y, ly, c) = planes(data, channels, w, h, weights);
    let c1 = [down(&c[0]), down(&c[1])];
    Some(estimate(&c, &c1, &y, &ly))
}

/// Reduces the colour noise of linear RGB `data` (`w × h` pixels of `channels` samples, RGB
/// first: 3 or 4; relative to white, before any exposure gain), keeping every pixel's luminance
/// (`frame.weights · rgb`) and any further channel. Returns the full-size frame's noise model it used, or `None` when nothing was done
/// (Color 0, an empty or mis-sized buffer, non-finite values, a buffer too small for a level).
pub fn reduce_color_noise(data: &mut [f32], channels: usize, w: usize, h: usize, p: &ColorNoise, frame: &NoiseFrame) -> Option<ChromaNoise> {
    let p = p.clamped();
    if p.is_off() {
        return None;
    }
    let weights = valid(data, channels, w, h, frame.weights)?;
    // Buffer level `j` is full-size level `j + shift`.
    let scale = if frame.pixel_scale.is_finite() { frame.pixel_scale.clamp(1.0 / 256.0, 1.0) } else { 1.0 };
    let shift = -scale.log2();
    let levels = ((reach(&p) - shift).ceil().max(0.0) as usize).min(LEVELS);
    let (y, ly, c) = planes(data, channels, w, h, weights);
    // Gaussian pyramids of the chroma and of the log-luminance; stop before a level would be
    // smaller than a few pixels.
    let mut gc = vec![c];
    let mut gl = vec![ly];
    for _ in 0..levels.max(1) {
        let (Some(last_c), Some(last_l)) = (gc.last(), gl.last()) else { break };
        if last_l.w < 4 || last_l.h < 4 {
            break;
        }
        let next = [down(&last_c[0]), down(&last_c[1])];
        let nl = down(last_l);
        gc.push(next);
        gl.push(nl);
    }
    let (Some(c0), Some(c1)) = (gc.first(), gc.get(1)) else { return None };
    let noise = match frame.noise {
        Some(nz) if nz.a.is_finite() && nz.b.is_finite() => ChromaNoise { a: nz.a.max(0.0), b: nz.b.max(0.0) },
        _ => {
            // The buffer's finest level is the full frame's level `shift`.
            let e = estimate(c0, c1, &y, &gl[0]);
            let r = noise_ratio(shift).powi(2).max(1e-12);
            ChromaNoise { a: e.a / r, b: e.b / r }
        }
    };
    let top = gc.len().min(levels + 1) - 1;
    if top == 0 {
        return None;
    }
    let mut out = gc[top].clone();
    for j in (0..top).rev() {
        let l = j as f32 + shift;
        let k = threshold(&p, l, noise) * noise_ratio(l);
        let rk = chroma_range(&p, l) * noise_ratio(l);
        let lv = Level { g: &gc[j], g1: &gc[j + 1], out1: &out, lf: &gl[j], lc: &gl[j + 1] };
        out = reconstruct_level(&lv, k, rk, noise);
    }
    // Back to RGB with the luminance kept; the new chroma is shortened where it would take a
    // channel below zero (deep shadows next to strong colour).
    let (wr, wg, wb) = (weights[0], weights[1], weights[2]);
    let rows = par::band_rows(w);
    par::chunks_mut(data, channels * w * rows, |b, chunk| {
        let start = b * rows * w;
        for (j, q) in chunk.chunks_exact_mut(channels).enumerate() {
            let i = start + j;
            let l = y.v.get(i).copied().unwrap_or(0.0);
            if l.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                continue;
            }
            let (c0, c1) = (out[0].v.get(i).copied().unwrap_or(0.0), out[1].v.get(i).copied().unwrap_or(0.0));
            let cg = -(wr * c0 + wb * c1) / wg;
            let mut s = 1.0f32;
            for c in [c0, cg, c1] {
                if l + s * c < 0.0 {
                    s = s.min(l / -c);
                }
            }
            if let [r, g, bl, ..] = q {
                *r = l + s * c0;
                *g = l + s * cg;
                *bl = l + s * c1;
            }
        }
    });
    Some(noise)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic standard normal samples (LCG + Box–Muller).
    fn gauss(seed: u64) -> impl FnMut() -> f32 {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 40) as f32 + 0.5) / (1u64 << 24) as f32
        };
        move || {
            let (u, v) = (next(), next());
            (-2.0 * u.ln()).sqrt() * (std::f32::consts::TAU * v).cos()
        }
    }

    /// A linear ProPhoto frame from `colour(x, y)` with per-channel Poisson-like noise.
    fn frame(w: usize, h: usize, noise: f32, seed: u64, colour: impl Fn(usize, usize) -> [f32; 3]) -> Vec<f32> {
        let mut g = gauss(seed);
        let mut v = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                for c in colour(x, y) {
                    v.push((c + noise * c.max(0.0).sqrt() * g()).max(0.0));
                }
            }
        }
        v
    }

    fn lum(v: &[f32]) -> Vec<f32> {
        let wt = prophoto_luminance();
        let s: f32 = wt.iter().sum();
        v.as_chunks::<3>().0.iter().map(|q| (wt[0] * q[0] + wt[1] * q[1] + wt[2] * q[2]) / s).collect()
    }

    /// Per-pixel chroma `R − Y`, `B − Y`.
    fn chroma(v: &[f32]) -> Vec<[f32; 2]> {
        v.as_chunks::<3>().0.iter().zip(lum(v)).map(|(q, l)| [q[0] - l, q[2] - l]).collect()
    }

    fn std_dev(v: impl Iterator<Item = f32>) -> f32 {
        let v: Vec<f32> = v.collect();
        let m = v.iter().sum::<f32>() / v.len() as f32;
        (v.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / v.len() as f32).sqrt()
    }

    fn reduce(v: &mut [f32], w: usize, h: usize, amount: f32) -> Option<ChromaNoise> {
        reduce_color_noise(v, 3, w, h, &ColorNoise { amount, ..Default::default() }, &NoiseFrame::prophoto())
    }

    #[test]
    fn color_zero_changes_nothing() {
        let (w, h) = (64, 48);
        let src = frame(w, h, 0.05, 1, |_, _| [0.2, 0.18, 0.15]);
        let mut v = src.clone();
        assert_eq!(reduce(&mut v, w, h, 0.0), None);
        assert_eq!(v, src);
        assert!(ColorNoise::default().is_off());
        assert!(ColorNoise { amount: f32::NAN, ..Default::default() }.is_off());
    }

    #[test]
    fn colour_noise_drops_and_luminance_stays() {
        let (w, h) = (192, 160);
        let src = frame(w, h, 0.05, 2, |_, _| [0.18, 0.18, 0.18]);
        let mut v = src.clone();
        let noise = reduce(&mut v, w, h, 25.0).expect("reduced");
        assert!(noise.a > 0.0, "{noise:?}");
        let (c0, c1) = (chroma(&src), chroma(&v));
        for k in 0..2 {
            let before = std_dev(c0.iter().map(|c| c[k]));
            let after = std_dev(c1.iter().map(|c| c[k]));
            assert!(after < before / 8.0, "channel {k}: {before} -> {after}");
        }
        for (a, b) in lum(&src).iter().zip(lum(&v)) {
            assert!((a - b).abs() <= 1e-5 * a.max(1e-3), "luminance {a} -> {b}");
        }
    }

    #[test]
    fn a_strong_colour_edge_stays_sharp() {
        // Grey on the left, saturated red on the right, same luminance, mild noise.
        let (w, h) = (192, 128);
        let red = [0.5, 0.083, 0.18];
        let src = frame(w, h, 0.02, 3, |x, _| if x < w / 2 { [0.18; 3] } else { red });
        let mut v = src.clone();
        reduce(&mut v, w, h, 25.0).expect("reduced");
        let c = chroma(&v);
        let col = |x: usize| (40..88).map(|y| c[y * w + x][0]).sum::<f32>() / 48.0;
        let (left, right) = (col(w / 2 - 3), col(w / 2 + 2));
        let full = red[0] - lum(&red)[0];
        assert!(left.abs() < 0.1 * full, "grey side picked up {left} of {full}");
        assert!(right > 0.9 * full, "red side kept {right} of {full}");
    }

    #[test]
    fn a_weak_colour_edge_on_a_luminance_edge_stays_sharp_but_not_on_its_own() {
        // A slight warm cast on the right, with and without a 2.5x luminance step.
        let (w, h) = (256, 128);
        let tint = |y: f32| [y * 1.12, y * 0.97, y * 0.93];
        let step = |lit: f32| frame(w, h, 0.03, 4, move |x, _| if x < w / 2 { [0.12; 3] } else { tint(0.12 * lit) });
        let edge = |v: &[f32]| {
            let c = chroma(v);
            let l = lum(v);
            // R − Y relative to Y, averaged over a column.
            let col = |x: usize| (32..96).map(|y| c[y * w + x][0] / l[y * w + x]).sum::<f32>() / 64.0;
            (col(w / 2 - 3), col(w / 2 + 2))
        };
        let full = {
            let t = tint(1.0);
            t[0] - lum(&t)[0]
        };
        let mut lit = step(2.5);
        reduce(&mut lit, w, h, 25.0).expect("reduced");
        let (l, r) = edge(&lit);
        assert!(l.abs() < 0.25 * full && r > 0.75 * full, "on a luminance edge: {l} / {r} of {full}");
        let mut flat = step(1.0);
        reduce(&mut flat, w, h, 25.0).expect("reduced");
        let (l, r) = edge(&flat);
        assert!(r - l < 0.6 * full, "without one the weak step spreads: {l} / {r} of {full}");
    }

    #[test]
    fn exposure_changes_nothing() {
        let (w, h) = (128, 96);
        let src = frame(w, h, 0.05, 5, |x, y| [0.1 + 0.002 * x as f32, 0.12, 0.1 + 0.002 * y as f32]);
        let mut a = src.clone();
        let mut b: Vec<f32> = src.iter().map(|v| v * 4.0).collect();
        reduce(&mut a, w, h, 40.0).expect("reduced");
        reduce(&mut b, w, h, 40.0).expect("reduced");
        for (x, y) in a.iter().zip(&b) {
            assert!((x * 4.0 - y).abs() <= 1e-4 * y.abs().max(0.05), "{x} x 4 vs {y}");
        }
    }

    #[test]
    fn fine_colour_texture_of_a_clean_frame_survives() {
        // A fine red/green checker (4 px cells, ±8 % colour) in a flat grey frame: under
        // base-ISO noise it is mostly kept, under high-ISO noise it is treated as noise.
        let (w, h) = (192, 160);
        let inside = |x: usize, y: usize| (64..128).contains(&x) && (48..112).contains(&y);
        let texture = move |x: usize, y: usize| {
            let s = if !inside(x, y) {
                0.0
            } else if (x / 4 + y / 4).is_multiple_of(2) {
                1.0
            } else {
                -1.0
            };
            [0.18 * (1.0 + 0.08 * s), 0.18 * (1.0 - 0.03 * s), 0.18]
        };
        let kept = |noise: f32| {
            let src = frame(w, h, noise, 13, texture);
            let mut v = src.clone();
            reduce(&mut v, w, h, 25.0).expect("reduced");
            let amp = |v: &[f32]| {
                let c = chroma(v);
                std_dev((0..w * h).filter(|i| inside(i % w, i / w)).map(|i| c[i][0]))
            };
            amp(&v) / amp(&frame(w, h, 0.0, 13, texture))
        };
        let (clean, noisy) = (kept(0.0015), kept(0.03));
        assert!(clean > 0.7, "base ISO keeps the texture: {clean}");
        assert!(noisy < 0.5 * clean, "high ISO smooths it: {noisy} vs {clean}");
    }

    #[test]
    fn sliders_change_the_result_the_right_way() {
        let (w, h) = (192, 160);
        let src = frame(w, h, 0.05, 6, |x, y| [0.15 + 0.1 * ((x / 24 + y / 24) % 2) as f32, 0.18, 0.18]);
        let run = |p: ColorNoise| {
            let mut v = src.clone();
            reduce_color_noise(&mut v, 3, w, h, &p, &NoiseFrame::prophoto());
            let c = chroma(&v);
            // Residual fine colour noise: differences between horizontal neighbours.
            std_dev((1..w * h).filter(|i| i % w != 0).map(|i| c[i][0] - c[i - 1][0]))
        };
        let d = ColorNoise { amount: 25.0, ..Default::default() };
        let base = run(d);
        assert!(run(ColorNoise { amount: 100.0, ..d }) < base, "more Color, less noise");
        assert!(run(ColorNoise { detail: 100.0, ..d }) > base, "more Detail keeps more fine colour");
        assert!(run(ColorNoise { detail: 0.0, ..d }) <= base * 1.001, "less Detail keeps less");
    }

    #[test]
    fn a_reduced_preview_with_the_frame_noise_matches_the_full_size_result() {
        let (w, h) = (256, 192);
        let src = frame(w, h, 0.05, 7, |x, _| if x < w / 2 { [0.18; 3] } else { [0.3, 0.14, 0.12] });
        let half = |v: &[f32]| -> Vec<f32> {
            let (w2, h2) = (w / 2, h / 2);
            let mut o = vec![0.0f32; w2 * h2 * 3];
            for y in 0..h2 {
                for x in 0..w2 {
                    for c in 0..3 {
                        let at = |dx: usize, dy: usize| v[((2 * y + dy) * w + 2 * x + dx) * 3 + c];
                        o[(y * w2 + x) * 3 + c] = (at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1)) / 4.0;
                    }
                }
            }
            o
        };
        let p = ColorNoise { amount: 25.0, ..Default::default() };
        let mut full = src.clone();
        let noise = reduce(&mut full, w, h, 25.0).expect("reduced");
        let want = chroma(&half(&full));
        let mut preview = half(&src);
        let frame = NoiseFrame { pixel_scale: 0.5, noise: Some(noise), ..NoiseFrame::prophoto() };
        reduce_color_noise(&mut preview, 3, w / 2, h / 2, &p, &frame).expect("reduced");
        let got = chroma(&preview);
        let before = chroma(&half(&src));
        let err =
            |a: &[[f32; 2]], b: &[[f32; 2]]| (a.iter().zip(b).map(|(p, q)| (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2)).sum::<f32>() / a.len() as f32).sqrt();
        let (e, e0) = (err(&got, &want), err(&before, &want));
        assert!(e < 0.35 * e0, "preview off by {e} (unreduced {e0})");
        // Estimated from the preview itself the noise comes out roughly the same (the level
        // ratios are those of demosaicked noise; this test's noise is white), and the full
        // frame's own estimate is what `estimate_color_noise` gives.
        let mut est = half(&src);
        let n2 = reduce_color_noise(&mut est, 3, w / 2, h / 2, &p, &NoiseFrame { pixel_scale: 0.5, ..NoiseFrame::prophoto() }).expect("reduced");
        assert!((n2.sigma(0.18) / noise.sigma(0.18)).ln().abs() < 0.95, "{n2:?} vs {noise:?}");
        assert_eq!(estimate_color_noise(&src, 3, w, h, prophoto_luminance()), Some(noise));
    }

    #[test]
    fn four_channel_buffers_keep_the_fourth() {
        let (w, h) = (64, 64);
        let rgb = frame(w, h, 0.05, 8, |_, _| [0.2, 0.2, 0.2]);
        let mut rgba: Vec<f32> = rgb.as_chunks::<3>().0.iter().flat_map(|q| [q[0], q[1], q[2], 0.5]).collect();
        let mut plain = rgb.clone();
        reduce(&mut plain, w, h, 25.0).expect("reduced");
        reduce_color_noise(&mut rgba, 4, w, h, &ColorNoise { amount: 25.0, ..Default::default() }, &NoiseFrame::prophoto()).expect("reduced");
        for (q, p) in rgba.as_chunks::<4>().0.iter().zip(plain.as_chunks::<3>().0) {
            assert_eq!(q[3], 0.5);
            assert!(q[..3].iter().zip(p).all(|(a, b)| (a - b).abs() < 1e-6));
        }
    }

    #[test]
    fn hostile_buffers_are_left_alone() {
        let on = ColorNoise { amount: 50.0, ..Default::default() };
        let f = NoiseFrame::prophoto();
        let mut v = vec![0.2f32; 30];
        assert_eq!(reduce_color_noise(&mut v, 3, 4, 4, &on, &f), None, "wrong length");
        assert_eq!(reduce_color_noise(&mut v, 3, 0, 10, &on, &f), None, "empty");
        assert_eq!(reduce_color_noise(&mut v, 2, 3, 5, &on, &f), None, "two channels");
        assert_eq!(reduce_color_noise(&mut v, 3, usize::MAX, 2, &on, &f), None, "overflow");
        let mut tiny = vec![0.2f32, 0.1, 0.3];
        assert_eq!(reduce_color_noise(&mut tiny, 3, 1, 1, &on, &f), None, "1 px");
        let mut nan = frame(16, 16, 0.05, 9, |_, _| [0.2; 3]);
        nan[5] = f32::NAN;
        let keep = nan.clone();
        assert_eq!(reduce_color_noise(&mut nan, 3, 16, 16, &on, &f), None, "NaN");
        assert_eq!(format!("{nan:?}"), format!("{keep:?}"));
        let mut z = vec![0.0f32; 16 * 16 * 3];
        reduce_color_noise(&mut z, 3, 16, 16, &on, &f);
        assert!(z.iter().all(|v| *v == 0.0), "black stays black");
        for weights in [[0.0; 3], [f32::NAN, 1.0, 0.0], [0.5, 0.0, 0.5]] {
            let mut v = frame(16, 16, 0.05, 10, |_, _| [0.2; 3]);
            assert_eq!(reduce_color_noise(&mut v, 3, 16, 16, &on, &NoiseFrame { weights, ..f }), None, "{weights:?}");
        }
        let odd = [ColorNoise { amount: 1e9, detail: -5.0, smoothness: f32::INFINITY }, ColorNoise { amount: 100.0, detail: f32::NAN, smoothness: -1.0 }];
        for p in odd {
            let mut v = frame(33, 17, 0.05, 11, |_, _| [0.2; 3]);
            reduce_color_noise(&mut v, 3, 33, 17, &p, &f);
            assert!(v.iter().all(|x| x.is_finite()));
        }
        for scale in [0.0, -1.0, f32::NAN, 1e-9, 7.0] {
            let mut v = frame(40, 40, 0.05, 12, |_, _| [0.2; 3]);
            reduce_color_noise(&mut v, 3, 40, 40, &on, &NoiseFrame { pixel_scale: scale, noise: Some(ChromaNoise { a: f32::NAN, b: 1.0 }), ..f });
            assert!(v.iter().all(|x| x.is_finite()), "scale {scale}");
        }
    }
}
