// SPDX-License-Identifier: GPL-3.0-or-later
//! The rescue pipeline: diagnose, reconstruct, validate, clip.
//!
//! Two philosophies, used where each applies:
//!  - **surgical** (head_truncated MP4s): the surviving moov index tells us exactly
//!    what was lost and where the first clean keyframe is. No guessing.
//!  - **empirical** (self-synchronizing streams, artifacted files): trim to
//!    the first verifiable sync structure and let the decoder vote.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::ffx;
use crate::forensics::{self, Finding};
use crate::matroska;
use crate::mkv_codecs::Codec;
use crate::mp4::{self, Analysis};
use crate::transplant;

pub struct Options {
    pub correct_format: bool,
    /// Keep damaged leading frames instead of clipping to a clean keyframe.
    pub no_clip: bool,
    /// Keep intermediate reconstruction files for inspection.
    pub keep_temp: bool,
    /// An intact sibling file from the same device — the organ donor for a
    /// moov transplant when the casualty's index is gone entirely.
    pub reference: Option<PathBuf>,
    /// Override the video frame rate. A paramset-less stream carries no
    /// timing of its own, so the donor's rate is a guess; this lets you set
    /// the truth (common: 23.976, 24, 25, 29.97, 30) without re-divining.
    pub fps: Option<f64>,
    /// Attempt to salvage raw AAC audio from the interleave gaps.
    pub audio: bool,
    /// Sample rate to assume for salvaged AAC (default 44100).
    pub audio_rate: u32,
}

/// A frame rate as (timescale, per-sample delta), keeping NTSC fractions
/// exact: 23.976 → (24000, 1001), not a rounded 23976/1000.
fn fps_to_ticks(fps: f64) -> (u32, u32) {
    for (num, den) in [(24000, 1001), (30000, 1001), (60000, 1001)] {
        if (fps - num as f64 / den as f64).abs() < 0.01 {
            return (num, den);
        }
    }
    ((fps * 1000.0).round() as u32, 1000)
}

pub fn rescue(input: &Path, output: Option<PathBuf>, opts: &Options) -> Result<()> {
    let data = fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    println!("◌ {} — {} bytes", input.display(), data.len());

    let findings = forensics::identify(&data);
    report_findings(&findings);

    match mp4::analyze(&data) {
        Analysis::HeadTruncated(b) => {
            if opts.reference.is_some() {
                println!("  (reference not needed — this file's own index survived)");
            }
            let out = output.unwrap_or_else(|| default_output(input, "rescued", "mp4"));
            rescue_head_truncated_mp4(input, &out, &data, &b, opts)
        }
        Analysis::Intact => {
            if opts.reference.is_some() {
                println!("  (reference not needed — this file's own index survived)");
            }
            let out = output.unwrap_or_else(|| default_output(input, "rescued", "mp4"));
            rescue_intact(input, &out, opts)
        }
        Analysis::NoMoov => match matroska::analyze(&data) {
            matroska::Analysis::Heavy(h) => rescue_matroska(input, output, &data, h, opts),
            // Intact Matroska / no structure: let the headerless path probe it
            // (ffprobe reads intact Matroska; truly unknown data falls through).
            _ => rescue_headerless_stream(input, output, &data, &findings, opts),
        },
    }
}

fn report_findings(findings: &[Finding]) {
    if findings.is_empty() {
        println!("  forensics: no recognizable media structure found");
        return;
    }
    for f in findings.iter().take(3) {
        println!(
            "  forensics: {:>3.0}%  {}  [{}]",
            f.confidence * 100.0,
            f.kind,
            f.evidence
        );
    }
}

fn default_output(input: &Path, tag: &str, ext: &str) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{stem}.{tag}.{ext}"))
}

// ---------------------------------------------------------------------------
// Surgical: head_truncated MP4 with surviving index
// ---------------------------------------------------------------------------

