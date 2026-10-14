//! Shadows and Highlights as a local Laplacian filter.
//!
//! Adobe has said Camera Raw's process version 2012 rests on local Laplacian filters (S. Paris,
//! S. W. Hasinoff, J. Kautz, *Local Laplacian Filters: Edge-aware Image Processing with a
//! Laplacian Pyramid*, SIGGRAPH 2011). This follows that paper, in its fast form (M. Aubry et
//! al., *Fast Local Laplacian Filters*, ACM TOG 2014: the remapping sampled at a few reference
//! values, each pyramid coefficient interpolated between them), on the log2 luminance:
//!
//! - **Shadows** raises pixels darker than their surroundings, by an amount set by their own and
//!   their surroundings' place in the image's range (1st to 99th percentile); **Highlights**
//!   pulls pixels brighter than their surroundings toward them. Differences under a threshold
//!   are left alone, so fine detail stays.
//! - Shadows then keeps the image's 99th percentile, Highlights its 1st (Photoshop keeps the
//!   brightest content under Shadows and the darkest under Highlights).
//! - A uniform image is unchanged, edges keep no halo, and the result does not depend on the
//!   image size.
//!
//! The strengths were fitted to Photoshop 25.4's Camera Raw Filter on ten public-domain photos
//! (black-box measurements, no Adobe code or data). It is an approximation: on those photos the
//! mean error against Photoshop is 2.8–9.4 levels (of 255) where the previous heuristic's was
//! 3.7–14.3.

use super::{Encoding, luma};
use crate::photo_util::par_rows;

/// Spacing of the fast filter's reference values in stops. Its remapping changes within about
/// half a stop, and coefficients interpolated across wider gaps show as contour lines on smooth
/// gradients (12 references over a whole raw's range left visible steps; at an eighth of a stop a
/// ramp's slope varies by under a third, as the remapping itself makes it).
const REF_STEP: f32 = 0.125;
/// The filter runs on a copy at most this many pixels across; the change it makes is brought back
/// to full size by a guided upsampling, so a preview and the full image agree.
const WORK_SIDE: usize = 1600;
/// Knots of the strength curves over the surroundings' place in the image range (0 … 1).
const KNOTS: usize = 5;

/// Shadows: how far a pixel darker than its surroundings rises, in stops per unit slider, as
/// a function of its own place in the image range (`own`) times a factor of its surroundings'
/// place (`around`); it rises fully once it is `sigma` stops below them. The lift depends on
/// the pixel's tone, not on the size of the difference, so it is the same at every pyramid
/// level and a big edge keeps no halo. Lowering (negative values) is `down` times as strong.
const SHADOWS: Lift = Lift { own: [-0.017, 0.466, 0.886, 1.221, -0.081], around: [4.566, 4.067, 1.088, 0.149, 0.878], sigma: 0.571, down: 0.808 };
const HIGHLIGHTS: Side = Side { pull: [0.179, 0.141, 0.196, 0.629, 2.07], push: [0.156, 0.08, 0.175, 0.049, 1.261], sigma: 0.05 };

/// See [`SHADOWS`].
struct Lift {
    own: [f32; KNOTS],
    around: [f32; KNOTS],
    sigma: f32,
    down: f32,
}

/// Highlights: strengths per unit slider over the surroundings' place in the image range, for
/// lowering (`pull`, toward the surroundings) and raising (`push`), and the detail threshold in
/// stops.
struct Side {
    pull: [f32; KNOTS],
    push: [f32; KNOTS],
    sigma: f32,
}

/// One channel, `w × h`.
#[derive(Clone)]
struct Plane {
    w: usize,
    h: usize,
    v: Vec<f32>,
}

/// Mirrors `i` into `0..n` (the edge sample is not repeated).
fn mirror(i: isize, n: usize) -> usize {
    if i >= 0 && (i as usize) < n {
        return i as usize;
    }
    let n = n as isize;
    if n == 1 {
        return 0;
    }
    let p = 2 * (n - 1);
    let m = i.rem_euclid(p);
    (if m >= n { p - m } else { m }) as usize
}

const TAPS: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];

