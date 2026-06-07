// SPDX-License-Identifier: GPL-3.0-or-later
//! Gestalt: does this buffer of pixels look like a *picture*?
//!
//! When brute-forcing decode parameters for a headless stream, "0 decoder
//! errors" is necessary but not sufficient — a wrong guess can decode
//! "cleanly" into confetti. The decisive test is the one a human does in a
//! tenth of a second: is there coherent structure in there? Borders, shapes,
//! things. This module is that glance, made numerical:
//!
//!  - **edge kurtosis** — natural images are mostly flat with sparse, strong
//!    edges: a heavy-tailed gradient distribution. Misdecoded garbage is
//!    texture everywhere: Gaussian-ish, kurtosis near 3.
//!  - **seam ratio** — a decoder hallucinating from misaligned entropy data
//!    paints hard discontinuities along the 16×16 macroblock grid. Real
//!    frames keep boundary gradients close to interior gradients.
//!  - **chroma sanity** — parameter mismatches produce saturated rainbow
//!    soup; real frames keep chroma concentrated near neutral.
//!  - **coherence** — consecutive frames of real video correlate strongly;
//!    garbage decorrelates. The hardest metric to fool.
//!
//! An optional neural second opinion (see `net`) re-ranks the top survivors.

/// One decoded frame, yuv420p.
pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Frame {
    /// Parse a raw yuv420p buffer; None if the size doesn't divide evenly.
    pub fn from_yuv420(data: &[u8], w: usize, h: usize) -> Option<Frame> {
        let ys = w * h;
        let cs = w.div_ceil(2) * h.div_ceil(2);
        if data.len() < ys + 2 * cs || w < 32 || h < 32 {
            return None;
        }
        Some(Frame {
            w,
            h,
            y: data[..ys].to_vec(),
            u: data[ys..ys + cs].to_vec(),
            v: data[ys + cs..ys + 2 * cs].to_vec(),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Gestalt {
    /// Excess-tailedness of the luma gradient distribution. Noise ≈ 3.
    pub edge_kurtosis: f64,
    /// Macroblock-boundary gradient energy over interior energy. Real ≈ 1.
    pub seam_ratio: f64,
    /// 0..1 — fraction of chroma that stays in plausible territory.
    pub chroma_sanity: f64,
    /// 0..1 — inter-frame correlation; None with a single frame.
    pub coherence: Option<f64>,
    /// Combined verdict, 0 (static on the wall) .. 1 (a picture of something).
    pub score: f64,
}

fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

/// Horizontal luma gradients, sampled on every row.
fn gradients(f: &Frame) -> Vec<f64> {
    let mut g = Vec::with_capacity(f.w * f.h);
    for row in 0..f.h {
        let base = row * f.w;
        for x in 1..f.w {
            g.push(f.y[base + x] as f64 - f.y[base + x - 1] as f64);
        }
    }
    g
}

fn kurtosis(g: &[f64]) -> Option<f64> {
    let n = g.len() as f64;
    if n < 16.0 {
        return None;
    }
    let mean = g.iter().sum::<f64>() / n;
    let m2 = g.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    if m2 < 1e-3 {
        return None; // flat frame: nothing to judge
    }
    let m4 = g.iter().map(|x| (x - mean).powi(4)).sum::<f64>() / n;
    Some(m4 / (m2 * m2))
}

/// Mean |∇| along 16-pixel grid boundaries vs everywhere else, both axes.
fn seam_ratio(f: &Frame) -> Option<f64> {
    let (mut seam, mut seam_n) = (0.0f64, 0u64);
    let (mut body, mut body_n) = (0.0f64, 0u64);
    for row in 0..f.h {
        let base = row * f.w;
        for x in 1..f.w {
            let d = (f.y[base + x] as f64 - f.y[base + x - 1] as f64).abs();
            if x % 16 == 0 {
                seam += d;
                seam_n += 1;
            } else {
                body += d;
                body_n += 1;
            }
        }
    }
    for row in 1..f.h {
        let (a, b) = ((row - 1) * f.w, row * f.w);
        for x in 0..f.w {
            let d = (f.y[b + x] as f64 - f.y[a + x] as f64).abs();
            if row % 16 == 0 {
                seam += d;
                seam_n += 1;
            } else {
                body += d;
                body_n += 1;
            }
        }
    }
    if seam_n == 0 || body_n == 0 {
        return None;
    }
    let (s, b) = (seam / seam_n as f64, body / body_n as f64);
    if b < 1e-3 {
        return None; // flat frame
    }
    Some(s / b)
}

/// Fraction of chroma samples within sane saturation. The threshold sits
/// beyond legitimate full-saturation primaries (vivid content is innocent);
/// only the corner-of-the-gamut confetti that misdecodes produce gets there.
fn chroma_sanity(f: &Frame) -> f64 {
    let n = f.u.len().min(f.v.len());
    if n == 0 {
        return 1.0; // grayscale is perfectly sane
    }
    let insane = (0..n)
        .filter(|&i| {
            let du = (f.u[i] as i32 - 128).abs();
            let dv = (f.v[i] as i32 - 128).abs();
            du + dv > 180
        })
        .count();
    1.0 - clamp01(insane as f64 / n as f64 * 8.0)
}

/// Inter-frame coherence: how small is the frame-to-frame difference
/// relative to the frame's own internal variation?
fn coherence(a: &Frame, b: &Frame) -> Option<f64> {
    if a.w != b.w || a.h != b.h || a.y.len() != b.y.len() {
        return None;
    }
    let n = a.y.len() as f64;
    let mean = a.y.iter().map(|&v| v as f64).sum::<f64>() / n;
    let spread = a.y.iter().map(|&v| (v as f64 - mean).abs()).sum::<f64>() / n;
    if spread < 1e-3 {
        return None; // flat frame
    }
    let diff =
        a.y.iter()
            .zip(&b.y)
            .map(|(&x, &y)| (x as f64 - y as f64).abs())
            .sum::<f64>()
            / n;
    Some(clamp01(1.0 - diff / (spread * 1.5)))
}

/// Score one or more consecutive decoded frames. None if every metric
/// abstained (flat/empty frames — nothing to judge either way).
pub fn score(frames: &[Frame]) -> Option<Gestalt> {
    let first = frames.first()?;
    let k = kurtosis(&gradients(first));
    let s = seam_ratio(first);
    let c = chroma_sanity(first);
    let coh = frames
        .windows(2)
        .filter_map(|w| coherence(&w[0], &w[1]))
        .fold(None::<(f64, u32)>, |acc, v| {
            Some(match acc {
                Some((sum, n)) => (sum + v, n + 1),
                None => (v, 1),
            })
        })
        .map(|(sum, n)| sum / n as f64);

    if k.is_none() && s.is_none() && coh.is_none() {
        return None;
    }

    // Fold each metric into 0..1, then combine with weights that favor the
    // hardest-to-fool signals. Abstaining metrics redistribute their weight.
    let mut total = 0.0;
    let mut weight = 0.0;
    if let Some(k) = k {
        total += 0.30 * clamp01((k - 3.0) / 17.0);
        weight += 0.30;
    }
    if let Some(s) = s {
        total += 0.25 * clamp01((3.0 - s) / 2.0);
        weight += 0.25;
    }
    total += 0.15 * c;
    weight += 0.15;
    if let Some(coh) = coh {
        total += 0.30 * coh;
        weight += 0.30;
    }

    Some(Gestalt {
        edge_kurtosis: k.unwrap_or(f64::NAN),
        seam_ratio: s.unwrap_or(f64::NAN),
        chroma_sanity: c,
        coherence: coh,
        score: total / weight,
    })
}

// ---------------------------------------------------------------------------
// Optional neural second opinion
// ---------------------------------------------------------------------------

/// A tiny image classifier as a tiebreaker: among candidates decoding the
/// *same* source bytes, the correctly-parameterized one is the most
/// recognizable. Score = the model's confidence that the frame is *anything*.
pub mod net {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};

    /// Where the model lives: --model, $BASINSKI_MODEL, or the cache dir.
    pub fn find_model(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = explicit {
            return p.exists().then(|| p.to_path_buf());
        }
        if let Ok(p) = std::env::var("BASINSKI_MODEL") {
            let p = PathBuf::from(p);
            if p.exists() {
                return Some(p);
            }
        }
        let cache = dirs_cache()?.join("basinski");
        for name in ["mobilenetv2.onnx", "mobilenetv2-7.onnx", "model.onnx"] {
            let p = cache.join(name);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    fn dirs_cache() -> Option<PathBuf> {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }

    pub struct Critic {
        model: tract_onnx::prelude::TypedRunnableModel<tract_onnx::prelude::TypedModel>,
    }

    impl Critic {
        pub fn load(path: &Path) -> Result<Critic> {
            use tract_onnx::prelude::*;
            let model = tract_onnx::onnx()
                .model_for_path(path)
                .with_context(|| format!("loading model {}", path.display()))?
                .with_input_fact(0, f32::fact([1, 3, 224, 224]).into())?
                .into_optimized()?
                .into_runnable()?;
            Ok(Critic { model })
        }

        /// `rgb` must be 224*224*3 interleaved bytes. Returns the model's top
        /// softmax probability — its confidence that this is a picture of
        /// anything at all.
        pub fn recognizability(&self, rgb: &[u8]) -> Result<f64> {
            use tract_onnx::prelude::*;
            anyhow::ensure!(rgb.len() >= 224 * 224 * 3, "need a 224x224 rgb frame");
            const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
            const STD: [f32; 3] = [0.229, 0.224, 0.225];
            let input = tract_ndarray::Array4::from_shape_fn((1, 3, 224, 224), |(_, c, y, x)| {
                let v = rgb[(y * 224 + x) * 3 + c] as f32 / 255.0;
                (v - MEAN[c]) / STD[c]
            });
            let out = self.model.run(tvec!(input.into_tensor().into()))?;
            let logits = out[0].to_array_view::<f32>()?;
            let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let denom: f32 = logits.iter().map(|&v| (v - max).exp()).sum();
            Ok((1.0 / denom) as f64) // softmax of the max logit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise.
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 24) as u8
        }
    }

    const W: usize = 128;
    const H: usize = 96;

    /// A "real" frame: smooth gradient background with sharp rectangles.
    fn natural() -> Frame {
        let mut y = vec![0u8; W * H];
        for row in 0..H {
            for x in 0..W {
                let mut v = (x / 2 + row / 3) as i32 + 40;
                // two crisp shapes with borders
                if (20..50).contains(&x) && (20..44).contains(&row) {
                    v = 200;
                }
                if (70..110).contains(&x) && (50..80).contains(&row) {
                    v = 16;
                }
                y[row * W + x] = v.clamp(0, 255) as u8;
            }
        }
        Frame {
            w: W,
            h: H,
            y,
            u: vec![128; W * H / 4],
            v: vec![128; W * H / 4],
        }
    }

    fn noise(seed: u64) -> Frame {
        let mut r = XorShift(seed | 1);
        Frame {
            w: W,
            h: H,
            y: (0..W * H).map(|_| r.next()).collect(),
            u: (0..W * H / 4).map(|_| r.next()).collect(),
            v: (0..W * H / 4).map(|_| r.next()).collect(),
        }
    }

    /// The natural frame with its 16x16 blocks shuffled — structure inside
    /// blocks, chaos at every seam. The misaligned-decode signature.
    fn scrambled() -> Frame {
        let src = natural();
        let mut y = vec![0u8; W * H];
        let (bw, bh) = (W / 16, H / 16);
        for by in 0..bh {
            for bx in 0..bw {
                // a fixed permutation of blocks
                let sx = (bx * 7 + by * 3 + 5) % bw;
                let sy = (by * 5 + bx + 2) % bh;
                for r in 0..16 {
                    for c in 0..16 {
                        y[(by * 16 + r) * W + bx * 16 + c] = src.y[(sy * 16 + r) * W + sx * 16 + c];
                    }
                }
            }
        }
        Frame {
            w: W,
            h: H,
            y,
            u: src.u.clone(),
            v: src.v.clone(),
        }
    }

    #[test]
    fn pictures_beat_noise() {
        let pic = score(&[natural()]).unwrap();
        let snow = score(&[noise(42)]).unwrap();
        assert!(
            pic.score > snow.score + 0.3,
            "picture {:.3} vs noise {:.3}",
            pic.score,
            snow.score
        );
        assert!(pic.edge_kurtosis > 10.0);
        assert!(snow.edge_kurtosis < 5.0);
    }

    #[test]
    fn scrambled_blocks_betray_themselves() {
        let pic = score(&[natural()]).unwrap();
        let scram = score(&[scrambled()]).unwrap();
        assert!(
            scram.seam_ratio > pic.seam_ratio * 2.0,
            "scrambled seam {:.2} vs natural {:.2}",
            scram.seam_ratio,
            pic.seam_ratio
        );
        assert!(scram.score < pic.score);
    }

    #[test]
    fn coherent_motion_beats_decorrelated_noise() {
        let a = natural();
        // "next frame": same scene, one pixel of drift
        let mut b = natural();
        b.y.rotate_right(1);
        let video = score(&[a, b]).unwrap();
        let static_score = video.coherence.unwrap();
        assert!(
            static_score > 0.8,
            "real motion coherence {static_score:.3}"
        );

        let n = score(&[noise(1), noise(2)]).unwrap();
        assert!(
            n.coherence.unwrap() < 0.3,
            "noise coherence {:?}",
            n.coherence
        );
    }

    #[test]
    fn insane_chroma_is_penalized() {
        let mut f = natural();
        // rainbow confetti chroma
        let mut r = XorShift(7);
        f.u = (0..W * H / 4).map(|_| r.next()).collect();
        f.v = (0..W * H / 4).map(|_| r.next()).collect();
        let g = score(&[f]).unwrap();
        assert!(
            g.chroma_sanity < 0.7,
            "chroma sanity {:.3}",
            g.chroma_sanity
        );
    }

    #[test]
    fn flat_frames_abstain() {
        let f = Frame {
            w: W,
            h: H,
            y: vec![16; W * H],
            u: vec![128; W * H / 4],
            v: vec![128; W * H / 4],
        };
        assert!(score(&[f]).is_none());
    }
}
