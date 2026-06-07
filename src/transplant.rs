// SPDX-License-Identifier: GPL-3.0-or-later
//! The moov transplant: untrunc-style recovery for files whose index is gone.
//!
//! When a file loses its `moov` — a recording cut off mid-write, a faststart
//! file whose front died — the mdat payload is a reel with no spindle. The
//! bytes are all there; nothing says where each sample starts, how long it
//! lasts, or what codec it speaks. But an intact sibling file from the same
//! device speaks the same dialect: same codec parameters (SPS/PPS in `stsd`),
//! same frame cadence, same muxing habits. We harvest that file's organs —
//! its sample descriptions, its `tkhd`, its timing — and regrow the index by
//! walking the orphaned payload one NAL unit at a time:
//!
//!  - H.264 video samples are recovered *exactly*: AVCC length prefixes chain
//!    from sample to sample, and `first_mb_in_slice == 0` (the first bit of
//!    every slice payload) marks each new access unit. IDR NALs mark the
//!    keyframes for a fresh `stss`.
//!  - The bytes between video chains are audio chunks. Constant-sample-size
//!    audio (PCM) splits exactly. Variable-frame audio (AAC) cannot be split
//!    without a decoder in the loop, so it is dropped — and we say so.

use anyhow::{Context, Result, bail};

use crate::forensics::KNOWN_ATOMS;
use crate::h264::{Bits, SpsInfo, parse_sps_from_stsd, strip_emulation};
use crate::mp4::{self, BoxIter, be32, be64, find_box};

// ---------------------------------------------------------------------------
// The donor
// ---------------------------------------------------------------------------

pub struct Donor {
    pub video: Option<DonorVideo>,
    pub audio: Option<DonorAudio>,
}

pub struct DonorVideo {
    /// Entire stsd box, verbatim — carries avcC (SPS/PPS), dimensions, all of it.
    stsd: Vec<u8>,
    /// tkhd fullbox body, verbatim — carries the display matrix (rotation!).
    tkhd: Option<Vec<u8>>,
    pub timescale: u32,
    /// Dominant stts delta — ticks per frame.
    pub sample_delta: u32,
    /// Largest sample in the donor; plausibility ceiling when walking.
    pub max_sample: u32,
    /// Donor uses ctts (B-frame reordering); we re-derive one from slice POCs.
    pub has_ctts: bool,
    /// Parsed from the donor's avcC — needed to read slice headers.
    sps: Option<SpsInfo>,
    pub codec: String,
}

pub struct DonorAudio {
    stsd: Vec<u8>,
    tkhd: Option<Vec<u8>>,
    pub timescale: u32,
    pub sample_delta: u32,
    pub codec: String,
    /// Some(n) when every donor sample is n bytes (PCM-style) — exact audio
    /// recovery is possible. None (AAC etc.) means frame boundaries need a
    /// decoder we don't have; audio will be dropped, plainly.
    pub constant_sample_size: Option<u32>,
}

impl Donor {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = &self.video {
            let fps = v.timescale as f64 / v.sample_delta.max(1) as f64;
            parts.push(format!("video {} @ {:.3} fps", v.codec, fps));
        }
        if let Some(a) = &self.audio {
            match a.constant_sample_size {
                Some(n) => parts.push(format!("audio {} ({n}-byte samples, recoverable)", a.codec)),
                None => parts.push(format!(
                    "audio {} (variable frames — splitting needs a decoder; will be dropped)",
                    a.codec
                )),
            }
        }
        parts.join(", ")
    }
}

/// Harvest the organs from an intact reference file.
pub fn extract_donor(data: &[u8]) -> Result<Donor> {
    let m = mp4::find_moov(data)
        .context("no parseable moov in the reference file — the donor must be intact")?;
    let moov = BoxIter::new(data, m.offset as usize, data.len())
        .next()
        .filter(|b| &b.fourcc == b"moov")
        .context("reference moov unreadable")?;

    let mut video: Option<DonorVideo> = None;
    let mut audio: Option<DonorAudio> = None;

    for trak in BoxIter::new(data, moov.body_start, moov.body_end).filter(|b| &b.fourcc == b"trak")
    {
        let Ok(t) = mp4::parse_track(data, &trak) else {
            continue;
        };
        let tkhd = find_box(data, trak.body_start, trak.body_end, b"tkhd")
            .map(|b| data[b.body_start..b.body_end].to_vec());
        let Some(mdia) = find_box(data, trak.body_start, trak.body_end, b"mdia") else {
            continue;
        };
        let Some(minf) = find_box(data, mdia.body_start, mdia.body_end, b"minf") else {
            continue;
        };
        let Some(stbl) = find_box(data, minf.body_start, minf.body_end, b"stbl") else {
            continue;
        };
        let Some(stsd) = find_box(data, stbl.body_start, stbl.body_end, b"stsd") else {
            continue;
        };
        // Re-box rather than slice backwards: header width stays honest.
        let stsd_raw = boxed(b"stsd", &data[stsd.body_start..stsd.body_end]);

        match t.handler.as_str() {
            "vide" if video.is_none() => {
                if !matches!(t.codec.as_str(), "avc1" | "avc3") {
                    bail!(
                        "donor video codec is `{}` — basinski only knows how to walk \
                         H.264 (avc1/avc3) payloads so far",
                        t.codec
                    );
                }
                let sps = parse_sps_from_stsd(&stsd_raw);
                video = Some(DonorVideo {
                    stsd: stsd_raw,
                    tkhd,
                    timescale: t.timescale,
                    sample_delta: t.dominant_delta(),
                    max_sample: t.sizes.iter().copied().max().unwrap_or(0).max(1),
                    has_ctts: find_box(data, stbl.body_start, stbl.body_end, b"ctts").is_some(),
                    sps,
                    codec: t.codec.clone(),
                });
            }
            "soun" if audio.is_none() => {
                let constant = match t.sizes.first() {
                    Some(&first) if t.sizes.iter().all(|&s| s == first) => Some(first),
                    _ => None,
                };
                audio = Some(DonorAudio {
                    stsd: stsd_raw,
                    tkhd,
                    timescale: t.timescale,
                    sample_delta: t.dominant_delta(),
                    codec: t.codec.clone(),
                    constant_sample_size: constant,
                });
            }
            _ => {}
        }
    }

    if video.is_none()
        && audio
            .as_ref()
            .and_then(|a| a.constant_sample_size)
            .is_none()
    {
        bail!(
            "the reference has no H.264 video track and no constant-size audio — \
             nothing transplantable in it"
        );
    }
    Ok(Donor { video, audio })
}

