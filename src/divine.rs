// SPDX-License-Identifier: GPL-3.0-or-later
//! `divine` — dowsing for lost codec parameters.
//!
//! A headless H.264 stream with no parameter sets is undecodable: SPS and
//! PPS hold the field widths every slice header is parsed with, and they're
//! gone. But the space of plausible parameter sets is small and *testable*:
//! synthesize a candidate for each guess, decode the stream's own keyframe
//! under it, and ask the gestalt scorer whether what came out looks like a
//! picture or like static. The guess that produces borders and shapes is the
//! truth — the parameters fit the data like a dental record.
//!
//! The economics: the SPS depends only on geometry (proven byte-identical
//! across encoder settings at a fixed resolution), so one donor encode per
//! resolution supplies it. The PPS is synthesized from scratch — entropy
//! mode, reference counts, weighted prediction, init QP are all free knobs,
//! no re-encoding. Three stages, cheap to expensive:
//!
//!  1. geometry & entropy on the keyframe alone. CAVLC parses independent of
//!     init QP, so it sweeps fast; CABAC seeds its contexts from init QP and
//!     must search it jointly (26 outward — encoders cluster there).
//!  2. header semantics on a keyframe-plus-slices chain: SPS width grid ×
//!     reference counts × weighted prediction, with inter-frame coherence
//!     joining the vote.
//!  3. a neural second opinion, if a model is on hand, re-ranks survivors.
//!
//! The winner is written out as a ready-to-use `--reference` donor.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::ffx;
use crate::gestalt;
use crate::h264::{self, PpsParams};
use crate::mp4::be32;
use crate::transplant;

pub struct Options {
    /// Frame rate for the donor's timing tables (does not affect decoding).
    pub fps: u32,
    pub keep_temp: bool,
    pub model: Option<PathBuf>,
    pub output: Option<PathBuf>,
}

/// Resolutions in rough order of real-world likelihood.
const RESOLUTIONS: &[(u32, u32)] = &[
    (1920, 1080),
    (1280, 720),
    (640, 360),
    (854, 480),
    (3840, 2160),
    (2560, 1440),
    (960, 540),
    (720, 480),
    (640, 480),
    (720, 576),
    (426, 240),
    (320, 240),
];

/// SPS width guesses tried in stage 1: donor-native, the hardware-encoder
/// corner (small frame_num, wide POC), the x264 keyint family, and the
/// poc_type-2 layouts of B-frame-less streams (no POC field in headers).
/// Format: (log2_max_frame_num-4, Some(log2_max_poc_lsb-4) | None=poc_type 2).
type Widths = (u64, Option<u64>);
const STAGE1_WIDTHS: &[Option<Widths>] = &[
    None,
    Some((0, Some(4))),
    Some((1, Some(4))),
    Some((4, Some(5))),
    Some((2, Some(3))),
    Some((0, None)),
    Some((1, None)),
];

/// init_qp search order for CABAC: 26 outward. Encoders cluster near the
/// middle of the scale; the tails exist but rarely.
fn init_qp_order() -> Vec<i64> {
    let mut v = vec![26];
    for d in 1..=16 {
        v.push(26 - d);
        v.push(26 + d);
    }
    v.retain(|&q| (0..=51).contains(&q));
    v
}

#[derive(Clone)]
struct Candidate {
    res: (u32, u32),
    widths: Option<Widths>,
    pps: PpsParams,
    sps: Vec<u8>,
    base: PathBuf,
    errors: usize,
    gestalt: f64,
    nn: Option<f64>,
    /// frame_num counting consistency across the whole payload.
    consistency: Option<f64>,
}