/// The binomial 5-tap blur, sampled every `step` pixels from 0 (1 = full resolution).
fn blur_step(p: &Plane, step: usize) -> Plane {
    let (w2, h2) = (p.w.div_ceil(step), p.h.div_ceil(step));
    // Columns first at the kept rows, then rows at the kept columns.
    let mut tmp = vec![0.0f32; p.w * h2];
    par_rows(&mut tmp, p.w, 1, |y2, row| {
        let y = (y2 * step) as isize;
        for (x, o) in row.iter_mut().enumerate() {
            *o = (0..5).map(|k| TAPS[k] * p.v[mirror(y + k as isize - 2, p.h) * p.w + x]).sum();
        }
    });
    let mut out = vec![0.0f32; w2 * h2];
    par_rows(&mut out, w2, 1, |y2, row| {
        let src = &tmp[y2 * p.w..(y2 + 1) * p.w];
        for (x2, o) in row.iter_mut().enumerate() {
            let x = (x2 * step) as isize;
            *o = (0..5).map(|k| TAPS[k] * src[mirror(x + k as isize - 2, p.w)]).sum();
        }
    });
    Plane { w: w2, h: h2, v: out }
}

/// [`up`]`(p, w, h)` at one pixel (`x`, `y`): the same taps over the same zero-filled samples,
/// added in the same order, so the value is bit for bit [`up`]'s.
fn up_at(p: &Plane, w: usize, h: usize, x: usize, y: usize) -> f32 {
    let (kh, kw) = (p.h.min(h.div_ceil(2)), p.w.min(w.div_ceil(2)));
    let z = |yy: usize, xx: usize| {
        if yy.is_multiple_of(2) && xx.is_multiple_of(2) && yy / 2 < kh && xx / 2 < kw { p.v[(yy / 2) * p.w + xx / 2] * 4.0 } else { 0.0 }
    };
    let column = |xx: usize| -> f32 { (0..5).map(|k| TAPS[k] * z(mirror(y as isize + k as isize - 2, h), xx)).sum() };
    (0..5).map(|k| TAPS[k] * column(mirror(x as isize + k as isize - 2, w))).sum()
}

/// The next coarser level.
fn down(p: &Plane) -> Plane {
    blur_step(p, 2)
}

/// `p` brought up to `w × h`: zeros between samples, then the blur times four.
fn up(p: &Plane, w: usize, h: usize) -> Plane {
    let mut z = Plane { w, h, v: vec![0.0; w * h] };
    for y in 0..p.h.min(h.div_ceil(2)) {
        for x in 0..p.w.min(w.div_ceil(2)) {
            z.v[2 * y * w + 2 * x] = p.v[y * p.w + x] * 4.0;
        }
    }
    blur_step(&z, 1)
}

/// Levels down to about 2 pixels on the short side.
fn depth(w: usize, h: usize) -> usize {
    let mut s = w.min(h);
    let mut n = 1;
    while s > 2 {
        s = s.div_ceil(2);
        n += 1;
    }
    n
}

fn gaussian(p: Plane, n: usize) -> Vec<Plane> {
    let mut g = vec![p];
    for _ in 1..n {
        let next = down(&g[g.len() - 1]);
        g.push(next);
    }
    g
}