// ---------------------------------------------------------------------------
// Locating the orphaned media
// ---------------------------------------------------------------------------

/// Find the media payload in the broken file. Returns (start, end, head_torn).
/// `head_torn` means the region begins at an unknown phase (raw payload, no
/// surviving mdat header) — the leading bytes may be a torn sample's tail.
pub(crate) fn locate_media(d: &[u8]) -> (usize, usize, bool) {
    let mut pos = 0usize;
    while pos + 8 <= d.len() {
        let size32 = be32(d, pos) as u64;
        let fourcc: [u8; 4] = d[pos + 4..pos + 8].try_into().unwrap();
        if !KNOWN_ATOMS.contains(&&fourcc) {
            break;
        }
        let (size, hdr) = match size32 {
            0 => ((d.len() - pos) as u64, 8usize),
            1 if pos + 16 <= d.len() => (be64(d, pos + 8), 16usize),
            1 => break,
            s => (s, 8usize),
        };
        if &fourcc == b"mdat" {
            // A tail-cut file's mdat size overruns EOF — that's fine, the
            // header itself survived and anchors the payload start.
            let start = pos + hdr;
            let end = ((pos as u64).saturating_add(size) as usize).clamp(start, d.len());
            return (start.min(d.len()), end, false);
        }
        if size < hdr as u64 || pos as u64 + size > d.len() as u64 {
            break;
        }
        pos += size as usize;
    }
    // No leading chain reaches an mdat. Ask forensics for a *chained*
    // interior mdat header — one whose size lands exactly on EOF or another
    // known atom. A faststart file whose front died looks like this: moov
    // wreckage up front, then a perfectly healthy mdat. Walking the wreckage
    // would invent samples out of stale index tables; skip past it.
    if let Some(hit) = crate::forensics::scan_atoms(d)
        .into_iter()
        .filter(|h| &h.fourcc == b"mdat" && h.chains)
        .max_by_key(|h| h.size)
    {
        let hdr = if be32(d, hit.offset as usize) == 1 {
            16
        } else {
            8
        };
        let start = (hit.offset as usize + hdr).min(d.len());
        let end = ((hit.offset + hit.size) as usize).clamp(start, d.len());
        return (start, end, false);
    }
    // Raw payload at unknown phase (the mdat header died too).
    (0, d.len(), true)
}

// ---------------------------------------------------------------------------
// Walking the payload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Offset within the media region.
    off: usize,
    size: u32,
    sync: bool,
}

enum Nal {
    Valid {
        len: usize,
        ty: u8,
    },
    /// Length and header plausible but the payload overruns EOF — a sample
    /// the truncation tore in half.
    Torn,
    Invalid,
}

fn parse_nal(d: &[u8], i: usize, max_len: usize) -> Nal {
    if i >= d.len() {
        return Nal::Invalid;
    }
    if i + 5 > d.len() {
        return Nal::Torn; // 1–4 trailing crumbs: not even a whole length prefix
    }
    let len = be32(d, i) as usize;
    let nal = d[i + 4];
    let ty = nal & 0x1F;
    let ref_idc = nal >> 5;
    let header_ok = nal & 0x80 == 0
        && (1..=23).contains(&ty)
        && match ty {
            5 | 7 | 8 => ref_idc != 0,  // IDR/SPS/PPS are always references
            6 | 9..=12 => ref_idc == 0, // SEI/AUD/end/filler never are
            _ => true,
        };
    if !header_ok || len < 2 || len > max_len {
        return Nal::Invalid;
    }
    if i + 4 + len > d.len() {
        return Nal::Torn;
    }
    Nal::Valid { len, ty }
}

/// First bit of the slice payload is `first_mb_in_slice == 0` in Exp-Golomb:
/// a set MSB means this slice starts a new picture (access unit).
fn first_mb_zero(d: &[u8], nal_start: usize) -> bool {
    d.get(nal_start + 5).is_some_and(|b| b & 0x80 != 0)
}

/// Would `i` plausibly begin a video sample? (Used to resync after a gap.)
fn au_start_candidate(d: &[u8], i: usize, dv: &DonorVideo) -> bool {
    let max_len = dv.max_sample as usize * 4 + 4096;
    match parse_nal(d, i, max_len) {
        Nal::Valid { ty, .. } => match ty {
            1 | 5 => match &dv.sps {
                Some(sps) => slice_header_sane(d, i, ty == 5, sps),
                None => first_mb_zero(d, i),
            },
            6..=9 => true,
            _ => false,
        },
        _ => false,
    }
}