impl Candidate {
    fn recipe(&self) -> String {
        let widths = match self.widths {
            Some((f, Some(p))) => format!("fn={} poc={}", f + 4, p + 4),
            Some((f, None)) => format!("fn={} poc_type=2", f + 4),
            None => "native widths".into(),
        };
        format!(
            "{}x{} {} init_qp={} ref={} wp={} 8x8={} {}",
            self.res.0,
            self.res.1,
            if self.pps.cabac { "cabac" } else { "cavlc" },
            self.pps.init_qp,
            self.pps.ref_l0,
            self.pps.weighted_pred as u8,
            self.pps.transform_8x8 as u8,
            widths
        )
    }
}

// ---------------------------------------------------------------------------
// Pulling testable NALs out of the wreck — no donor needed yet
// ---------------------------------------------------------------------------

/// At `i`: a length-prefixed slice NAL whose header is consistent and whose
/// first slice starts a picture. Returns (nal bytes incl. header, end, idr).
/// The deeper header checks (slice_type exists and matches IDR-ness, pps_id
/// is small) cost nothing and keep torn-head garbage from posing as
/// evidence — a fake keyframe here poisons every verdict downstream.
fn slice_nal_at(d: &[u8], i: usize, min: usize) -> Option<(&[u8], usize, bool)> {
    if i + 6 > d.len() {
        return None;
    }
    let len = be32(d, i) as usize;
    if len < min || len > 8 << 20 || i + 4 + len > d.len() {
        return None;
    }
    let nal = d[i + 4];
    let (ty, ref_idc) = (nal & 0x1F, nal >> 5);
    let idr = ty == 5;
    if nal & 0x80 != 0 || !(ty == 1 || idr) || (idr && ref_idc == 0) {
        return None;
    }
    let header_end = (i + 5 + 12).min(d.len());
    let rbsp = crate::h264::strip_emulation(&d[i + 5..header_end]);
    let mut b = crate::h264::Bits::new(&rbsp);
    if b.ue()? != 0 {
        return None; // first_mb_in_slice
    }
    let slice_type = b.ue()?;
    if slice_type > 9 || (idr && !matches!(slice_type % 5, 2 | 4)) {
        return None; // nonexistent, or an IDR claiming to be P/B
    }
    if b.ue()? > 3 {
        return None; // pps_id
    }
    Some((&d[i + 4..i + 4 + len], i + 4 + len, idr))
}

/// Greedy scan for one IDR slice and the slices that follow it.
fn extract_chain(d: &[u8]) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let mut i = 0;
    let (idr, pos) = loop {
        if i + 6 > d.len() {
            return None;
        }
        match slice_nal_at(d, i, 1000) {
            Some((nal, end, true)) => break (nal.to_vec(), end),
            _ => i += 1,
        }
    };
    let mut rest = Vec::new();
    let mut i = pos;
    while rest.len() < 6 && i + 6 <= d.len() {
        match slice_nal_at(d, i, 64) {
            Some((nal, end, _)) => {
                rest.push(nal.to_vec());
                i = end;
            }
            None => i += 1,
        }
    }
    Some((idr, rest))
}

/// Long-range counting check: parse frame_num from hundreds of slices at a
/// candidate width and score how often the count behaves — +0/+1 mod M in
/// decode order, reset to 0 at every IDR. A short evidence chain cannot tell
/// adjacent widths apart (frame 6 looks the same 4 bits wide or 5), but the
/// whole tape can: at the wrong width, the count goes mad mid-GOP.
fn frame_num_consistency(media: &[u8], fn_width: u32) -> f64 {
    let m = 1u64 << fn_width;
    let mut i = 0usize;
    let (mut ok, mut tot) = (0u32, 0u32);
    let mut prev: Option<u64> = None;
    let mut seen = 0;
    while i + 6 <= media.len() && seen < 400 {
        match slice_nal_at(media, i, 200) {
            Some((nal, end, idr)) => {
                seen += 1;
                i = end;
                let rbsp = crate::h264::strip_emulation(&nal[1..nal.len().min(13)]);
                let mut b = crate::h264::Bits::new(&rbsp);
                let fnum = (|| {
                    b.ue()?; // first_mb_in_slice
                    b.ue()?; // slice_type
                    b.ue()?; // pps_id
                    b.u(fn_width)
                })();
                let Some(f) = fnum else { continue };
                if idr {
                    tot += 1;
                    if f == 0 {
                        ok += 1;
                        prev = Some(0);
                    } else {
                        prev = None;
                    }
                } else if let Some(p) = prev {
                    tot += 1;
                    if f == p || f == (p + 1) % m {
                        ok += 1;
                    }
                    prev = Some(f);
                } else {
                    prev = Some(f);
                }
            }
            None => i += 1,
        }
    }
    if tot < 16 {
        return 0.5; // not enough testimony either way
    }
    ok as f64 / tot as f64
}