/// The local Laplacian filter of `u` for remappings `x + amount · change(x, reference)`, split
/// in two: `(base, detail)` with the filter's output `base + amount · detail` for any amount (the
/// pyramids, the Laplacian levels and the reference weights are all linear in the remapped
/// values, and the weights sum to 1). `base` is the identity's output (`u` rebuilt from its
/// pyramid); `detail`, the change's, whose coarsest level is 0 (the coarsest level is kept).
///
/// A coefficient takes only the two (rarely three) references within a step of it, so each
/// reference's Laplacian is computed only there, its coarse level brought up pixel by pixel
/// ([`up_at`]); the references run in parallel and their shares are added in reference order,
/// so the result is the same bits as computing every reference's whole Laplacian in turn.
fn llf_parts(u: &Plane, change: impl Fn(f32, f32) -> f32 + Sync) -> (Plane, Plane) {
    let n = depth(u.w, u.h);
    let g = gaussian(u.clone(), n);
    // References span the image's tones; the deepest 0.1 % (noise down to the floor) share the
    // lowest reference instead of stretching the spacing.
    let hi = u.v.iter().fold(f32::NEG_INFINITY, |a, &v| a.max(v));
    let lo = quantile(&u.v, 0.001).min(hi);
    if hi - lo <= 1e-6 || (hi - lo).is_nan() {
        return (u.clone(), Plane { w: u.w, h: u.h, v: vec![0.0; u.v.len()] });
    }
    let refs = (((hi - lo) / REF_STEP).ceil() as usize + 1).clamp(2, 96);
    let step = (hi - lo) / (refs - 1) as f32;
    let weight = |gl: f32, k: usize| (1.0 - (gl.clamp(lo, hi) - (lo + step * k as f32)).abs() / step).max(0.0);
    // A coefficient takes only the references within a step of it (two, rarely three). For each
    // level, each reference's coefficients: (index, slot, weight), the slot being the
    // coefficient's count of earlier references, so they add up in reference order as before.
    const SLOTS: usize = 3;
    let mut wanted: Vec<Vec<Vec<(u32, u8, f32)>>> = vec![vec![Vec::new(); n - 1]; refs];
    let mut slots: Vec<Vec<u8>> = Vec::with_capacity(n - 1);
    for (l, gl) in g[..n - 1].iter().enumerate() {
        let mut count = vec![0u8; gl.v.len()];
        for (i, &v) in gl.v.iter().enumerate() {
            let k0 = ((v.clamp(lo, hi) - lo) / step).floor() as isize;
            for k in (k0 - 1).max(0)..=(k0 + 2).min(refs as isize - 1) {
                let k = k as usize;
                let wgt = weight(v, k);
                if wgt > 0.0 && (count[i] as usize) < SLOTS {
                    wanted[k][l].push((i as u32, count[i], wgt));
                    count[i] += 1;
                }
            }
        }
        slots.push(count);
    }
    // Each reference's pyramid on its own (in parallel); its Laplacian coefficients only where
    // they are wanted, the coarse level brought up at those pixels alone.
    let parts: Vec<Vec<Vec<f32>>> = crate::photo_util::par_map(refs, |k| {
        let r = lo + step * k as f32;
        let mut m = u.clone();
        par_rows(&mut m.v, u.w, 1, |_, row| {
            for v in row.iter_mut() {
                *v = change(*v, r);
            }
        });
        let gr = gaussian(m, n);
        (0..n - 1)
            .map(|l| {
                wanted[k][l]
                    .iter()
                    .map(|&(i, _, wgt)| {
                        let (lw, lh) = (gr[l].w, gr[l].h);
                        let i = i as usize;
                        wgt * (gr[l].v[i] - up_at(&gr[l + 1], lw, lh, i % lw, i / lw))
                    })
                    .collect()
            })
            .collect()
    });
    let mut lap: Vec<Vec<f32>> = Vec::with_capacity(n - 1);
    for l in 0..n - 1 {
        let len = g[l].v.len();
        let mut by_slot = vec![0.0f32; len * SLOTS];
        for k in 0..refs {
            for (&(i, slot, _), v) in wanted[k][l].iter().zip(&parts[k][l]) {
                by_slot[i as usize * SLOTS + slot as usize] = *v;
            }
        }
        lap.push(
            (0..len)
                .map(|i| {
                    let mut o = 0.0f32;
                    for s in 0..slots[l][i] as usize {
                        o += by_slot[i * SLOTS + s];
                    }
                    o
                })
                .collect(),
        );
    }
    let rebuild = |coarsest: Plane, levels: &dyn Fn(usize) -> Vec<f32>| {
        let mut o = coarsest;
        for l in (0..n - 1).rev() {
            let mut next = up(&o, g[l].w, g[l].h);
            for (a, b) in next.v.iter_mut().zip(levels(l)) {
                *a += b;
            }
            o = next;
        }
        o
    };
    let top = &g[n - 1];
    let base = rebuild(top.clone(), &|l| {
        let coarse = up(&g[l + 1], g[l].w, g[l].h);
        g[l].v.iter().zip(&coarse.v).map(|(a, b)| a - b).collect()
    });
    let detail = rebuild(Plane { w: top.w, h: top.h, v: vec![0.0; top.v.len()] }, &|l| lap[l].clone());
    (base, detail)
}

/// `base + amount · detail`.
fn combine(base: &Plane, detail: &Plane, amount: f32) -> Plane {
    Plane { w: base.w, h: base.h, v: base.v.iter().zip(&detail.v).map(|(b, d)| b + amount * d).collect() }
}

/// The `q` quantile (0 … 1) of `v`.
fn quantile(v: &[f32], q: f32) -> f32 {
    let mut s = v.to_vec();
    let k = ((s.len() - 1) as f32 * q).round() as usize;
    *s.select_nth_unstable_by(k, f32::total_cmp).1
}