/// Deep sanity for a resync candidate's slice header, parsed with the donor's
/// SPS. A fake NAL conjured out of audio bytes rarely survives this far:
/// first_mb must be 0, the slice type must exist (and match IDR-ness), the
/// PPS id must be small, and frame_num must parse. Returns that frame_num —
/// the caller holds it against the previous sample's (see below).
fn slice_frame_num(d: &[u8], nal_start: usize, idr: bool, sps: &SpsInfo) -> Option<u64> {
    let end = (nal_start + 5 + 16).min(d.len());
    let raw = d.get(nal_start + 5..end)?;
    let rbsp = strip_emulation(raw);
    let mut b = Bits::new(&rbsp);
    if b.ue()? != 0 {
        return None; // first_mb_in_slice
    }
    let slice_type = b.ue()?;
    if slice_type > 9 || (idr && !matches!(slice_type % 5, 2 | 4)) {
        return None; // nonexistent, or an IDR claiming to be P/B
    }
    if b.ue()? > 3 {
        return None; // pps_id — encoders use 0, exotics stay small
    }
    if sps.separate_colour_plane {
        b.u(2)?;
    }
    b.u(sps.log2_max_frame_num)
}

fn slice_header_sane(d: &[u8], nal_start: usize, idr: bool, sps: &SpsInfo) -> bool {
    slice_frame_num(d, nal_start, idr, sps).is_some()
}

struct AuWalk {
    samples: Vec<Sample>,
    /// First byte past the last complete sample (where the chain broke).
    end: usize,
    torn: bool,
}

/// Greedily walk length-prefixed NALs from `start`, splitting them into
/// access units. Stops at the first byte that doesn't parse (an audio chunk,
/// garbage, or EOF).
fn walk_aus(d: &[u8], start: usize, dv: &DonorVideo) -> AuWalk {
    let max_len = dv.max_sample as usize * 4 + 4096;
    let mut samples = Vec::new();
    let mut pos = start;
    let (mut cur, mut has_slice, mut idr) = (start, false, false);
    let mut torn = false;
    loop {
        match parse_nal(d, pos, max_len) {
            Nal::Valid { len, ty } => {
                let starts_new_au = match ty {
                    1 | 5 => has_slice && first_mb_zero(d, pos),
                    6..=9 => has_slice, // SEI/SPS/PPS/AUD belong to the NEXT picture
                    _ => false,
                };
                if starts_new_au {
                    samples.push(Sample {
                        off: cur,
                        size: (pos - cur) as u32,
                        sync: idr,
                    });
                    cur = pos;
                    has_slice = false;
                    idr = false;
                }
                has_slice |= ty == 1 || ty == 5;
                idr |= ty == 5;
                pos += 4 + len;
            }
            Nal::Torn => {
                // The torn NAL most likely opened the *next* sample; the
                // current one, if it has a slice, is complete.
                if has_slice {
                    samples.push(Sample {
                        off: cur,
                        size: (pos - cur) as u32,
                        sync: idr,
                    });
                    cur = pos;
                }
                torn = true;
                break;
            }
            Nal::Invalid => break,
        }
    }
    if !torn && has_slice {
        samples.push(Sample {
            off: cur,
            size: (pos - cur) as u32,
            sync: idr,
        });
        cur = pos;
    }
    AuWalk {
        samples,
        end: cur,
        torn,
    }
}

/// Scan forward from `from` for the next position where a video sample
/// verifiably begins: candidate header, at least one complete access unit,
/// a sane first slice — and a frame_num that *continues the count*.
///
/// frame_num is the H.264 decoder's own continuity check: each reference
/// frame increments it by exactly one (mod 2^log2_max_frame_num), and an IDR
/// resets it to zero. A fake NAL conjured out of audio bytes carries a random
/// frame_num; the real next sample carries the one the tape expects.
fn next_video_start(
    d: &[u8],
    from: usize,
    dv: &DonorVideo,
    prev_frame_num: Option<u64>,
) -> Option<(usize, AuWalk)> {
    for q in from..d.len().saturating_sub(5) {
        if !au_start_candidate(d, q, dv) {
            continue;
        }
        let aw = walk_aus(d, q, dv);
        if aw.samples.is_empty() {
            continue;
        }
        // The candidate may have opened with a non-slice NAL (SEI etc.);
        // hold the first actual slice to the same standard.
        if let Some(sps) = &dv.sps {
            let Some((pos, idr)) = first_vcl(d, &aw.samples[0]) else {
                continue;
            };
            let Some(fnum) = slice_frame_num(d, pos, idr, sps) else {
                continue;
            };
            if idr {
                if fnum != 0 {
                    continue; // the spec says IDR frame_num is 0; believe it
                }
            } else if let Some(prev) = prev_frame_num {
                let m = 1u64 << sps.log2_max_frame_num;
                // Allow a small jump: non-ref frames repeat the count, and a
                // genuinely damaged frame or two may be missing in between.
                if (fnum + m - prev) % m > 4 {
                    continue;
                }
            }
        }
        return Some((q, aw));
    }
    None
}

/// Position and IDR-ness of a sample's first VCL NAL.
fn first_vcl(d: &[u8], s: &Sample) -> Option<(usize, bool)> {
    let end = s.off + s.size as usize;
    let mut p = s.off;
    while p + 5 <= end {
        let len = be32(d, p) as usize;
        let ty = d[p + 4] & 0x1F;
        if ty == 1 || ty == 5 {
            return Some((p, ty == 5));
        }
        p += 4 + len;
    }
    None
}

struct Walk {
    video: Vec<Sample>,
    /// Non-video byte runs (offset, len) — audio chunks, in muxing order.
    gaps: Vec<(usize, usize)>,
    /// Bytes attributable to nothing: torn samples, head fragments.
    torn: u64,
}

