// SPDX-License-Identifier: GPL-3.0-or-later
//! Thin wrappers around ffprobe/ffmpeg, the two external dependencies this
//! tool admits to having. Everything else is done by hand, by candlelight.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

fn require(tool: &str) -> Result<()> {
    if Command::new(tool).arg("-version").output().is_err() {
        bail!("`{tool}` not found on PATH — basinski needs ffmpeg/ffprobe for this operation");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ffprobe
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Probe {
    #[serde(default)]
    pub streams: Vec<Stream>,
    pub format: Option<Format>,
}

#[derive(Debug, Deserialize)]
pub struct Stream {
    pub codec_type: Option<String>,
    pub codec_name: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct Format {
    pub format_name: Option<String>,
    pub duration: Option<String>,
}

/// Probe a file; Ok(None) if ffprobe can't make sense of it at all.
pub fn probe(path: &Path) -> Result<Option<Probe>> {
    require("ffprobe")?;
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !out.status.success() {
        return Ok(None);
    }
    let probe: Probe = serde_json::from_slice(&out.stdout).context("parsing ffprobe JSON")?;
    if probe.streams.is_empty() {
        return Ok(None);
    }
    Ok(Some(probe))
}

impl Probe {
    /// Dimensions of the first real video stream.
    pub fn video_dims(&self) -> Option<(u32, u32)> {
        self.streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("video") && s.width.unwrap_or(0) > 0)
            .and_then(|s| Some((s.width?, s.height?)))
    }

    pub fn has_video(&self) -> bool {
        // Cover art streams masquerade as video; require real dimensions.
        self.streams.iter().any(|s| {
            s.codec_type.as_deref() == Some("video")
                && s.width.unwrap_or(0) > 0
                && s.codec_name.as_deref() != Some("mjpeg")
                && s.codec_name.as_deref() != Some("png")
        })
    }

    pub fn has_audio(&self) -> bool {
        self.streams
            .iter()
            .any(|s| s.codec_type.as_deref() == Some("audio"))
    }

    /// Duration of the file as ffprobe sees it (format-level), in seconds.
    pub fn audio_duration(&self) -> Option<f64> {
        self.format.as_ref()?.duration.as_deref()?.parse().ok()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(f) = &self.format {
            if let Some(name) = &f.format_name {
                parts.push(name.clone());
            }
            if let Some(d) = &f.duration {
                parts.push(format!("{d}s"));
            }
        }
        for s in &self.streams {
            let kind = s.codec_type.as_deref().unwrap_or("?");
            let codec = s.codec_name.as_deref().unwrap_or("?");
            match (s.width, s.height) {
                (Some(w), Some(h)) => parts.push(format!("{kind}:{codec} {w}x{h}")),
                _ => parts.push(format!("{kind}:{codec}")),
            }
        }
        parts.join(", ")
    }
}

/// Presentation timestamps of every video keyframe, in order.
pub fn keyframes(path: &Path) -> Result<Vec<f64>> {
    require("ffprobe")?;
    #[derive(Deserialize)]
    struct Packets {
        #[serde(default)]
        packets: Vec<Packet>,
    }
    #[derive(Deserialize)]
    struct Packet {
        pts_time: Option<String>,
        dts_time: Option<String>,
        flags: Option<String>,
    }
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,dts_time,flags",
            "-print_format",
            "json",
        ])
        .arg(path)
        .output()
        .context("running ffprobe for keyframes")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed reading packets: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let packets: Packets = serde_json::from_slice(&out.stdout)?;
    Ok(packets
        .packets
        .iter()
        .filter(|p| p.flags.as_deref().map(|f| f.contains('K')).unwrap_or(false))
        .filter_map(|p| {
            p.pts_time
                .as_deref()
                .or(p.dts_time.as_deref())
                .and_then(|t| t.parse::<f64>().ok())
        })
        .collect())
}

/// Decode the entire file to /dev/null and count decoder errors.
pub fn decode_errors(path: &Path) -> Result<usize> {
    require("ffmpeg")?;
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror"])
        .arg("-i")
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .context("running ffmpeg decode check")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut errors = stderr.lines().filter(|l| !l.trim().is_empty()).count();
    if !out.status.success() && errors == 0 {
        errors = 1;
    }
    Ok(errors)
}