/// 1st and 99th percentiles of the log luminance on a copy at most 256 pixels across.
fn range(u: &Plane) -> (f32, f32) {
    let f = u.w.max(u.h).div_ceil(256).max(1);
    let (sw, sh) = (u.w.div_ceil(f), u.h.div_ceil(f));
    let mut sum = vec![(0.0f64, 0u32); sw * sh];
    for y in 0..u.h {
        for x in 0..u.w {
            let c = &mut sum[(y / f) * sw + x / f];
            c.0 += f64::from(u.v[y * u.w + x]);
            c.1 += 1;
        }
    }
    let cells: Vec<f32> = sum.iter().filter(|c| c.1 > 0).map(|c| (c.0 / f64::from(c.1)) as f32).collect();
    (quantile(&cells, 0.01), quantile(&cells, 0.99))
}

/// `d` past the detail threshold: about 0 below `sigma`, `d − sigma` well above it.
fn excess(d: f32, sigma: f32) -> f32 {
    d - sigma * (d / sigma).tanh()
}

fn knot(t: f32, k: &[f32; KNOTS]) -> f32 {
    let x = t.clamp(0.0, 1.0) * (KNOTS - 1) as f32;
    let i = (x as usize).min(KNOTS - 2);
    k[i] + (k[i + 1] - k[i]) * (x - i as f32)
}

/// The Highlights pass's parts on the log luminance `u` for lowering (`up` false) or raising:
/// pixels brighter than their surroundings move toward them (or away) by the part of the
/// difference past `sigma`, per unit of the slider (see [`llf_parts`]).
fn highlights_parts(u: &Plane, up: bool, side: &Side) -> (Plane, Plane) {
    let (lo, hi) = range(u);
    let span = (hi - lo).max(1.0);
    let strengths = if up { &side.push } else { &side.pull };
    llf_parts(u, |x, g| knot((g - lo) / span, strengths) * excess((x - g).max(0.0), side.sigma))
}

/// The Shadows pass's parts on the log luminance `u`: pixels darker than their surroundings
/// rise per unit of the slider (lowering is [`SHADOWS`]`.down` times as strong).
fn shadows_parts(u: &Plane) -> (Plane, Plane) {
    let (lo, hi) = range(u);
    let span = (hi - lo).max(1.0);
    llf_parts(u, |x, g| {
        let below = ((g - x) / SHADOWS.sigma).clamp(0.0, 1.0);
        if below == 0.0 {
            return 0.0;
        }
        let w = below * below * (3.0 - 2.0 * below);
        knot((x - lo) / span, &SHADOWS.own) * knot((g - lo) / span, &SHADOWS.around) * w
    })
}

/// A pass's output from its parts: `amount` (−1 … 1) of the change, then the content at the `q`
/// quantile of the input put back where it was.
fn pass(input: &Plane, parts: &(Plane, Plane), amount: f32, q: f32) -> Plane {
    let o = combine(&parts.0, &parts.1, amount);
    let shift = quantile(&input.v, q) - quantile(&o.v, q);
    Plane { w: o.w, h: o.h, v: o.v.iter().map(|v| v + shift).collect() }
}

/// What [`apply_with`] keeps for the pixels it last saw: the log luminance and its work copy, the
/// Highlights pass's parts for lowering and raising, and the Shadows pass's parts with the
/// Highlights amount its input had. Dragging one slider then costs a weighted sum, not a filter.
#[derive(Default)]
pub(crate) struct Kept {
    print: u64,
    planes: Option<(Plane, Plane, usize)>,
    highlights: [Option<(Plane, Plane)>; 2],
    shadows: Option<(f32, Plane, (Plane, Plane))>,
}

/// Applies Shadows and Highlights (−100 … 100) to straight RGBA pixels encoded with `enc`.
#[cfg(test)]
pub(crate) fn apply(px: &mut [[f32; 4]], w: usize, h: usize, shadows: f32, highlights: f32, enc: Encoding) {
    apply_with(px, w, h, shadows, highlights, enc, &mut Kept::default());
}