fn walk_media(d: &[u8], head_torn: bool, dv: &DonorVideo) -> Walk {
    let mut video = Vec::new();
    let mut gaps = Vec::new();
    let mut torn = 0u64;
    let mut pos = 0usize;
    // When the head is torn, the first gap is a wounded sample's tail — not
    // a real chunk. Don't let it masquerade as audio.
    let mut gap_is_garbage = head_torn;
    // frame_num of the last accepted sample — the continuity baseline.
    let mut prev_fn: Option<u64> = None;
    while pos < d.len() {
        match next_video_start(d, pos, dv, prev_fn) {
            Some((q, aw)) => {
                if q > pos {
                    if gap_is_garbage {
                        torn += (q - pos) as u64;
                    } else {
                        gaps.push((pos, q - pos));
                    }
                }
                gap_is_garbage = false;
                if let (Some(sps), Some(last)) = (&dv.sps, aw.samples.last()) {
                    prev_fn =
                        first_vcl(d, last).and_then(|(p, idr)| slice_frame_num(d, p, idr, sps));
                }
                video.extend_from_slice(&aw.samples);
                if aw.torn {
                    torn += (d.len() - aw.end) as u64;
                    pos = d.len();
                } else {
                    pos = aw.end;
                }
            }
            None => {
                if gap_is_garbage {
                    torn += (d.len() - pos) as u64;
                } else {
                    gaps.push((pos, d.len() - pos));
                }
                break;
            }
        }
    }
    Walk { video, gaps, torn }
}

// ---------------------------------------------------------------------------
// Composition timing — recovering ctts from the slices themselves
//
// B-frames decode in one order and display in another; the lost moov's ctts
// box held the difference. But H.264 slices carry their own display order:
// pic_order_cnt_lsb, sitting a few Exp-Golomb fields into every slice header.
// Parse the donor's SPS (from avcC) for the field widths, read each recovered
// sample's POC, rank them within their GOP, and the ctts regrows.
// (The bitstream machinery itself lives in h264.rs.)
// ---------------------------------------------------------------------------

/// POC (pic_order_cnt_lsb) of a sample's first slice, plus whether it's IDR.
fn first_slice_poc(media: &[u8], s: &Sample, sps: &SpsInfo) -> Option<(bool, u64)> {
    let (p, idr) = first_vcl(media, s)?;
    let len = be32(media, p) as usize;
    let hdr_end = (p + 5 + 32).min(s.off + s.size as usize).min(p + 4 + len);
    let rbsp = strip_emulation(media.get(p + 5..hdr_end)?);
    let mut b = Bits::new(&rbsp);
    b.ue()?; // first_mb_in_slice
    b.ue()?; // slice_type
    b.ue()?; // pps_id
    if sps.separate_colour_plane {
        b.u(2)?;
    }
    b.u(sps.log2_max_frame_num)?; // frame_num
    if !sps.frame_mbs_only && b.u1()? {
        b.u1()?; // bottom_field_flag
    }
    if idr {
        b.ue()?; // idr_pic_id
    }
    if sps.poc_type != 0 {
        return None;
    }
    Some((idr, b.u(sps.log2_max_poc_lsb)?))
}

/// Per-sample composition offsets (in track ticks), recovered by ranking
/// slice POCs within each IDR-delimited group. None if anything resists.
fn compute_ctts(media: &[u8], samples: &[Sample], dv: &DonorVideo) -> Option<Vec<u32>> {
    let sps = dv.sps.as_ref()?;
    if sps.poc_type == 2 {
        return None; // decode order IS display order; nothing to recover
    }
    let max_lsb = 1u64 << sps.log2_max_poc_lsb;

    // Display rank of each sample, assigned GOP by GOP.
    let mut rank = vec![0i64; samples.len()];
    let mut gop: Vec<(usize, u64)> = Vec::new();
    let mut base = 0i64;
    let flush = |gop: &mut Vec<(usize, u64)>, base: &mut i64, rank: &mut Vec<i64>| {
        let mut order: Vec<usize> = (0..gop.len()).collect();
        order.sort_by_key(|&j| gop[j].1);
        for (r, &j) in order.iter().enumerate() {
            rank[gop[j].0] = *base + r as i64;
        }
        *base += gop.len() as i64;
        gop.clear();
    };

    let (mut prev_lsb, mut wraps) = (0u64, 0u64);
    for (i, s) in samples.iter().enumerate() {
        let (idr, lsb) = first_slice_poc(media, s, sps)?;
        if idr {
            if !gop.is_empty() {
                flush(&mut gop, &mut base, &mut rank);
            }
            wraps = 0;
        } else if lsb < prev_lsb && prev_lsb - lsb > max_lsb / 2 {
            wraps += 1; // the lsb counter rolled over mid-GOP
        }
        gop.push((i, wraps * max_lsb + lsb));
        prev_lsb = lsb;
    }
    flush(&mut gop, &mut base, &mut rank);

    // The reorder delay: enough to make every composition offset non-negative.
    let delay = (0..samples.len()).map(|i| i as i64 - rank[i]).max()?.max(0);
    Some(
        (0..samples.len())
            .map(|i| ((rank[i] + delay - i as i64) as u32) * dv.sample_delta)
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Rebuilding the index
// ---------------------------------------------------------------------------

fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 8);
    b.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    b.extend_from_slice(fourcc);
    b.extend_from_slice(body);
    b
}