fn rescue_head_truncated_mp4(
    input: &Path,
    output: &Path,
    data: &[u8],
    b: &mp4::HeadTruncation,
    opts: &Options,
) -> Result<()> {
    println!("\n  diagnosis: head-truncated MP4");
    println!(
        "    bytes cut from front : {}  (determined by {})",
        b.cut_bytes, b.method
    );
    println!("    media data destroyed : {} bytes", b.media_bytes_lost);
    for t in &b.tracks {
        println!(
            "    track: {} ({}) — {} samples @ timescale {}",
            t.handler, t.codec, t.sample_count, t.timescale
        );
    }
    if b.media_bytes_lost > 0 {
        println!(
            "    keyframes destroyed  : {} (first clean keyframe at {:.3}s)",
            b.damaged_keyframes,
            b.first_clean_keyframe_time.unwrap_or(f64::NAN)
        );
    }

    let rebuilt = mp4::reconstruct(data, b)?;
    let temp = output.with_extension("reconstructed.mp4");
    fs::write(&temp, &rebuilt).with_context(|| format!("writing {}", temp.display()))?;
    println!(
        "\n  ☼ regrew {}-byte prefix (ftyp + free silence) → {}",
        b.cut_bytes,
        temp.display()
    );

    let probe = ffx::probe(&temp)?
        .context("reconstruction produced a file ffprobe cannot read — index may be lying")?;
    println!("  container restored: {}", probe.summary());

    if b.media_bytes_lost == 0 || opts.no_clip {
        ffx::remux(&temp, output)?;
        if b.media_bytes_lost > 0 {
            println!("  (--no-clip: damaged leading frames kept, expect artifacts)");
        }
    } else {
        // The index already told us *when* the file becomes clean. Find the
        // first real keyframe packet at or after that moment. (ffprobe quietly
        // strips the K flag from keyframes whose data we zero-filled, so the
        // packet list can't be matched by ordinal — only by time.)
        let target = b
            .first_clean_keyframe_time
            .context("no intact keyframe survives — nothing clippable")?;
        let kfs = ffx::keyframes(&temp)?;
        let t = kfs
            .iter()
            .copied()
            .find(|&k| k >= target - 1e-3)
            .unwrap_or(target);
        println!("  ✂ clipping to first clean keyframe at {t:.3}s");
        ffx::clip_from_keyframe(&temp, output, t)?;
    }

    if !opts.keep_temp {
        let _ = fs::remove_file(&temp);
    }

    finish(input, output, opts)
}

// ---------------------------------------------------------------------------
// Reconstructive: heavily-beheaded Matroska/WebM (only clusters survive)
// ---------------------------------------------------------------------------

fn rescue_matroska(
    input: &Path,
    output: Option<PathBuf>,
    data: &[u8],
    h: matroska::Heavy,
    opts: &Options,
) -> Result<()> {
    println!("\n  diagnosis: beheaded Matroska/WebM — only clusters survived");
    println!(
        "    bytes lost before first clean cluster : {} (~head + first cluster)",
        h.bytes_lost
    );
    for t in &h.tracks {
        println!("    track {}: {:?}", t.number, t.codec);
    }

    if opts.reference.is_some() {
        println!(
            "  (note: --reference is not used for Matroska/WebM yet; donor-based transplant is planned)"
        );
    }

    // Honest failure: a track whose parameters lived in the lost header and
    // cannot be synthesized from frames. Transplant (donor) arrives in Plan 2.
    for t in &h.tracks {
        if let Codec::NeedsDonor { hint } = t.codec {
            bail!(
                "track {} ({hint}) keeps its parameters in the lost header, which \
                 basinski can't synthesize from the frames alone. This version \
                 re-heads VP9 video + Opus audio only; donor-based Matroska \
                 transplant for other codecs is planned but not yet available.",
                t.number
            );
        }
    }

    let webm = matroska::all_webm_legal(&h.tracks);
    let ext = if webm { "webm" } else { "mkv" };
    let out = output.unwrap_or_else(|| default_output(input, "rescued", ext));

    // Choose the playback start at the container level: the first cluster led by
    // a true keyframe gives clean playback from frame one (no ffmpeg clip).
    // --no-clip keeps everything from the first surviving cluster instead.
    let start = if opts.no_clip {
        h.first_cluster
    } else {
        h.first_keyframe_cluster.unwrap_or(h.first_cluster)
    };
    if opts.no_clip {
        println!(
            "  (--no-clip: starting at first surviving cluster; leading mid-GOP frames may artifact)"
        );
    } else if h.first_keyframe_cluster.is_none() {
        println!(
            "  ⚠ no keyframe-led cluster found in scan window — starting mid-GOP, expect leading artifacts"
        );
    } else if start > h.first_cluster {
        println!("  ✂ starting at first keyframe-led cluster (offset {start})");
    }

    let rebuilt = matroska::reconstruct(data, &h, start)?;
    let head_len = rebuilt.len() - (data.len() - start);
    let temp = out.with_extension(format!("rehead.{ext}"));
    fs::write(&temp, &rebuilt).with_context(|| format!("writing {}", temp.display()))?;
    println!(
        "\n  ☼ synthesized {head_len}-byte head (EBML + Segment + Info + Tracks) → {}",
        temp.display()
    );

    let probe = ffx::probe(&temp)?
        .context("reconstruction produced a file ffprobe cannot read — head may be wrong")?;
    println!("  container restored: {}", probe.summary());

    // Stream-copy remux: ffmpeg's own muxer rewrites the blocks cleanly and
    // launders the benign VP9 parser chatter, yielding a pristine container.
    ffx::remux_copy(&temp, &out)?;
    if !opts.keep_temp {
        let _ = fs::remove_file(&temp);
    }

    // Validate against the decoder (the basinski contract). A clean keyframe-led
    // start should decode cleanly; errors here mean the reconstruction is wrong
    // — most often a misidentified codec (Plan 1 only re-heads VP9 + Opus) or
    // clusters too damaged to use. Don't hand back a confidently-wrong file.
    let errs = ffx::decode_errors(&out).unwrap_or(0);
    if errs == 0 {
        println!("  ✓ decodes clean (0 errors)");
    } else if opts.no_clip {
        println!("  ⚠ {errs} decode error line(s) — expected with --no-clip (mid-GOP start)");
    } else {
        let _ = fs::remove_file(&out);
        bail!(
            "reconstructed file does not decode cleanly ({errs} error line(s)). \
             The codec identification is likely wrong (this version re-heads VP9 \
             video + Opus audio only) or the surviving clusters are too damaged. \
             Re-run with --no-clip to write the file anyway for inspection."
        );
    }

    finish(input, &out, opts)
}