/// Decode up to `n` frames to raw yuv420p, returning the frames and the
/// number of decoder error lines — both halves of "did that work" in one
/// pass. `format` forces the input demuxer (e.g. "h264" for bare streams).
pub fn decode_sample(
    path: &Path,
    n: usize,
    format: Option<&str>,
) -> Result<(Vec<crate::gestalt::Frame>, usize)> {
    require("ffmpeg")?;
    let probe_dims = {
        let mut cmd = Command::new("ffprobe");
        cmd.args(["-v", "error"]);
        if let Some(f) = format {
            cmd.args(["-f", f]);
        }
        cmd.args(["-print_format", "json", "-show_streams"])
            .arg(path);
        let out = cmd.output().context("running ffprobe for dimensions")?;
        serde_json::from_slice::<Probe>(&out.stdout)
            .ok()
            .and_then(|p| p.video_dims())
    };
    let Some((w, h)) = probe_dims else {
        return Ok((Vec::new(), usize::MAX)); // not even probeable: maximal failure
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if let Some(f) = format {
        cmd.args(["-f", f]);
    }
    cmd.arg("-i").arg(path);
    cmd.args([
        "-frames:v",
        &n.to_string(),
        "-f",
        "rawvideo",
        "-pix_fmt",
        "yuv420p",
        "-",
    ]);
    let out = cmd.output().context("running ffmpeg rawvideo decode")?;
    let errors = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();

    let (w, h) = (w as usize, h as usize);
    let frame_size = w * h + 2 * (w.div_ceil(2) * h.div_ceil(2));
    let frames = out
        .stdout
        .chunks_exact(frame_size)
        .filter_map(|c| crate::gestalt::Frame::from_yuv420(c, w, h))
        .collect();
    Ok((frames, errors))
}

/// First frame scaled to 224x224 rgb24 — food for the neural critic.
pub fn decode_rgb224(path: &Path, format: Option<&str>) -> Result<Option<Vec<u8>>> {
    require("ffmpeg")?;
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if let Some(f) = format {
        cmd.args(["-f", f]);
    }
    cmd.arg("-i").arg(path);
    cmd.args([
        "-frames:v",
        "1",
        "-vf",
        "scale=224:224",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "-",
    ]);
    let out = cmd.output().context("running ffmpeg rgb decode")?;
    if out.stdout.len() < 224 * 224 * 3 {
        return Ok(None);
    }
    Ok(Some(out.stdout[..224 * 224 * 3].to_vec()))
}

/// Encode a short synthetic clip — the blank check an organ donor signs.
/// Only the SPS geometry matters (the PPS gets synthesized from scratch and
/// grafted in later), so one encode per resolution suffices. ref=8 oversizes
/// max_num_ref_frames as DPB headroom for whatever the casualty references;
/// crf 12 keeps samples fat so the transplant walker's size ceiling clears
/// whatever the casualty contains.
pub fn synth_donor(out_path: &Path, w: u32, h: u32, fps: u32) -> Result<()> {
    require("ffmpeg")?;
    let src = format!("testsrc2=size={w}x{h}:rate={fps}");
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &src])
        .args([
            "-frames:v",
            "8",
            "-c:v",
            "libx264",
            "-preset",
            "fast",
            "-crf",
            "12",
        ])
        .args(["-x264-params", "ref=8:keyint=16", "-an"])
        .arg(out_path)
        .output()
        .context("running ffmpeg donor encode")?;
    if !out.status.success() {
        bail!(
            "donor encode failed ({w}x{h}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ffmpeg operations
// ---------------------------------------------------------------------------

fn run_ffmpeg(args: &[&str], input: &Path, output: &Path, pre_input: &[&str]) -> Result<()> {
    require("ffmpeg")?;
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-y"]);
    cmd.args(pre_input);
    cmd.arg("-i").arg(input);
    cmd.args(args);
    cmd.arg(output);
    let out = cmd.output().context("running ffmpeg")?;
    if !out.status.success() {
        bail!(
            "ffmpeg failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Losslessly clip from `start_time` (must be a keyframe pts) to the end.
pub fn clip_from_keyframe(input: &Path, output: &Path, start_time: f64) -> Result<()> {
    let ss = format!("{start_time:.6}");
    run_ffmpeg(
        &[
            "-map",
            "0",
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
            "-movflags",
            "+faststart",
        ],
        input,
        output,
        &["-ss", &ss],
    )
}

/// Mux a recovered ADTS audio track onto a video-only mp4. Returns Ok(false)
/// if ffmpeg refused — the caller keeps the silent video rather than failing.
///
/// Two steps, because our salvaged ADTS packs several raw blocks under one
/// sync header: fed straight to the AAC encoder, ffmpeg's ADTS path emits
/// only the first block of each packet and silently drops the rest. Decoding
/// to PCM first forces every block out; then we re-encode that to clean
/// one-AU-per-sample AAC and mux. `-shortest` trims the small tail mismatch
/// from skipped gaps.
/// Returns Ok(None) on refusal, Ok(Some(audio_seconds)) on success — the
/// *true* decoded audio duration (before any `-shortest` trim), which the
/// caller uses as an independent clock to sanity-check the frame rate.
pub fn mux_audio(video: &Path, audio_adts: &Path, output: &Path) -> Result<Option<f64>> {
    require("ffmpeg")?;
    let wav = output.with_extension("audio.wav");
    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .arg("-i")
        .arg(audio_adts)
        .args(["-c:a", "pcm_s16le"])
        .arg(&wav)
        .output()
        .context("running ffmpeg ADTS→PCM decode")?;
    if !decode.status.success() {
        let _ = std::fs::remove_file(&wav);
        return Ok(None);
    }
    let audio_secs = probe(&wav)?.and_then(|p| p.audio_duration());
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .arg("-i")
        .arg(video)
        .arg("-i")
        .arg(&wav)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-shortest",
            "-movflags",
            "+faststart",
        ])
        .arg(output)
        .output()
        .context("running ffmpeg audio mux")?;
    let _ = std::fs::remove_file(&wav);
    Ok(out.status.success().then_some(audio_secs).flatten())
}

/// Remux without re-encoding (used to normalize a reconstructed file).
pub fn remux(input: &Path, output: &Path) -> Result<()> {
    run_ffmpeg(
        &["-map", "0", "-c", "copy", "-movflags", "+faststart"],
        input,
        output,
        &[],
    )
}

/// The Correct Format. Video → H.264/AAC MP4. Audio → MP3.
/// This is not configurable. That is the joke, and also the truth.
pub fn to_correct_format(input: &Path, output: &Path, video: bool) -> Result<()> {
    if video {
        run_ffmpeg(
            &[
                "-c:v",
                "libx264",
                "-preset",
                "medium",
                "-crf",
                "20",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-movflags",
                "+faststart",
            ],
            input,
            output,
            &[],
        )
    } else {
        run_ffmpeg(
            &["-vn", "-c:a", "libmp3lame", "-q:a", "2"],
            input,
            output,
            &[],
        )
    }
}