fn full_box(fourcc: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 12);
    b.extend_from_slice(&(body.len() as u32 + 12).to_be_bytes());
    b.extend_from_slice(fourcc);
    b.push(version);
    b.extend_from_slice(&flags.to_be_bytes()[1..4]);
    b.extend_from_slice(body);
    b
}

const IDENTITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn chunk_offset_box(offsets: &[u64]) -> Vec<u8> {
    let mut body = (offsets.len() as u32).to_be_bytes().to_vec();
    if offsets.iter().all(|&o| o <= u32::MAX as u64) {
        for &o in offsets {
            body.extend_from_slice(&(o as u32).to_be_bytes());
        }
        full_box(b"stco", 0, 0, &body)
    } else {
        for &o in offsets {
            body.extend_from_slice(&o.to_be_bytes());
        }
        full_box(b"co64", 0, 0, &body)
    }
}

fn mdhd(timescale: u32, duration: u64) -> Vec<u8> {
    let mut b = vec![0u8; 8]; // creation, modification: unknown, honestly zero
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&(duration.min(u32::MAX as u64) as u32).to_be_bytes());
    b.extend_from_slice(&0x55C4u16.to_be_bytes()); // language: und
    b.extend_from_slice(&[0u8; 2]);
    full_box(b"mdhd", 0, 0, &b)
}

fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
    let mut b = vec![0u8; 4];
    b.extend_from_slice(handler);
    b.extend_from_slice(&[0u8; 12]);
    b.extend_from_slice(b"basinski\0");
    full_box(b"hdlr", 0, 0, &b)
}

fn dinf() -> Vec<u8> {
    let url = full_box(b"url ", 0, 1, &[]); // flag 1: data lives in this file
    let mut dref = 1u32.to_be_bytes().to_vec();
    dref.extend(url);
    boxed(b"dinf", &full_box(b"dref", 0, 0, &dref))
}

/// Donor tkhd with the duration swapped in; a fresh one if the donor had none.
fn tkhd(raw: &Option<Vec<u8>>, fallback_id: u32, audio: bool, duration: u64) -> (Vec<u8>, u32) {
    if let Some(body) = raw {
        let mut b = body.clone();
        let v1 = b.first() == Some(&1);
        let (id_off, dur_off) = if v1 { (20, 28) } else { (12, 20) };
        let id = if b.len() >= id_off + 4 {
            be32(&b, id_off)
        } else {
            fallback_id
        };
        if v1 && b.len() >= dur_off + 8 {
            b[dur_off..dur_off + 8].copy_from_slice(&duration.to_be_bytes());
        } else if !v1 && b.len() >= dur_off + 4 {
            let d32 = duration.min(u32::MAX as u64) as u32;
            b[dur_off..dur_off + 4].copy_from_slice(&d32.to_be_bytes());
        }
        (boxed(b"tkhd", &b), id)
    } else {
        let mut b = Vec::with_capacity(80);
        b.extend_from_slice(&[0u8; 8]); // creation, modification
        b.extend_from_slice(&fallback_id.to_be_bytes());
        b.extend_from_slice(&[0u8; 4]); // reserved
        b.extend_from_slice(&(duration.min(u32::MAX as u64) as u32).to_be_bytes());
        b.extend_from_slice(&[0u8; 8]); // reserved
        b.extend_from_slice(&[0u8; 4]); // layer, alternate_group
        b.extend_from_slice(&(if audio { 0x0100u16 } else { 0 }).to_be_bytes());
        b.extend_from_slice(&[0u8; 2]);
        for v in IDENTITY_MATRIX {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(&[0u8; 8]); // width, height: unknown
        (full_box(b"tkhd", 0, 3, &b), fallback_id)
    }
}

fn mvhd(timescale: u32, duration: u64, next_track_id: u32) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&(duration.min(u32::MAX as u64) as u32).to_be_bytes());
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    b.extend_from_slice(&[0u8; 10]); // reserved
    for v in IDENTITY_MATRIX {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&[0u8; 24]); // pre_defined
    b.extend_from_slice(&next_track_id.to_be_bytes());
    full_box(b"mvhd", 0, 0, &b)
}

fn wrap_trak(
    tkhd_raw: &Option<Vec<u8>>,
    fallback_id: u32,
    audio: bool,
    duration_movie_ts: u64,
    mdhd_box: Vec<u8>,
    media_header: Vec<u8>, // vmhd or smhd
    stbl_body: Vec<u8>,
) -> (Vec<u8>, u32) {
    let (tkhd_box, id) = tkhd(tkhd_raw, fallback_id, audio, duration_movie_ts);
    let handler: &[u8; 4] = if audio { b"soun" } else { b"vide" };
    let mut minf = media_header;
    minf.extend(dinf());
    minf.extend(boxed(b"stbl", &stbl_body));
    let mut mdia = mdhd_box;
    mdia.extend(hdlr(handler));
    mdia.extend(boxed(b"minf", &minf));
    let mut trak = tkhd_box;
    trak.extend(boxed(b"mdia", &mdia));
    (boxed(b"trak", &trak), id)
}