/// [`apply`], keeping in `kept` what does not depend on the sliders' values for these pixels.
pub(crate) fn apply_with(px: &mut [[f32; 4]], w: usize, h: usize, shadows: f32, highlights: f32, enc: Encoding, kept: &mut Kept) {
    if (shadows == 0.0 && highlights == 0.0) || w < 2 || h < 2 || px.len() < w * h {
        return;
    }
    let px = &mut px[..w * h];
    let print = fingerprint(px, w, enc);
    if kept.print != print || kept.planes.is_none() {
        const FLOOR: f32 = 1.0 / 65536.0;
        let src: &[[f32; 4]] = px;
        let u = Plane { w, h, v: crate::photo_util::par_map(w * h, |i| (luma([0, 1, 2].map(|c| enc.decode(src[i][c]))).max(0.0) + FLOOR).log2()) };
        // The filter on a copy at most WORK_SIDE across (box-averaged in log).
        let f = w.max(h).div_ceil(WORK_SIDE).max(1);
        let small = if f == 1 { u.clone() } else { shrink(&u, f) };
        *kept = Kept { print, planes: Some((u, small, f)), ..Kept::default() };
    }
    let Some((u, small, f)) = &kept.planes else { return };
    let mut o = small.clone();
    let hl = highlights.clamp(-100.0, 100.0) / 100.0;
    if hl != 0.0 {
        let side = usize::from(hl > 0.0);
        let parts = kept.highlights[side].get_or_insert_with(|| highlights_parts(small, hl > 0.0, &HIGHLIGHTS));
        // Keep the darkest content.
        o = pass(small, parts, hl, 0.01);
    }
    let sh = shadows.clamp(-100.0, 100.0) / 100.0;
    if sh != 0.0 {
        if kept.shadows.as_ref().is_none_or(|(k, _, _)| *k != hl) {
            let parts = shadows_parts(&o);
            kept.shadows = Some((hl, o.clone(), parts));
        }
        if let Some((_, input, parts)) = &kept.shadows {
            let k = if sh > 0.0 { sh } else { sh * SHADOWS.down };
            // Keep the brightest content.
            o = pass(input, parts, k, 0.99);
        }
    }
    // The change in log luminance, at full size.
    let change: Vec<f32> = if *f == 1 {
        crate::photo_util::par_map(w * h, |i| o.v[i] - u.v[i])
    } else {
        let d: Vec<f32> = o.v.iter().zip(&small.v).map(|(a, b)| a - b).collect();
        guided_upsample(small, &d, u)
    };
    par_rows(px, w, 1, |y, row| {
        for (x, q) in row.iter_mut().enumerate() {
            let gain = change[y * w + x].exp2();
            for c in 0..3 {
                q[c] = enc.encode(enc.decode(q[c]) * gain);
            }
        }
    });
}

/// A cheap check that the pixels (and their encoding) are the ones [`Kept`] was filled from.
fn fingerprint(px: &[[f32; 4]], w: usize, enc: Encoding) -> u64 {
    let step = (px.len() / 251).max(1);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (px.len() as u64) ^ ((w as u64) << 32) ^ u64::from(enc.gamma().to_bits());
    for p in px.iter().step_by(step) {
        for v in &p[..3] {
            h = (h ^ u64::from(v.to_bits())).wrapping_mul(0x0100_0000_01b3);
        }
    }
    h.max(1)
}

/// `u` averaged over `f × f` blocks.
fn shrink(u: &Plane, f: usize) -> Plane {
    let (sw, sh) = (u.w.div_ceil(f), u.h.div_ceil(f));
    let mut v = vec![0.0f32; sw * sh];
    par_rows(&mut v, sw, 1, |y, row| {
        for (x, o) in row.iter_mut().enumerate() {
            let (x0, y0) = (x * f, y * f);
            let (x1, y1) = ((x0 + f).min(u.w), (y0 + f).min(u.h));
            let mut s = 0.0f64;
            for yy in y0..y1 {
                for xx in x0..x1 {
                    s += f64::from(u.v[yy * u.w + xx]);
                }
            }
            *o = (s / ((x1 - x0) * (y1 - y0)) as f64) as f32;
        }
    });
    Plane { w: sw, h: sh, v }
}