fn annexb(sps: &[u8], pps: &[u8], nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in [sps, pps].iter().copied().chain(nals.iter().copied()) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

// ---------------------------------------------------------------------------
// Judging candidates — in parallel, each an independent ffmpeg
// ---------------------------------------------------------------------------

/// Decode the candidate stream → (decoder error lines, gestalt score).
/// Flat-frame abstentions score a wary neutral 0.45.
fn judge_at(path: &Path, stream: &[u8], n_frames: usize) -> Result<(usize, f64)> {
    fs::write(path, stream)?;
    let (frames, errors) = ffx::decode_sample(path, n_frames, Some("h264"))?;
    if frames.is_empty() {
        return Ok((errors.max(1), 0.0));
    }
    let g = gestalt::score(&frames).map(|g| g.score).unwrap_or(0.45);
    Ok((errors, g))
}

/// A parameter guess awaiting judgment.
struct Trial {
    res: (u32, u32),
    widths: Option<Widths>,
    pps: PpsParams,
    sps: Vec<u8>,
    base: PathBuf,
}

/// Judge every trial against the evidence NALs, fanning out across workers.
fn judge_all(trials: Vec<Trial>, nals: &[&[u8]], tmp: &Path) -> Vec<Candidate> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let results = Mutex::new(Vec::with_capacity(trials.len()));
    let next = AtomicUsize::new(0);
    let n_frames = nals.len();

    std::thread::scope(|s| {
        for w in 0..workers {
            let results = &results;
            let next = &next;
            let trials = &trials;
            s.spawn(move || {
                let path = tmp.join(format!("cand{w}.264"));
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= trials.len() {
                        break;
                    }
                    let t = &trials[i];
                    let stream = annexb(&t.sps, &h264::build_pps(t.pps), nals);
                    if let Ok((errors, gestalt)) = judge_at(&path, &stream, n_frames) {
                        results.lock().unwrap().push(Candidate {
                            res: t.res,
                            widths: t.widths,
                            pps: t.pps,
                            sps: t.sps.clone(),
                            base: t.base.clone(),
                            errors,
                            gestalt,
                            nn: None,
                            consistency: None,
                        });
                    }
                }
            });
        }
    });

    let mut out = results.into_inner().unwrap();
    out.sort_by(|a, b| {
        a.errors
            .cmp(&b.errors)
            .then(b.gestalt.partial_cmp(&a.gestalt).unwrap())
    });
    out
}

/// One resolution's donor: the encode and its native SPS.
struct Geometry {
    res: (u32, u32),
    base: PathBuf,
    native_sps: Vec<u8>,
}

fn prepare_geometry(res: (u32, u32), fps: u32, tmp: &Path) -> Option<Geometry> {
    let base = tmp.join(format!("donor_{}x{}.mp4", res.0, res.1));
    ffx::synth_donor(&base, res.0, res.1, fps).ok()?;
    let bytes = fs::read(&base).ok()?;
    let (native_sps, _) = h264::extract_avcc_params(&bytes)?;
    Some(Geometry {
        res,
        base,
        native_sps,
    })
}

fn sps_variant(g: &Geometry, widths: Option<Widths>) -> Option<Vec<u8>> {
    match widths {
        None => Some(g.native_sps.clone()),
        Some((f, p)) => h264::patch_sps_widths(&g.native_sps, f, p),
    }
}