fn video_stbl(dv: &DonorVideo, samples: &[Sample], ctts: Option<&[u32]>, base: u64) -> Vec<u8> {
    let n = samples.len() as u32;
    let mut stbl = dv.stsd.clone();

    let mut stts = 1u32.to_be_bytes().to_vec();
    stts.extend_from_slice(&n.to_be_bytes());
    stts.extend_from_slice(&dv.sample_delta.to_be_bytes());
    stbl.extend(full_box(b"stts", 0, 0, &stts));

    // ctts: run-length encoded composition offsets, when we recovered any.
    if let Some(offsets) = ctts.filter(|o| o.iter().any(|&v| v != 0)) {
        let mut entries: Vec<(u32, u32)> = Vec::new();
        for &off in offsets {
            match entries.last_mut() {
                Some((count, last)) if *last == off => *count += 1,
                _ => entries.push((1, off)),
            }
        }
        let mut body = (entries.len() as u32).to_be_bytes().to_vec();
        for (count, off) in entries {
            body.extend_from_slice(&count.to_be_bytes());
            body.extend_from_slice(&off.to_be_bytes());
        }
        stbl.extend(full_box(b"ctts", 0, 0, &body));
    }

    let syncs: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.sync)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    if syncs.len() < samples.len() {
        let mut stss = (syncs.len() as u32).to_be_bytes().to_vec();
        for s in &syncs {
            stss.extend_from_slice(&s.to_be_bytes());
        }
        stbl.extend(full_box(b"stss", 0, 0, &stss));
    }

    // One sample per chunk — the offsets are exact, so why pretend otherwise.
    let mut stsc = 1u32.to_be_bytes().to_vec();
    for v in [1u32, 1, 1] {
        stsc.extend_from_slice(&v.to_be_bytes());
    }
    stbl.extend(full_box(b"stsc", 0, 0, &stsc));

    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend_from_slice(&n.to_be_bytes());
    for s in samples {
        stsz.extend_from_slice(&s.size.to_be_bytes());
    }
    stbl.extend(full_box(b"stsz", 0, 0, &stsz));

    let offsets: Vec<u64> = samples.iter().map(|s| base + s.off as u64).collect();
    stbl.extend(chunk_offset_box(&offsets));
    stbl
}

/// Returns (stbl body, total samples). Chunks that don't hold even one whole
/// sample are skipped.
fn audio_stbl(da: &DonorAudio, css: u32, chunks: &[(usize, usize)], base: u64) -> (Vec<u8>, u64) {
    let counted: Vec<(usize, u32)> = chunks
        .iter()
        .map(|&(off, len)| (off, (len as u64 / css as u64) as u32))
        .filter(|&(_, n)| n > 0)
        .collect();
    let total: u64 = counted.iter().map(|&(_, n)| n as u64).sum();

    let mut stbl = da.stsd.clone();

    let mut stts = 1u32.to_be_bytes().to_vec();
    stts.extend_from_slice(&(total.min(u32::MAX as u64) as u32).to_be_bytes());
    stts.extend_from_slice(&da.sample_delta.to_be_bytes());
    stbl.extend(full_box(b"stts", 0, 0, &stts));

    // stsc: run-length encode samples-per-chunk.
    let mut entries: Vec<(u32, u32)> = Vec::new();
    for (i, &(_, n)) in counted.iter().enumerate() {
        if entries.last().map(|&(_, last)| last) != Some(n) {
            entries.push((i as u32 + 1, n));
        }
    }
    let mut stsc = (entries.len() as u32).to_be_bytes().to_vec();
    for (first, n) in entries {
        for v in [first, n, 1] {
            stsc.extend_from_slice(&v.to_be_bytes());
        }
    }
    stbl.extend(full_box(b"stsc", 0, 0, &stsc));

    let mut stsz = css.to_be_bytes().to_vec(); // uniform size — no table
    stsz.extend_from_slice(&(total.min(u32::MAX as u64) as u32).to_be_bytes());
    stbl.extend(full_box(b"stsz", 0, 0, &stsz));

    let offsets: Vec<u64> = counted.iter().map(|&(off, _)| base + off as u64).collect();
    stbl.extend(chunk_offset_box(&offsets));
    (stbl, total)
}

// ---------------------------------------------------------------------------
// The operation itself
// ---------------------------------------------------------------------------

pub struct Transplant {
    pub out: Vec<u8>,
    pub video_samples: usize,
    pub sync_samples: usize,
    /// Undecodable pre-keyframe samples dropped from the front.
    pub dropped_leading: usize,
    pub audio_samples: u64,
    /// Audio bytes present but unsplittable (no decoder) or unclaimed.
    pub audio_bytes_dropped: u64,
    pub torn_bytes: u64,
    pub duration_s: f64,
    /// B-frame composition timing was re-derived from slice POCs.
    pub ctts_recovered: bool,
    /// Raw AAC salvaged from the interleave gaps, ADTS-wrapped, ready to mux
    /// alongside the video. None when no audio donor track was built and the
    /// gaps didn't look like AAC (or audio recovery was disabled).
    pub audio_adts: Option<Vec<u8>>,
    /// Audio gaps wrapped, and gaps skipped as non-AAC.
    pub aac_chunks: usize,
    pub aac_skipped: usize,
}