// ---------------------------------------------------------------------------
// Intact container — maybe artifacted, maybe just fine
// ---------------------------------------------------------------------------

fn rescue_intact(input: &Path, output: &Path, opts: &Options) -> Result<()> {
    println!("\n  diagnosis: container structure intact");
    let errors = ffx::decode_errors(input)?;
    if errors == 0 {
        println!("  decodes cleanly — nothing to rescue");
        if opts.correct_format {
            return finish(input, output, opts);
        }
        println!("  (pass --the-correct-format if you want it converted anyway)");
        return Ok(());
    }
    println!("  decode reported {errors} error line(s) — searching for first clean keyframe");
    let t = first_clean_keyframe(input, output)?;
    println!("  ✂ clipping from keyframe at {t:.3}s");
    ffx::clip_from_keyframe(input, output, t)?;
    finish(input, output, opts)
}

/// Empirically find the earliest keyframe from which the file decodes clean.
pub fn first_clean_keyframe(input: &Path, output: &Path) -> Result<f64> {
    let kfs = ffx::keyframes(input)?;
    if kfs.is_empty() {
        bail!("no keyframes found at all");
    }
    let temp = output.with_extension("probe.mp4");
    let mut chosen = None;
    for (i, &t) in kfs.iter().enumerate().take(32) {
        if ffx::clip_from_keyframe(input, &temp, t).is_err() {
            continue;
        }
        let errs = ffx::decode_errors(&temp).unwrap_or(usize::MAX);
        if errs == 0 {
            chosen = Some(t);
            println!("    keyframe {i} @ {t:.3}s decodes clean");
            break;
        }
        println!("    keyframe {i} @ {t:.3}s still artifacted ({errs} errors)");
    }
    let _ = fs::remove_file(&temp);
    chosen.context("no keyframe in the first 32 yields a clean decode")
}

// ---------------------------------------------------------------------------
// Empirical: headerless self-synchronizing streams (MP3, ADTS, MPEG-TS, H.264)
// ---------------------------------------------------------------------------