/// The change `d` computed on `small` brought to the size of `full`, following its edges: a local
/// linear model of the change against the log luminance (the guided filter of K. He, J. Sun,
/// *Fast Guided Filter*, 2015), fitted on the small copy and evaluated on the full one.
fn guided_upsample(small: &Plane, d: &[f32], full: &Plane) -> Vec<f32> {
    let (sw, sh) = (small.w, small.h);
    let r = 2;
    let eps = 0.01;
    let mean = |v: &[f32]| super::box_mean(v, sw, sh, r);
    let ud: Vec<f32> = small.v.iter().zip(d).map(|(a, b)| a * b).collect();
    let uu: Vec<f32> = small.v.iter().map(|a| a * a).collect();
    let (mu, md, mud, muu) = (mean(&small.v), mean(d), mean(&ud), mean(&uu));
    let a: Vec<f32> = (0..sw * sh).map(|i| (mud[i] - mu[i] * md[i]) / (muu[i] - mu[i] * mu[i] + eps)).collect();
    let b: Vec<f32> = (0..sw * sh).map(|i| md[i] - a[i] * mu[i]).collect();
    let (ma, mb) = (mean(&a), mean(&b));
    let (fx, fy) = (sw as f32 / full.w as f32, sh as f32 / full.h as f32);
    let mut out = vec![0.0f32; full.w * full.h];
    par_rows(&mut out, full.w, 1, |y, row| {
        let sy = (y as f32 + 0.5) * fy - 0.5;
        for (x, o) in row.iter_mut().enumerate() {
            let sx = (x as f32 + 0.5) * fx - 0.5;
            let av = crate::photo_util::bilinear(&ma, sw, sh, 1, 0, sx, sy);
            let bv = crate::photo_util::bilinear(&mb, sw, sh, 1, 0, sx, sy);
            *o = av * full.v[y * full.w + x] + bv;
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRGB: Encoding = Encoding::SRGB;

    fn grey(w: usize, h: usize, f: impl Fn(usize, usize) -> f32) -> Vec<[f32; 4]> {
        (0..w * h).map(|i| f(i % w, i / w)).map(|v| [v, v, v, 1.0]).collect()
    }

    /// The coarse level brought up at one pixel is bit for bit what the whole upsampling gives,
    /// edges and odd sizes included.
    #[test]
    fn up_at_one_pixel_is_up() {
        for (w, h) in [(9usize, 7usize), (8, 8), (13, 2), (2, 11), (31, 17)] {
            let coarse =
                Plane { w: w.div_ceil(2), h: h.div_ceil(2), v: (0..w.div_ceil(2) * h.div_ceil(2)).map(|i| ((i * 37 % 101) as f32 * 0.173).sin()).collect() };
            let full = up(&coarse, w, h);
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(up_at(&coarse, w, h, x, y).to_bits(), full.v[y * w + x].to_bits(), "{w}x{h} at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn a_flat_image_is_unchanged() {
        let mut px = grey(64, 48, |_, _| 0.4);
        apply(&mut px, 64, 48, 100.0, -100.0, SRGB);
        assert!(px.iter().all(|q| (q[0] - 0.4).abs() < 1e-5));
    }

    #[test]
    fn shadows_raise_a_dark_area_without_a_halo() {
        // A 0.1 | 0.6 step (the dark half is the image's black point, which Photoshop barely
        // moves either): the bright half stays, and no halo forms on either side of the edge.
        let (w, h) = (256, 128);
        let mut px = grey(w, h, |x, _| if x < w / 2 { 0.1 } else { 0.6 });
        apply(&mut px, w, h, 100.0, 0.0, SRGB);
        let row = &px[64 * w..65 * w];
        assert!((row[w / 2 + 1][0] - 0.6).abs() < 0.002 && (row[w - 5][0] - 0.6).abs() < 0.002);
        let (far, near) = (row[10][0], row[w / 2 - 1][0]);
        assert!((far - 0.1).abs() < 0.004 && (far - near).abs() < 0.004, "{far} {near}");
    }

    #[test]
    fn grey_moves_as_in_photoshop() {
        // Mid-grey with 1 % black (0.015) and white (0.97) corners, Photoshop 25.4: Shadows +100
        // takes the grey from 127.5 to 165.3, Highlights −100 to 89.6 (of 255).
        let n = 256;
        let image = |c: f32| {
            grey(n, n, |x, y| {
                if x >= n - 26 && y >= n - 26 {
                    0.97
                } else if x >= n - 26 && y < 26 {
                    0.015
                } else {
                    c
                }
            })
        };
        for (s, hl, want) in [(100.0, 0.0, 165.3), (0.0, -100.0, 89.6)] {
            let mut px = image(0.5);
            apply(&mut px, n, n, s, hl, SRGB);
            let got = px[175 * n + 100][0] * 255.0;
            assert!((got - want).abs() < 5.0, "{s} {hl}: {got} vs {want}");
        }
    }

    #[test]
    fn highlights_lower_a_bright_area_and_keep_the_darkest() {
        let (w, h) = (192, 192);
        let mut px = grey(w, h, |x, y| if (64..128).contains(&x) && (64..128).contains(&y) { 0.8 } else { 0.2 });
        apply(&mut px, w, h, 0.0, -100.0, SRGB);
        assert!(px[96 * w + 96][0] < 0.75, "{}", px[96 * w + 96][0]);
        assert!((px[5 * w + 5][0] - 0.2).abs() < 0.01, "{}", px[5 * w + 5][0]);
    }

    #[test]
    fn smooth_gradients_stay_smooth() {
        // A soft ramp over eight stops, as in an out-of-focus background. Coefficients interpolated
        // between references too far apart drew contour lines across such ramps (Shadows +100 on
        // a D4 raw): the output must rise steadily, without ripples.
        let (w, h) = (640, 48);
        let mut px = grey(w, h, |x, _| crate::photo_util::linear_to_srgb((x as f32 / (w - 1) as f32 * 8.0 - 9.0).exp2()));
        for (s, hl) in [(100.0, 0.0), (0.0, -100.0), (-100.0, 100.0)] {
            let mut p = px.clone();
            apply(&mut p, w, h, s, hl, SRGB);
            let row: Vec<f32> = (0..w).map(|x| crate::photo_util::srgb_to_linear(p[24 * w + x][0]).max(1e-6).log2()).collect();
            let steps: Vec<f32> = row.windows(2).map(|d| d[1] - d[0]).collect();
            assert!(steps.iter().all(|d| *d > 0.0), "{s} {hl}: the ramp reverses");
            let jumps: Vec<f32> = steps.windows(2).map(|d| (d[1] - d[0]).abs()).collect();
            let ripple = jumps[16..jumps.len() - 16].iter().fold(0.0f32, |a, b| a.max(*b));
            // The ramp rises 0.0125 stops per pixel; its slope may change by a third at most.
            assert!(ripple < 0.004, "{s} {hl}: slope jumps by {ripple} stops per pixel");
        }
        px.clear();
    }

    #[test]
    fn the_result_does_not_depend_on_the_image_size() {
        let at = |n: usize| {
            let mut px = grey(n, n, |x, y| if x < n / 3 || y < n / 4 { 0.08 } else { 0.7 });
            apply(&mut px, n, n, 70.0, -60.0, SRGB);
            [px[(n / 8) * n + n / 8][0], px[(3 * n / 4) * n + 3 * n / 4][0]]
        };
        let (a, b) = (at(96), at(384));
        assert!((a[0] - b[0]).abs() < 0.02 && (a[1] - b[1]).abs() < 0.02, "{a:?} {b:?}");
    }

    /// Dragging either slider through the kept parts gives exactly what a fresh call gives, and
    /// other pixels are not mistaken for the kept ones.
    #[test]
    fn kept_parts_give_the_fresh_result() {
        let (w, h) = (120, 90);
        let px = grey(w, h, |x, y| 0.05 + 0.8 * (x as f32 / w as f32) * if (x / 9 + y / 7) % 3 == 0 { 0.4 } else { 1.0 });
        let mut kept = Kept::default();
        for (s, hl) in [(40.0, 0.0), (70.0, 0.0), (70.0, -30.0), (70.0, -60.0), (-50.0, -60.0), (0.0, 45.0), (20.0, 45.0)] {
            let (mut a, mut b) = (px.clone(), px.clone());
            apply_with(&mut a, w, h, s, hl, SRGB, &mut kept);
            apply(&mut b, w, h, s, hl, SRGB);
            assert_eq!(a, b, "{s} {hl}");
        }
        let mut other: Vec<[f32; 4]> = px.iter().map(|q| [q[0] * 0.5, q[1] * 0.5, q[2] * 0.5, 1.0]).collect();
        let mut fresh = other.clone();
        apply_with(&mut other, w, h, 40.0, 0.0, SRGB, &mut kept);
        apply(&mut fresh, w, h, 40.0, 0.0, SRGB);
        assert_eq!(other, fresh);
    }
}