/// `audio_rate`: Some(sr) attempts raw-AAC salvage from the gaps at that
/// sample rate when no PCM audio track is built; None disables it.
pub fn transplant_opts(
    broken: &[u8],
    donor: &Donor,
    keep_preroll: bool,
    audio_rate: Option<u32>,
) -> Result<Transplant> {
    let (ms, me, head_torn) = locate_media(broken);
    let media = &broken[ms..me];
    if media.is_empty() {
        bail!("no media payload found to transplant onto");
    }

    let (mut samples, mut gaps, torn) = match &donor.video {
        Some(dv) => {
            let w = walk_media(media, head_torn, dv);
            (w.video, w.gaps, w.torn)
        }
        // Audio-only donor: the whole payload is one chunk of constant-size
        // samples (extract_donor guarantees constant size in this case).
        None => (Vec::new(), vec![(0usize, media.len())], 0u64),
    };

    if donor.video.is_some() && samples.is_empty() {
        bail!(
            "the donor's H.264 grammar matches nothing in this file — \
             wrong donor, or the media is truly gone"
        );
    }

    // Clip at the walk level: video before the first keyframe can never
    // decode; drop it (and the audio chunks interleaved beside it) unless
    // the user wants the wreckage.
    let mut dropped_leading = 0usize;
    if !keep_preroll && !samples.is_empty() {
        let first_sync = samples
            .iter()
            .position(|s| s.sync)
            .context("no keyframe survives in the recovered video — nothing playable")?;
        if first_sync > 0 {
            let cut_off = samples[first_sync].off;
            dropped_leading = first_sync;
            samples.drain(..first_sync);
            gaps.retain(|&(off, _)| off >= cut_off);
        }
    }

    // Assemble: ftyp + mdat (payload verbatim) + a freshly grown moov.
    let ftyp = mp4::synth_ftyp();
    let large = media.len() as u64 + 16 > u32::MAX as u64;
    let mdat_hdr_len: u64 = if large { 16 } else { 8 };
    let base = ftyp.len() as u64 + mdat_hdr_len;
    let movie_ts = 1000u32; // movie timescale: milliseconds

    let mut traks = Vec::new();
    let mut duration_ms = 0u64;
    let mut max_id = 0u32;
    let mut sync_samples = 0usize;
    let mut audio_samples = 0u64;
    let mut audio_bytes_dropped = 0u64;

    let mut ctts_recovered = false;
    if let Some(dv) = &donor.video {
        // Donor reorders B-frames (has a ctts)? Regrow ours from slice POCs.
        let ctts = if dv.has_ctts {
            compute_ctts(media, &samples, dv)
        } else {
            None
        };
        ctts_recovered = ctts.is_some();
        let dur_ticks = samples.len() as u64 * dv.sample_delta as u64;
        let dur_ms = dur_ticks * 1000 / dv.timescale.max(1) as u64;
        sync_samples = samples.iter().filter(|s| s.sync).count();
        let stbl = video_stbl(dv, &samples, ctts.as_deref(), base);
        let vmhd = full_box(b"vmhd", 0, 1, &[0u8; 8]);
        let (trak, id) = wrap_trak(
            &dv.tkhd,
            max_id + 1,
            false,
            dur_ms,
            mdhd(dv.timescale, dur_ticks),
            vmhd,
            stbl,
        );
        traks.push(trak);
        duration_ms = duration_ms.max(dur_ms);
        max_id = max_id.max(id);
    }

    if let Some(da) = &donor.audio {
        match da.constant_sample_size {
            Some(css) if !gaps.is_empty() => {
                let (stbl, total) = audio_stbl(da, css, &gaps, base);
                if total > 0 {
                    let dur_ticks = total * da.sample_delta as u64;
                    let dur_ms = dur_ticks * 1000 / da.timescale.max(1) as u64;
                    audio_samples = total;
                    let smhd = full_box(b"smhd", 0, 0, &[0u8; 4]);
                    let (trak, id) = wrap_trak(
                        &da.tkhd,
                        max_id + 1,
                        true,
                        dur_ms,
                        mdhd(da.timescale, dur_ticks),
                        smhd,
                        stbl,
                    );
                    traks.push(trak);
                    duration_ms = duration_ms.max(dur_ms);
                    max_id = max_id.max(id);
                }
            }
            _ => {
                audio_bytes_dropped = gaps.iter().map(|&(_, len)| len as u64).sum();
            }
        }
    } else {
        audio_bytes_dropped = gaps.iter().map(|&(_, len)| len as u64).sum();
    }

    // No PCM track was built, but the gaps may still be raw AAC. Salvage it
    // into an ADTS stream the rescue step can mux back in.
    let mut audio_adts = None;
    let (mut aac_chunks, mut aac_skipped) = (0usize, 0usize);
    if audio_samples == 0
        && let Some(sr) = audio_rate
        && let Some(rec) = crate::aac::recover(media, &gaps, sr)
    {
        aac_chunks = rec.chunks;
        aac_skipped = rec.skipped;
        audio_bytes_dropped = gaps
            .iter()
            .map(|&(_, l)| l as u64)
            .sum::<u64>()
            .saturating_sub(
                // chunks we DID keep aren't "dropped"
                gaps.iter()
                    .filter(|&&(o, l)| l > 16 && (media[o] & 0xE0) == 0x20)
                    .map(|&(_, l)| l as u64)
                    .sum::<u64>(),
            );
        audio_adts = Some(rec.adts);
    }

    if traks.is_empty() {
        bail!("transplant produced no tracks — nothing in this file fits the donor");
    }

    let mut moov_body = mvhd(movie_ts, duration_ms, max_id + 1);
    for t in &traks {
        moov_body.extend_from_slice(t);
    }
    let moov = boxed(b"moov", &moov_body);

    let mut out = ftyp;
    if large {
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(b"mdat");
        out.extend_from_slice(&(media.len() as u64 + 16).to_be_bytes());
    } else {
        out.extend_from_slice(&(media.len() as u32 + 8).to_be_bytes());
        out.extend_from_slice(b"mdat");
    }
    out.extend_from_slice(media);
    out.extend_from_slice(&moov);

    Ok(Transplant {
        out,
        video_samples: samples.len(),
        sync_samples,
        dropped_leading,
        audio_samples,
        audio_bytes_dropped,
        torn_bytes: torn,
        duration_s: duration_ms as f64 / 1000.0,
        ctts_recovered,
        audio_adts,
        aac_chunks,
        aac_skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::Analysis;
    use crate::mp4::testutil::{nal_sample, synthetic_original};

    /// ftyp(32) + free(8) + mdat header(8) in the synthetic original.
    const MEDIA_START: usize = 48;

    #[test]
    fn donor_extraction_reads_the_organs() {
        let (file, _) = synthetic_original();
        let d = extract_donor(&file).unwrap();
        let v = d.video.as_ref().unwrap();
        assert_eq!(v.timescale, 30);
        assert_eq!(v.sample_delta, 1);
        assert_eq!(v.max_sample, 900);
        assert!(!v.has_ctts);
        assert!(d.audio.is_none());
    }

    #[test]
    fn tail_cut_gets_a_transplant() {
        let (file, sizes) = synthetic_original();
        let donor = extract_donor(&file).unwrap();
        // Keep 4 complete samples plus 10 torn bytes of the fifth.
        let keep = MEDIA_START + sizes[..4].iter().sum::<u32>() as usize + 10;
        let broken = &file[..keep];
        assert!(matches!(mp4::analyze(broken), Analysis::NoMoov));

        let t = transplant_opts(broken, &donor, false, None).unwrap();
        assert_eq!(t.video_samples, 4);
        assert_eq!(t.sync_samples, 1);
        assert_eq!(t.dropped_leading, 0);
        assert_eq!(t.torn_bytes, 10);
        assert!(matches!(mp4::analyze(&t.out), Analysis::Intact));
        let m = mp4::find_moov(&t.out).unwrap();
        assert_eq!(m.tracks.len(), 1);
        assert_eq!(m.tracks[0].sample_count, 4);
        assert_eq!(m.tracks[0].sync_samples.as_deref(), Some(&[1u32][..]));
    }

    #[test]
    fn head_cut_resyncs_and_clips_to_the_keyframe() {
        let (file, sizes) = synthetic_original();
        let donor = extract_donor(&file).unwrap();
        // Cut 17 bytes into sample 0 and end exactly after sample 5 — a
        // faststart-style wound: no atoms at all, unknown phase.
        let begin = MEDIA_START + 17;
        let end = MEDIA_START + sizes[..6].iter().sum::<u32>() as usize;
        let broken = &file[begin..end];

        let t = transplant_opts(broken, &donor, false, None).unwrap();
        // Walker resyncs at sample 1; samples 1–3 precede the IDR at index 4.
        assert_eq!(t.dropped_leading, 3);
        assert_eq!(t.video_samples, 2); // the IDR and one trailer
        assert_eq!(t.sync_samples, 1);
        assert_eq!(t.torn_bytes, (sizes[0] - 17_u32) as u64);
        assert!(matches!(mp4::analyze(&t.out), Analysis::Intact));
    }

    #[test]
    fn pcm_audio_splits_exactly() {
        let (file, _) = synthetic_original();
        let mut donor = extract_donor(&file).unwrap();
        let mut stsd_body = vec![0u8; 4];
        stsd_body.extend_from_slice(&1u32.to_be_bytes());
        stsd_body.extend(super::boxed(b"sowt", &[0u8; 28]));
        donor.audio = Some(DonorAudio {
            stsd: super::boxed(b"stsd", &stsd_body),
            tkhd: None,
            timescale: 8000,
            sample_delta: 1,
            codec: "sowt".into(),
            constant_sample_size: Some(4),
        });

        // Media: [video IDR 100][16 B PCM][video P 60][8 B PCM]
        let mut media = nal_sample(100, 0x65, 1);
        media.extend(vec![0xAAu8; 16]);
        media.extend(nal_sample(60, 0x41, 2));
        media.extend(vec![0xBBu8; 8]);
        let mut broken = Vec::new();
        broken.extend_from_slice(&20u32.to_be_bytes());
        broken.extend_from_slice(b"ftypisom");
        broken.extend_from_slice(&0x200u32.to_be_bytes());
        broken.extend_from_slice(b"isom");
        broken.extend(super::boxed(b"mdat", &media));
        assert!(matches!(mp4::analyze(&broken), Analysis::NoMoov));

        let t = transplant_opts(&broken, &donor, false, None).unwrap();
        assert_eq!(t.video_samples, 2);
        assert_eq!(t.audio_samples, 6); // 16/4 + 8/4
        assert_eq!(t.audio_bytes_dropped, 0);
        assert!(matches!(mp4::analyze(&t.out), Analysis::Intact));
        let m = mp4::find_moov(&t.out).unwrap();
        assert_eq!(m.tracks.len(), 2);
        let audio = m.tracks.iter().find(|t| t.handler == "soun").unwrap();
        assert_eq!(audio.sample_count, 6);
        assert!(audio.sample_locations().iter().all(|&(_, s)| s == 4));
    }

    #[test]
    fn aac_audio_is_dropped_with_a_count() {
        let (file, sizes) = synthetic_original();
        let mut donor = extract_donor(&file).unwrap();
        donor.audio = Some(DonorAudio {
            stsd: super::boxed(b"stsd", &[0u8; 8]),
            tkhd: None,
            timescale: 44100,
            sample_delta: 1024,
            codec: "mp4a".into(),
            constant_sample_size: None, // variable — like AAC
        });
        let keep = MEDIA_START + sizes[..5].iter().sum::<u32>() as usize;
        let broken = &file[..keep];
        let t = transplant_opts(broken, &donor, false, None).unwrap();
        assert_eq!(t.video_samples, 5);
        assert_eq!(t.audio_samples, 0);
        let m = mp4::find_moov(&t.out).unwrap();
        assert_eq!(m.tracks.len(), 1); // video only — audio honestly dropped
    }
}