fn rescue_headerless_stream(
    input: &Path,
    output: Option<PathBuf>,
    data: &[u8],
    findings: &[Finding],
    opts: &Options,
) -> Result<()> {
    // Maybe the file is simply fine and not an MP4 at all.
    if let Some(probe) = ffx::probe(input)?
        && ffx::decode_errors(input)? == 0
    {
        println!(
            "\n  diagnosis: not an MP4, but healthy as-is ({})",
            probe.summary()
        );
        if opts.correct_format {
            let video = probe.has_video();
            let ext = if video { "mp4" } else { "mp3" };
            let out = output.unwrap_or_else(|| {
                let plain = input.with_extension(ext);
                if plain == input {
                    default_output(input, "correct", ext)
                } else {
                    plain
                }
            });
            return finish(input, &out, opts);
        }
        println!("  nothing to rescue");
        return Ok(());
    }

    // The user brought a donor: they know this is an MP4 body. Transplant.
    if let Some(ref_path) = opts.reference.clone() {
        if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
            bail!(
                "this is a Matroska/WebM file; the MP4 moov transplant doesn't \
                 apply here, and donor-based Matroska transplant is not yet \
                 implemented."
            );
        }
        return rescue_with_donor(input, output, data, &ref_path, opts);
    }

    let Some(best) = findings.first().filter(|f| f.confidence >= 0.5) else {
        bail!(
            "no usable structure found: no MP4 index, no recognizable stream sync.\n\
             If this is the body of an MP4 (mdat only), pass --reference <an intact\n\
             file from the same device> and basinski will attempt a moov transplant."
        );
    };

    let (trim_ext, container_ext) = match best.kind.as_str() {
        k if k.starts_with("MP3 audio") => ("mp3", "mp3"),
        k if k.starts_with("AAC audio in ADTS") => ("aac", "aac"),
        k if k.starts_with("MPEG transport stream") => ("ts", "mp4"),
        k if k.starts_with("H.264 elementary stream") => ("h264", "mp4"),
        k if k.starts_with("H.264 in MP4 framing") => bail!(
            "this is the body of an MP4 — length-prefixed H.264 with no index.\n\
             Pass --reference <an intact file from the same device> and basinski\n\
             will attempt a moov transplant."
        ),
        k => bail!("identified as `{k}` but basinski does not know how to re-head that format yet"),
    };

    println!(
        "\n  diagnosis: self-synchronizing stream with damaged head\n  ✂ trimming {} bytes of dead air to first sync at offset {}",
        best.offset, best.offset
    );
    let out = output.unwrap_or_else(|| default_output(input, "rescued", container_ext));
    let temp = out.with_extension(format!("trimmed.{trim_ext}"));
    fs::write(&temp, &data[best.offset as usize..])?;

    // Remux through ffmpeg to rebuild clean timestamps and a real container.
    ffx::remux(&temp, &out)?;
    if !opts.keep_temp {
        let _ = fs::remove_file(&temp);
    }
    finish(input, &out, opts)
}

// ---------------------------------------------------------------------------
// Surgical, with a donor: the moov transplant (what untrunc does)
// ---------------------------------------------------------------------------