// ---------------------------------------------------------------------------
// The rod itself
// ---------------------------------------------------------------------------

pub fn divine(input: &Path, opts: &Options) -> Result<PathBuf> {
    let data = fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    println!("◌ {} — {} bytes", input.display(), data.len());

    if !matches!(crate::mp4::analyze(&data), crate::mp4::Analysis::NoMoov) {
        bail!(
            "this file still has its own index — plain `rescue` will treat it better than divination"
        );
    }

    let (ms, me, head_torn) = transplant::locate_media(&data);
    let media = &data[ms..me];
    let (idr, ps) = extract_chain(media)
        .context("no H.264 IDR grammar found in the payload — divine only speaks H.264 so far")?;
    println!(
        "  payload: {} bytes{}; testable keyframe of {} bytes + {} trailing slices",
        media.len(),
        if head_torn { " (unknown phase)" } else { "" },
        idr.len(),
        ps.len()
    );

    let tmp = std::env::temp_dir().join(format!("basinski-divine-{}", std::process::id()));
    fs::create_dir_all(&tmp)?;
    let cleanup = TempGuard {
        path: tmp.clone(),
        keep: opts.keep_temp,
    };

    // -----------------------------------------------------------------------
    // Stage 1: geometry & entropy, judged on the keyframe alone. Every
    // resolution gets its day in court — a foreign IDR can decode "cleanly"
    // at the wrong geometry (macroblocks reflowed into a sheared raster), so
    // nothing is decided here; stage 2's motion vectors are the tiebreaker.
    // -----------------------------------------------------------------------
    println!("\n  ☉ stage 1: dowsing for geometry, entropy, and the CABAC seed");
    let geometries: Vec<Geometry> = RESOLUTIONS
        .iter()
        .filter_map(|&res| prepare_geometry(res, opts.fps, &tmp))
        .collect();
    if geometries.is_empty() {
        bail!("no candidate even encoded — is libx264 available?");
    }

    let inits = init_qp_order();
    let mut trials: Vec<Trial> = Vec::new();
    for geo in &geometries {
        for &widths in STAGE1_WIDTHS {
            let Some(sps) = sps_variant(geo, widths) else {
                continue;
            };
            // CAVLC: init_qp doesn't gate parsing — one shot per widths.
            trials.push(Trial {
                res: geo.res,
                widths,
                pps: PpsParams {
                    cabac: false,
                    ref_l0: 2,
                    weighted_pred: false,
                    init_qp: 26,
                    chroma_qp_offset: 0,
                    transform_8x8: false,
                },
                sps: sps.clone(),
                base: geo.base.clone(),
            });
            // CABAC: the seed must be right; sweep 26 outward, both 8x8 modes.
            for &t8 in &[true, false] {
                for &q in &inits {
                    trials.push(Trial {
                        res: geo.res,
                        widths,
                        pps: PpsParams {
                            cabac: true,
                            ref_l0: 2,
                            weighted_pred: false,
                            init_qp: q,
                            chroma_qp_offset: 0,
                            transform_8x8: t8,
                        },
                        sps: sps.clone(),
                        base: geo.base.clone(),
                    });
                }
            }
        }
    }
    println!("    {} candidate parameter sets on trial", trials.len());
    let stage1 = judge_all(trials, &[&idr], &tmp);
    let best1 = stage1
        .first()
        .context("every stage-1 candidate failed to even run")?;
    println!(
        "    rod twitches at: {}  ({} errors, gestalt {:.2})",
        best1.recipe(),
        best1.errors,
        best1.gestalt
    );

    // -----------------------------------------------------------------------
    // Stage 1.5: the long count. frame_num counting across the whole payload
    // pins its field width before any further decoding — pure parsing, and
    // immune to the bit-budget aliases that fool short evidence (fn=5+poc=6
    // reads the same header bits as fn=4+poc=7; only the tape-long counting
    // pattern tells them apart).
    // -----------------------------------------------------------------------
    let mut fn_scores: Vec<(u64, f64)> = (0..=5u64)
        .map(|m4| (m4, frame_num_consistency(media, m4 as u32 + 4)))
        .collect();
    fn_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let fn_widths: Vec<u64> = fn_scores
        .iter()
        .take_while(|&&(_, s)| s >= fn_scores[0].1 - 0.1)
        .map(|&(m4, _)| m4)
        .take(2)
        .collect();
    println!(
        "  ☉ the long count: frame_num width {} ({:.0}% consistent over the whole tape)",
        fn_scores[0].0 + 4,
        fn_scores[0].1 * 100.0
    );

    // -----------------------------------------------------------------------
    // Stage 2: header semantics on the keyframe + trailing slices. Distinct
    // geometries from stage 1 face the P slices, whose motion vectors and
    // reference lists do not forgive a wrong guess. With the fn width pinned,
    // the grid is small enough to sweep the CABAC seed jointly — a keyframe
    // can be degenerately tolerant of a seed the slice chain will not forgive.
    // -----------------------------------------------------------------------
    println!(
        "  ☉ stage 2: header semantics ({} trailing slices in the vote)",
        ps.len()
    );
    let mut nals: Vec<&[u8]> = vec![&idr];
    nals.extend(ps.iter().map(|v| v.as_slice()));
    let n_frames = nals.len();

    // Finalists: the best entry of each of the top distinct geometries.
    let mut finalists: Vec<&Candidate> = Vec::new();
    for c in &stage1 {
        if c.errors > 0 && finalists.first().is_some_and(|f| f.errors == 0) {
            break; // clean candidates exist; dirty geometries need not apply
        }
        if !finalists.iter().any(|f| f.res == c.res) {
            finalists.push(c);
        }
        if finalists.len() >= 3 {
            break;
        }
    }

    // Phase a: POC layout × init seed, jointly, at the pinned fn widths.
    let mut trials: Vec<Trial> = Vec::new();
    for f in &finalists {
        let Some(geo) = geometries.iter().find(|g| g.res == f.res) else {
            continue;
        };
        let mut width_grid: Vec<Option<Widths>> = Vec::new();
        for &fn_m4 in &fn_widths {
            width_grid.push(Some((fn_m4, None))); // poc_type 2
            for poc_m4 in 0..=6u64 {
                width_grid.push(Some((fn_m4, Some(poc_m4))));
            }
        }
        for widths in width_grid {
            let Some(sps) = sps_variant(geo, widths) else {
                continue;
            };
            let init_grid: Vec<i64> = if f.pps.cabac {
                init_qp_order()
            } else {
                vec![f.pps.init_qp]
            };
            for &init_qp in &init_grid {
                trials.push(Trial {
                    res: f.res,
                    widths,
                    pps: PpsParams { init_qp, ..f.pps },
                    sps: sps.clone(),
                    base: geo.base.clone(),
                });
            }
        }
    }
    let widths_pass = judge_all(trials, &nals, &tmp);

    // Phase b: refine the PPS on the best few survivors.
    let mut trials: Vec<Trial> = Vec::new();
    for c in widths_pass.iter().take(4) {
        for ref_l0 in [1u32, 2, 3, 4] {
            for weighted_pred in [false, true] {
                trials.push(Trial {
                    res: c.res,
                    widths: c.widths,
                    pps: PpsParams {
                        ref_l0,
                        weighted_pred,
                        ..c.pps
                    },
                    sps: c.sps.clone(),
                    base: c.base.clone(),
                });
            }
        }
    }
    let mut stage2 = judge_all(trials, &nals, &tmp);
    stage2.extend(widths_pass);
    if stage2.is_empty() {
        bail!("stage 2 produced no candidates — donor encoding failed across the board");
    }
    stage2.sort_by(|a, b| {
        a.errors
            .cmp(&b.errors)
            .then(b.gestalt.partial_cmp(&a.gestalt).unwrap())
    });

    // -----------------------------------------------------------------------
    // Stage 3: the neural second opinion, if a model is around.
    // -----------------------------------------------------------------------
    let critic = gestalt::net::find_model(opts.model.as_deref()).and_then(|p| {
        match gestalt::net::Critic::load(&p) {
            Ok(c) => {
                println!("  ☉ stage 3: second opinion from {}", p.display());
                Some(c)
            }
            Err(e) => {
                println!(
                    "  (model at {} failed to load: {e:#} — classical verdict stands)",
                    p.display()
                );
                None
            }
        }
    });
    match &critic {
        Some(critic) => {
            for c in stage2.iter_mut().take(8) {
                let stream = annexb(&c.sps, &h264::build_pps(c.pps), &[&idr]);
                let path = tmp.join("nn.264");
                fs::write(&path, &stream)?;
                if let Ok(Some(rgb)) = ffx::decode_rgb224(&path, Some("h264")) {
                    c.nn = critic.recognizability(&rgb).ok();
                }
            }
            stage2.sort_by(|a, b| {
                let key = |c: &Candidate| {
                    (
                        c.errors,
                        -(0.7 * c.gestalt + 0.3 * c.nn.unwrap_or(c.gestalt)),
                    )
                };
                key(a).partial_cmp(&key(b)).unwrap()
            });
        }
        None => {
            println!(
                "  (no model file — classical gestalt only; drop mobilenetv2.onnx in \
                 ~/.cache/basinski/ or pass --model for the second opinion)"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Stage 4: the long count. frame_num across the whole payload arbitrates
    // width degeneracies that a short evidence chain tolerates — including
    // bit-budget aliases (fn=5+poc=6 parses the same header bits as
    // fn=4+poc=7; only the tape-long counting pattern tells them apart).
    // Every candidate tied near the best error count gets judged; the check
    // is pure parsing, cached per width.
    // -----------------------------------------------------------------------
    let best_errors = stage2.first().map(|c| c.errors).unwrap_or(0);
    let mut width_scores: std::collections::HashMap<u32, f64> = std::collections::HashMap::new();
    for c in stage2.iter_mut() {
        if c.errors > best_errors + 2 {
            continue;
        }
        if let Some(info) = h264::parse_sps(&c.sps) {
            let score = *width_scores
                .entry(info.log2_max_frame_num)
                .or_insert_with(|| frame_num_consistency(media, info.log2_max_frame_num));
            c.consistency = Some(score);
        }
    }
    stage2.sort_by(|a, b| {
        let key = |c: &Candidate| {
            (
                c.errors,
                -(c.consistency.unwrap_or(0.5)),
                -(0.7 * c.gestalt + 0.3 * c.nn.unwrap_or(c.gestalt)),
            )
        };
        key(a).partial_cmp(&key(b)).unwrap()
    });

    println!("\n    errors  gestalt  nn      count   recipe");
    let mut shown: Vec<String> = Vec::new();
    for c in stage2.iter() {
        let r = c.recipe();
        if shown.contains(&r) {
            continue;
        }
        println!(
            "    {:>6}  {:>7.3}  {}  {}  {}",
            c.errors,
            c.gestalt,
            c.nn.map(|v| format!("{v:>6.3}"))
                .unwrap_or_else(|| "     —".into()),
            c.consistency
                .map(|v| format!("{v:>5.2}"))
                .unwrap_or_else(|| "    —".into()),
            r.clone()
        );
        shown.push(r);
        if shown.len() >= 5 {
            break;
        }
    }

    let winner = &stage2[0];
    if winner.errors > 0 && winner.gestalt < 0.4 {
        bail!(
            "no candidate produced a picture — the parameters live outside this grid\n\
             (interlaced field coding and multi-PPS streams are not divined yet; an\n\
             intact sibling file from the device would settle it)"
        );
    }

    // Full gestalt diagnostics for the winner — the glance, itemized.
    let wstream = annexb(&winner.sps, &h264::build_pps(winner.pps), &nals);
    let wpath = tmp.join("winner.264");
    fs::write(&wpath, &wstream)?;
    let (wframes, _) = ffx::decode_sample(&wpath, n_frames, Some("h264"))?;
    let detail = gestalt::score(&wframes);

    // Manufacture the donor: the winning base encode with the winning organs.
    let donor_out = opts.output.clone().unwrap_or_else(|| {
        let stem = input.file_stem().unwrap_or_default().to_string_lossy();
        input.with_file_name(format!("{stem}.donor.mp4"))
    });
    let base_bytes = fs::read(&winner.base)?;
    let final_bytes =
        h264::replace_avcc_params(&base_bytes, &winner.sps, &h264::build_pps(winner.pps))
            .context("failed to graft the divined parameter sets into the donor")?;
    fs::write(&donor_out, &final_bytes)
        .with_context(|| format!("writing {}", donor_out.display()))?;

    drop(cleanup);
    println!(
        "\n  ☽ divined: {}  ({} errors, gestalt {:.2})",
        winner.recipe(),
        winner.errors,
        winner.gestalt
    );
    if let Some(g) = detail {
        let fmt = |v: f64, p: usize| {
            if v.is_nan() {
                "—".into()
            } else {
                format!("{v:.p$}", p = p)
            }
        };
        println!(
            "    edges {} (noise≈3), seams {} (real≈1), chroma {:.2}, coherence {}",
            fmt(g.edge_kurtosis, 1),
            fmt(g.seam_ratio, 2),
            g.chroma_sanity,
            g.coherence
                .map(|c| format!("{c:.2}"))
                .unwrap_or_else(|| "—".into())
        );
    }
    println!(
        "    (timing assumed {} fps — pass --fps if you know better)",
        opts.fps
    );
    println!("  ✔ donor written → {}", donor_out.display());
    println!(
        "\n  next: basinski rescue {} --reference {}",
        input.display(),
        donor_out.display()
    );
    Ok(donor_out)
}

struct TempGuard {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_extraction_finds_idr_then_slices() {
        // [garbage][IDR 1200B][P 300B][P 280B] — slice headers must survive
        // the deep sanity check: first_mb=0, valid slice_type, pps_id=0.
        let mut d = vec![0xAAu8; 37];
        let nal = |ty: u8, hdr: [u8; 2], len: u32| {
            let mut v = (len - 4).to_be_bytes().to_vec();
            v.push(ty);
            v.extend_from_slice(&hdr);
            v.resize(len as usize, 0x55);
            v
        };
        // IDR: first_mb=0, slice_type=7 (I), pps_id=0 → bits 1 0001000 1...
        d.extend(nal(0x65, [0x88, 0x80], 1200));
        // P: first_mb=0, slice_type=5 (P), pps_id=0 → bits 1 00110 1...
        d.extend(nal(0x41, [0x9A, 0x00], 300));
        d.extend(nal(0x41, [0x9A, 0x00], 280));
        let (idr, ps) = extract_chain(&d).unwrap();
        assert_eq!(idr.len(), 1196);
        assert_eq!(idr[0], 0x65);
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].len(), 296);
    }

    #[test]
    fn fake_idr_with_garbage_header_is_rejected() {
        // Plausible length + IDR NAL byte + MSB payload, but the slice
        // header claims slice_type B for an IDR — the torn-head signature.
        let mut d = vec![0u8; 8];
        let mut v = 2000u32.to_be_bytes().to_vec();
        v.push(0x65);
        v.extend_from_slice(&[0xA8, 0x55]); // first_mb=0, slice_type=1 (B)
        v.resize(2004, 0x55);
        d.extend(v);
        assert!(extract_chain(&d).is_none());
    }

    #[test]
    fn no_idr_no_chain() {
        let d = vec![0x11u8; 4096];
        assert!(extract_chain(&d).is_none());
    }
}