fn rescue_with_donor(
    input: &Path,
    output: Option<PathBuf>,
    data: &[u8],
    ref_path: &Path,
    opts: &Options,
) -> Result<()> {
    let ref_data =
        fs::read(ref_path).with_context(|| format!("reading reference {}", ref_path.display()))?;
    let mut donor = transplant::extract_donor(&ref_data)
        .with_context(|| format!("{} won't work as a donor", ref_path.display()))?;

    // Re-time to the requested rate. The donor only supplied a guess; the
    // user knows whether it was overcranked.
    if let Some(fps) = opts.fps
        && let Some(v) = donor.video.as_mut()
    {
        let (ts, delta) = fps_to_ticks(fps);
        v.timescale = ts;
        v.sample_delta = delta;
        println!("  ⏲ retiming video to {fps} fps");
    }

    println!("\n  diagnosis: no index — attempting a moov transplant");
    println!(
        "    organ donor : {} ({})",
        ref_path.display(),
        donor.summary()
    );

    let audio_rate = opts.audio.then_some(opts.audio_rate);
    let t = transplant::transplant_opts(data, &donor, opts.no_clip, audio_rate)?;
    println!(
        "\n    video samples recovered : {} ({} keyframes)",
        t.video_samples, t.sync_samples
    );
    if donor.video.as_ref().is_some_and(|v| v.has_ctts) {
        if t.ctts_recovered {
            println!("    B-frame timing          : recovered from slice POCs (ctts regrown)");
        } else {
            println!(
                "    (donor reorders B-frames but their timing resisted recovery — \
                 expect slight judder, not damage)"
            );
        }
    }
    if t.dropped_leading > 0 {
        println!(
            "    dropped pre-keyframe    : {} samples (undecodable without their IDR)",
            t.dropped_leading
        );
    }
    if t.audio_samples > 0 {
        println!("    audio samples recovered : {}", t.audio_samples);
    }
    if t.aac_chunks > 0 {
        println!(
            "    audio recovered         : {} AAC chunks from the interleave gaps @ {} Hz{}",
            t.aac_chunks,
            opts.audio_rate,
            if t.aac_skipped > 0 {
                format!(" ({} non-audio gaps skipped)", t.aac_skipped)
            } else {
                String::new()
            }
        );
    }
    if t.audio_bytes_dropped > 0 {
        println!(
            "    audio dropped           : {} bytes (no recognizable AAC framing)",
            t.audio_bytes_dropped
        );
    }
    if t.torn_bytes > 0 {
        println!("    torn beyond recovery    : {} bytes", t.torn_bytes);
    }
    println!("    recovered duration      : {:.3}s", t.duration_s);

    let out = output.unwrap_or_else(|| default_output(input, "rescued", "mp4"));
    let temp = out.with_extension("transplanted.mp4");
    fs::write(&temp, &t.out).with_context(|| format!("writing {}", temp.display()))?;
    println!("\n  ☼ transplanted moov → {}", temp.display());

    let probe = ffx::probe(&temp)?
        .context("the transplant produced a file ffprobe cannot read — wrong donor?")?;
    println!("  container rebuilt: {}", probe.summary());

    // Mux the salvaged audio back in, if any survived the gaps.
    let mut muxed = false;
    if let Some(adts) = &t.audio_adts {
        let sidecar = out.with_extension("audio.aac");
        fs::write(&sidecar, adts).with_context(|| format!("writing {}", sidecar.display()))?;
        match ffx::mux_audio(&temp, &sidecar, &out)? {
            Some(adur) => {
                muxed = true;
                // The recovered audio is an independent clock: its true
                // duration over the video frame count reveals the frame rate.
                if adur > 1.0 {
                    let implied = t.video_samples as f64 / adur;
                    println!(
                        "    ♪ audio plays {adur:.1}s → at {} video frames that implies ~{implied:.2} fps",
                        t.video_samples
                    );
                    if opts.fps.is_none() {
                        println!(
                            "      (audio is the surer clock — re-run with --fps {:.0} if playback drifts)",
                            implied.round()
                        );
                    }
                }
            }
            None => println!("  (audio mux failed — keeping the silent video)"),
        }
        if !opts.keep_temp {
            let _ = fs::remove_file(&sidecar);
        }
    }
    if !muxed {
        ffx::remux(&temp, &out)?;
    }
    if !opts.keep_temp {
        let _ = fs::remove_file(&temp);
    }
    finish(input, &out, opts)
}

// ---------------------------------------------------------------------------
// Final validation + optional Correct Formatting
// ---------------------------------------------------------------------------

fn finish(original_input: &Path, output: &Path, opts: &Options) -> Result<()> {
    let mut final_path = output.to_path_buf();

    if opts.correct_format {
        // If a rescued intermediate exists at `output`, convert it and tag the
        // result; otherwise convert the input straight to `output`.
        let rescued_exists = output.exists();
        let src = if rescued_exists {
            output.to_path_buf()
        } else {
            original_input.to_path_buf()
        };
        let probe = ffx::probe(&src)?.context("nothing probeable to convert")?;
        let video = probe.has_video();
        let correct = if rescued_exists {
            output.with_extension(if video { "correct.mp4" } else { "correct.mp3" })
        } else {
            output.to_path_buf()
        };
        println!(
            "  ♻ converting to The Correct Format ({})",
            if video { "mp4: H.264 + AAC" } else { "mp3" }
        );
        ffx::to_correct_format(&src, &correct, video)?;
        if rescued_exists && !opts.keep_temp && src != correct {
            let _ = fs::remove_file(&src);
        }
        final_path = correct;
    }

    let probe = ffx::probe(&final_path)?
        .with_context(|| format!("{} is not probeable", final_path.display()))?;
    let errors = ffx::decode_errors(&final_path)?;
    println!("\n  ✔ rescued → {}", final_path.display());
    println!("    {}", probe.summary());
    if errors == 0 {
        println!("    full decode: clean (0 errors)");
    } else {
        println!("    full decode: {errors} error line(s) remain — partial rescue");
    }
    Ok(())
}
