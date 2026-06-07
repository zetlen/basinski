// SPDX-License-Identifier: GPL-3.0-or-later
//! MP4 (ISO BMFF) forensic parsing and reconstruction.
//!
//! The trick that makes head_truncated MP4s rescuable: ffmpeg and most muxers write
//! the `moov` box — the complete index of the file, including every sample's
//! *absolute byte offset* — at the END of the file. Cut the head off an MP4
//! and the index usually survives. If we can work out exactly how many bytes
//! `K` were lost, we can regrow a prosthetic prefix of exactly K's worth of
//! structure (`ftyp` + a `free` box of silence, plus a fresh `mdat` header if
//! the original one died) and every offset in the index becomes true again.
//!
//! Two ways to find K:
//!  1. **mdat anchor** — the surviving `mdat` header tells us where it sits
//!     now; the sample table tells us where it used to sit.
//!  2. **NAL correlation** — when even the mdat header is gone, slide the
//!     sample-size table (`stsz`) along the surviving bytes until the H.264
//!     length-prefixed NAL structure lines up. The index fits the data like
//!     a dental record.

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::forensics::{avcc_chain_len, scan_atoms};

pub(crate) fn be32(d: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(d[i..i + 4].try_into().unwrap())
}
pub(crate) fn be64(d: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(d[i..i + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Box walking
// ---------------------------------------------------------------------------

pub(crate) struct BoxIter<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

pub(crate) struct BoxRef {
    pub(crate) fourcc: [u8; 4],
    /// Payload range (after size+fourcc header).
    pub(crate) body_start: usize,
    pub(crate) body_end: usize,
}

impl<'a> BoxIter<'a> {
    pub(crate) fn new(data: &'a [u8], start: usize, end: usize) -> Self {
        Self {
            data,
            pos: start,
            end: end.min(data.len()),
        }
    }
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = BoxRef;
    fn next(&mut self) -> Option<BoxRef> {
        if self.pos + 8 > self.end {
            return None;
        }
        let size32 = be32(self.data, self.pos) as u64;
        let fourcc: [u8; 4] = self.data[self.pos + 4..self.pos + 8].try_into().unwrap();
        let (size, hdr) = match size32 {
            0 => ((self.end - self.pos) as u64, 8usize),
            1 => {
                if self.pos + 16 > self.end {
                    return None;
                }
                (be64(self.data, self.pos + 8), 16usize)
            }
            s => (s, 8usize),
        };
        if size < hdr as u64 || self.pos as u64 + size > self.end as u64 {
            return None;
        }
        let b = BoxRef {
            fourcc,
            body_start: self.pos + hdr,
            body_end: self.pos + size as usize,
        };
        self.pos += size as usize;
        Some(b)
    }
}

pub(crate) fn find_box(data: &[u8], start: usize, end: usize, fourcc: &[u8; 4]) -> Option<BoxRef> {
    BoxIter::new(data, start, end).find(|b| &b.fourcc == fourcc)
}

// ---------------------------------------------------------------------------
// Sample tables
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Track {
    /// 'vide', 'soun', etc.
    pub handler: String,
    /// First stsd entry fourcc, e.g. "avc1", "mp4a".
    pub codec: String,
    pub timescale: u32,
    pub sample_count: usize,
    #[serde(skip)]
    pub(crate) sizes: Vec<u32>,
    #[serde(skip)]
    chunk_offsets: Vec<u64>,
    #[serde(skip)]
    stsc: Vec<(u32, u32)>, // (first_chunk 1-based, samples_per_chunk)
    /// 1-based sync sample numbers; None means every sample is sync.
    #[serde(skip)]
    pub sync_samples: Option<Vec<u32>>,
    #[serde(skip)]
    pub(crate) stts: Vec<(u32, u32)>, // (count, delta)
}

impl Track {
    /// Absolute (original-file) offset and size of every sample, in order.
    pub fn sample_locations(&self) -> Vec<(u64, u32)> {
        let mut out = Vec::with_capacity(self.sizes.len());
        let mut sample = 0usize;
        for (ci, &chunk_off) in self.chunk_offsets.iter().enumerate() {
            let per_chunk = samples_in_chunk(&self.stsc, ci as u32 + 1);
            let mut off = chunk_off;
            for _ in 0..per_chunk {
                if sample >= self.sizes.len() {
                    return out;
                }
                let size = self.sizes[sample];
                out.push((off, size));
                off += size as u64;
                sample += 1;
            }
        }
        out
    }

    /// Decode timestamp of each sample in seconds.
    pub fn sample_times(&self) -> Vec<f64> {
        let mut out = Vec::with_capacity(self.sizes.len());
        let mut t = 0u64;
        for &(count, delta) in &self.stts {
            for _ in 0..count {
                out.push(t as f64 / self.timescale.max(1) as f64);
                t += delta as u64;
            }
        }
        out
    }

    pub fn is_video(&self) -> bool {
        self.handler == "vide"
    }

    /// The most common sample duration — the file's natural frame cadence.
    pub fn dominant_delta(&self) -> u32 {
        self.stts
            .iter()
            .max_by_key(|&&(count, _)| count)
            .map(|&(_, delta)| delta)
            .unwrap_or(1)
            .max(1)
    }
}

fn samples_in_chunk(stsc: &[(u32, u32)], chunk_1based: u32) -> u32 {
    let mut per = stsc.first().map(|e| e.1).unwrap_or(0);
    for &(first, count) in stsc {
        if first <= chunk_1based {
            per = count;
        } else {
            break;
        }
    }
    per
}

pub(crate) fn parse_track(data: &[u8], trak: &BoxRef) -> Result<Track> {
    let mdia =
        find_box(data, trak.body_start, trak.body_end, b"mdia").context("trak missing mdia")?;
    let mdhd =
        find_box(data, mdia.body_start, mdia.body_end, b"mdhd").context("mdia missing mdhd")?;
    let version = data[mdhd.body_start];
    let timescale = if version == 1 {
        be32(data, mdhd.body_start + 20)
    } else {
        be32(data, mdhd.body_start + 12)
    };
    let hdlr =
        find_box(data, mdia.body_start, mdia.body_end, b"hdlr").context("mdia missing hdlr")?;
    let handler =
        String::from_utf8_lossy(&data[hdlr.body_start + 8..hdlr.body_start + 12]).into_owned();
    let minf =
        find_box(data, mdia.body_start, mdia.body_end, b"minf").context("mdia missing minf")?;
    let stbl =
        find_box(data, minf.body_start, minf.body_end, b"stbl").context("minf missing stbl")?;

    // stsd: codec fourcc of first entry
    let stsd =
        find_box(data, stbl.body_start, stbl.body_end, b"stsd").context("stbl missing stsd")?;
    let codec = if stsd.body_end - stsd.body_start >= 16 {
        String::from_utf8_lossy(&data[stsd.body_start + 12..stsd.body_start + 16]).into_owned()
    } else {
        "????".into()
    };

    // stsz
    let stsz =
        find_box(data, stbl.body_start, stbl.body_end, b"stsz").context("stbl missing stsz")?;
    let uniform = be32(data, stsz.body_start + 4);
    let count = be32(data, stsz.body_start + 8) as usize;
    let sizes: Vec<u32> = if uniform != 0 {
        vec![uniform; count]
    } else {
        (0..count)
            .map(|i| be32(data, stsz.body_start + 12 + i * 4))
            .collect()
    };

    // stco / co64
    let chunk_offsets: Vec<u64> =
        if let Some(stco) = find_box(data, stbl.body_start, stbl.body_end, b"stco") {
            let n = be32(data, stco.body_start + 4) as usize;
            (0..n)
                .map(|i| be32(data, stco.body_start + 8 + i * 4) as u64)
                .collect()
        } else if let Some(co64) = find_box(data, stbl.body_start, stbl.body_end, b"co64") {
            let n = be32(data, co64.body_start + 4) as usize;
            (0..n)
                .map(|i| be64(data, co64.body_start + 8 + i * 8))
                .collect()
        } else {
            bail!("stbl has neither stco nor co64");
        };

    // stsc
    let stsc_box =
        find_box(data, stbl.body_start, stbl.body_end, b"stsc").context("stbl missing stsc")?;
    let n = be32(data, stsc_box.body_start + 4) as usize;
    let stsc: Vec<(u32, u32)> = (0..n)
        .map(|i| {
            let base = stsc_box.body_start + 8 + i * 12;
            (be32(data, base), be32(data, base + 4))
        })
        .collect();

    // stss (optional — absent means all samples are sync)
    let sync_samples = find_box(data, stbl.body_start, stbl.body_end, b"stss").map(|stss| {
        let n = be32(data, stss.body_start + 4) as usize;
        (0..n)
            .map(|i| be32(data, stss.body_start + 8 + i * 4))
            .collect()
    });

    // stts
    let stts_box =
        find_box(data, stbl.body_start, stbl.body_end, b"stts").context("stbl missing stts")?;
    let n = be32(data, stts_box.body_start + 4) as usize;
    let stts: Vec<(u32, u32)> = (0..n)
        .map(|i| {
            let base = stts_box.body_start + 8 + i * 8;
            (be32(data, base), be32(data, base + 4))
        })
        .collect();

    Ok(Track {
        handler,
        codec,
        timescale,
        sample_count: sizes.len(),
        sizes,
        chunk_offsets,
        stsc,
        sync_samples,
        stts,
    })
}

#[derive(Debug)]
pub struct Moov {
    pub tracks: Vec<Track>,
    /// Offset of the moov box within the buffer it was scanned from.
    pub offset: u64,
}

/// Hunt for a parseable moov box anywhere in the buffer.
pub fn find_moov(data: &[u8]) -> Option<Moov> {
    let mut candidates: Vec<_> = scan_atoms(data)
        .into_iter()
        .filter(|h| &h.fourcc == b"moov")
        .collect();
    // Prefer chained hits (ending at EOF or another atom) — a real moov chains.
    candidates.sort_by_key(|h| std::cmp::Reverse((h.chains, h.size)));
    for hit in candidates {
        let start = hit.offset as usize;
        let end = (hit.offset + hit.size) as usize;
        let tracks: Vec<Track> = BoxIter::new(data, start + 8, end)
            .filter(|b| &b.fourcc == b"trak")
            .filter_map(|t| parse_track(data, &t).ok())
            .collect();
        if !tracks.is_empty() {
            return Some(Moov {
                tracks,
                offset: hit.offset,
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Damage analysis
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub enum Analysis {
    /// The atom chain walks cleanly from byte 0 to EOF and includes moov.
    Intact,
    /// Front of the file is gone, but the index survived. Recoverable.
    HeadTruncated(HeadTruncation),
    /// No usable moov anywhere. Without the index (or a reference file with
    /// matching codec parameters) the mdat payload is a reel with no spindle.
    NoMoov,
}

#[derive(Debug, Serialize)]
pub struct HeadTruncation {
    /// K — number of bytes cut from the front.
    pub cut_bytes: u64,
    /// How K was determined.
    pub method: String,
    /// Lowest chunk offset (original coords) — first byte of media data.
    pub media_start: u64,
    /// moov position in original coords.
    pub moov_orig: u64,
    /// Bytes of actual media data destroyed (zero-filled on reconstruction).
    pub media_bytes_lost: u64,
    /// Number of leading video sync samples whose data was destroyed.
    pub damaged_keyframes: usize,
    /// Decode time (seconds) of the first moment every track is intact —
    /// video at a clean sync sample, audio past its last destroyed sample.
    /// Clip here for artifact-free playback.
    pub first_clean_keyframe_time: Option<f64>,
    pub tracks: Vec<Track>,
}

/// Does the buffer parse as a clean top-level atom chain from 0 to EOF?
/// Necessary but NOT sufficient for intactness: cut exactly an mp4's ftyp off
/// and `free+mdat+moov` still walks cleanly — while every chunk offset in the
/// index now lies by ftyp's size. Alignment must be verified separately.
fn chain_walks_clean(data: &[u8]) -> (bool, Vec<(u64, u64)>) {
    let mut pos = 0usize;
    let mut saw_moov = false;
    let mut mdat_ranges = Vec::new();
    for b in BoxIter::new(data, 0, data.len()) {
        if &b.fourcc == b"moov" {
            saw_moov = true;
        }
        if &b.fourcc == b"mdat" {
            mdat_ranges.push((b.body_start as u64, b.body_end as u64));
        }
        pos = b.body_end;
    }
    (saw_moov && pos == data.len(), mdat_ranges)
}

/// Does every sample the index describes fall inside an mdat? A front-cut
/// shifts real data leftward, so the index's tail samples overhang mdat's end.
fn samples_within_mdat(tracks: &[Track], mdat_ranges: &[(u64, u64)]) -> bool {
    tracks.iter().all(|t| {
        t.sample_locations().iter().all(|&(off, size)| {
            mdat_ranges
                .iter()
                .any(|&(start, end)| off >= start && off + size as u64 <= end)
        })
    })
}

/// Check that the index agrees with the data for a given K: at each surviving
/// video sample offset, the AVCC NAL-length chain must sum exactly to the
/// sample size recorded in stsz.
fn verify_k(data: &[u8], tracks: &[Track], k: u64, checks: usize) -> bool {
    let Some(video) = tracks.iter().find(|t| t.is_video()) else {
        return true; // nothing to verify against; trust the anchor
    };
    let locs = video.sample_locations();
    let mut tried = 0;
    let mut ok = 0;
    for &(off, size) in &locs {
        if off < k {
            continue; // destroyed region
        }
        let pos = (off - k) as usize;
        if pos + size as usize > data.len() {
            break;
        }
        // All-zero bytes mean *erased* (e.g. our own zero-fill), not shifted —
        // real H.264 data never opens with a zero run (emulation prevention).
        // Erased samples abstain from the alignment vote.
        let peek = (size as usize).min(16);
        if data[pos..pos + peek].iter().all(|&x| x == 0) {
            continue;
        }
        if nal_chain_sums_to(data, pos, size as usize) {
            ok += 1;
        }
        tried += 1;
        if tried >= checks {
            break;
        }
    }
    tried > 0 && ok * 10 >= tried * 8 // >= 80% must line up
}

/// Walk 4-byte BE length prefixes from `pos`; true if valid NALs sum to `size`.
fn nal_chain_sums_to(data: &[u8], pos: usize, size: usize) -> bool {
    let mut consumed = 0usize;
    let mut nals = 0;
    while consumed < size && nals < 64 {
        let i = pos + consumed;
        if i + 5 > data.len() {
            return false;
        }
        let len = be32(data, i) as usize;
        let nal = data[i + 4];
        if len == 0 || nal & 0x80 != 0 || !(1..=23).contains(&(nal & 0x1F)) {
            return false;
        }
        consumed += 4 + len;
        nals += 1;
    }
    consumed == size
}

/// Last-ditch K finder: slide along the buffer looking for a position whose
/// NAL structure matches the first surviving video sample's recorded size.
fn correlate_k(data: &[u8], tracks: &[Track]) -> Option<u64> {
    let video = tracks.iter().find(|t| t.is_video())?;
    let locs = video.sample_locations();
    // Search window: the head of the file (the cut is at the front).
    let window = data.len().min(1 << 20);
    for q in 0..window.saturating_sub(5) {
        if avcc_chain_len(data, q, 4) < 1 {
            continue;
        }
        // Try to attribute position q to each of the first few samples.
        for &(off, size) in locs.iter().take(64) {
            if off < q as u64 {
                continue;
            }
            if nal_chain_sums_to(data, q, size as usize) {
                let k = off - q as u64;
                if verify_k(data, tracks, k, 12) {
                    return Some(k);
                }
            }
        }
    }
    None
}

pub fn analyze(data: &[u8]) -> Analysis {
    let (chain_clean, mdat_ranges) = chain_walks_clean(data);
    let Some(moov) = find_moov(data) else {
        return Analysis::NoMoov;
    };
    if chain_clean {
        // Structure parses end to end — but is the index telling the truth?
        let has_video = moov.tracks.iter().any(|t| t.is_video());
        let aligned = if has_video {
            verify_k(data, &moov.tracks, 0, 12)
        } else {
            samples_within_mdat(&moov.tracks, &mdat_ranges)
        };
        if aligned {
            return Analysis::Intact;
        }
    }

    let media_start = moov
        .tracks
        .iter()
        .flat_map(|t| t.chunk_offsets.first().copied())
        .min()
        .unwrap_or(0);

    // K, method 1: anchor on a surviving mdat header. In the truncated file
    // the mdat box still chains (its size still reaches exactly to moov), so
    // a chained hit is a trustworthy anchor.
    let mut k_and_method: Option<(u64, String)> = None;
    let mdat_hits: Vec<_> = scan_atoms(data)
        .into_iter()
        .filter(|h| &h.fourcc == b"mdat" && h.offset < moov.offset)
        .collect();
    for hit in mdat_hits
        .iter()
        .filter(|h| h.chains)
        .chain(mdat_hits.iter().filter(|h| !h.chains))
    {
        // Original position of the mdat header is media_start - 8.
        if media_start >= 8 && media_start - 8 >= hit.offset {
            let k = media_start - 8 - hit.offset;
            if verify_k(data, &moov.tracks, k, 12) {
                k_and_method = Some((k, "mdat header anchor".into()));
                break;
            }
        }
    }

    // K, method 2: correlate the sample-size table against raw NAL structure.
    if k_and_method.is_none()
        && let Some(k) = correlate_k(data, &moov.tracks)
    {
        k_and_method = Some((k, "stsz/NAL-chain correlation".into()));
    }

    let Some((k, method)) = k_and_method else {
        return Analysis::NoMoov; // index present but data doesn't line up
    };

    // Census of the dead: which samples lived in the cut region? The clip
    // target is the latest "first clean moment" across all tracks — video
    // must restart at a sync sample, audio merely at an intact sample.
    let media_bytes_lost = k.saturating_sub(media_start);
    let mut damaged_keyframes = 0usize;
    let mut first_clean_keyframe_time = None;
    if media_bytes_lost > 0 {
        for track in &moov.tracks {
            let locs = track.sample_locations();
            let times = track.sample_times();
            let first_clean = if track.is_video() {
                let syncs: Vec<u32> = track
                    .sync_samples
                    .clone()
                    .unwrap_or_else(|| (1..=track.sample_count as u32).collect());
                let mut clean = None;
                for &s in &syncs {
                    let idx = (s - 1) as usize;
                    if let Some(&(off, _)) = locs.get(idx) {
                        if off < k {
                            damaged_keyframes += 1;
                        } else if clean.is_none() {
                            clean = times.get(idx).copied();
                        }
                    }
                }
                clean
            } else {
                locs.iter()
                    .position(|&(off, _)| off >= k)
                    .and_then(|i| times.get(i).copied())
            };
            if let Some(t) = first_clean
                && first_clean_keyframe_time
                    .map(|c: f64| t > c)
                    .unwrap_or(true)
            {
                first_clean_keyframe_time = Some(t);
            }
        }
    }

    Analysis::HeadTruncated(HeadTruncation {
        cut_bytes: k,
        method,
        media_start,
        moov_orig: moov.offset + k,
        media_bytes_lost,
        damaged_keyframes,
        first_clean_keyframe_time,
        tracks: moov.tracks,
    })
}

// ---------------------------------------------------------------------------
// Reconstruction
// ---------------------------------------------------------------------------

pub(crate) fn synth_ftyp() -> Vec<u8> {
    // size(4) 'ftyp' major(4) minor(4) compat(4) = 20 bytes
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(&20u32.to_be_bytes());
    b.extend_from_slice(b"ftypisom");
    b.extend_from_slice(&0x200u32.to_be_bytes());
    b.extend_from_slice(b"isom");
    b
}

/// A `free` box: structural silence. The honest atom — it admits it holds
/// nothing. We use it to stand in for everything the tape lost.
fn synth_free(total_size: u64) -> Vec<u8> {
    assert!(total_size >= 8);
    let mut b = Vec::with_capacity(total_size as usize);
    if total_size <= u32::MAX as u64 {
        b.extend_from_slice(&(total_size as u32).to_be_bytes());
        b.extend_from_slice(b"free");
    } else {
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(b"free");
        b.extend_from_slice(&total_size.to_be_bytes());
    }
    b.resize(total_size as usize, 0);
    b
}

/// Fill `[0, len)` of the original coordinate space with valid atoms.
fn synth_prefix(len: u64) -> Result<Vec<u8>> {
    if len >= 28 {
        let mut p = synth_ftyp();
        p.extend(synth_free(len - 20));
        Ok(p)
    } else if len >= 8 {
        Ok(synth_free(len))
    } else {
        bail!(
            "need to synthesize a {len}-byte prefix but the smallest atom is 8 bytes; \
             this layout requires chunk-offset patching, which this build does not do yet"
        )
    }
}

/// Rebuild the original file image from the head_truncated buffer.
pub fn reconstruct(data: &[u8], b: &HeadTruncation) -> Result<Vec<u8>> {
    let k = b.cut_bytes;
    let mdat_hdr_orig = b
        .media_start
        .checked_sub(8)
        .context("media starts before offset 8")?;
    // First original byte that must be *preserved* (not synthesized):
    // whichever comes first, the moov or the mdat header.
    let r = b.moov_orig.min(mdat_hdr_orig);

    let mut out = Vec::with_capacity(k as usize + data.len());
    if k <= r {
        // The cut only removed synthesizable structure (ftyp/free). Regrow
        // exactly K bytes of prefix and the survivors line right up.
        out.extend(synth_prefix(k)?);
        out.extend_from_slice(data);
    } else {
        // The cut reached into the mdat header or the media itself.
        if b.moov_orig < b.media_start {
            bail!("moov precedes mdat (faststart layout) and the cut reached it — index lost");
        }
        // [0, mdat_hdr_orig): prosthetic structure
        out.extend(synth_prefix(mdat_hdr_orig)?);
        // [mdat_hdr_orig, media_start): fresh mdat header spanning to moov
        let mdat_size = b.moov_orig - mdat_hdr_orig;
        if mdat_size > u32::MAX as u64 {
            bail!("reconstructed mdat would need a 64-bit size; unsupported layout");
        }
        out.extend_from_slice(&(mdat_size as u32).to_be_bytes());
        out.extend_from_slice(b"mdat");
        if k > b.media_start {
            // [media_start, K): media that no longer exists. Zero-fill —
            // the decoder will hiss here; the clip stage cuts it away.
            out.resize(out.len() + (k - b.media_start) as usize, 0);
            out.extend_from_slice(data);
        } else {
            // Cut ended inside the mdat header; skip its orphaned fragment.
            out.extend_from_slice(&data[(b.media_start - k) as usize..]);
        }
    }
    Ok(out)
}

/// Shared synthetic-MP4 builders, used by unit tests here and in transplant.rs.
#[cfg(test)]
pub(crate) mod testutil {
    pub(crate) fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
        b.extend_from_slice(fourcc);
        b.extend_from_slice(body);
        b
    }

    /// Build a minimal but structurally honest moov with one video track.
    pub(crate) fn test_moov(chunk_offsets: &[u32], sizes: &[u32], syncs: &[u32]) -> Vec<u8> {
        let mut mdhd = vec![0u8; 12]; // version+flags, creation, modification
        mdhd.extend_from_slice(&30u32.to_be_bytes()); // timescale
        mdhd.extend_from_slice(&(sizes.len() as u32).to_be_bytes()); // duration
        mdhd.extend_from_slice(&[0; 4]);

        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0; 13]);

        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend(boxed(b"avc1", &[0u8; 70]));

        let mut stts = vec![0u8; 4];
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        stts.extend_from_slice(&1u32.to_be_bytes()); // delta 1 tick @30 = 1/30 s

        let mut stss = vec![0u8; 4];
        stss.extend_from_slice(&(syncs.len() as u32).to_be_bytes());
        for s in syncs {
            stss.extend_from_slice(&s.to_be_bytes());
        }

        let mut stsc = vec![0u8; 4];
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes()); // first_chunk 1
        let per = (sizes.len() as u32).div_ceil(chunk_offsets.len() as u32);
        stsc.extend_from_slice(&per.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());

        let mut stsz = vec![0u8; 4];
        stsz.extend_from_slice(&0u32.to_be_bytes()); // non-uniform
        stsz.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        for s in sizes {
            stsz.extend_from_slice(&s.to_be_bytes());
        }

        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&(chunk_offsets.len() as u32).to_be_bytes());
        for c in chunk_offsets {
            stco.extend_from_slice(&c.to_be_bytes());
        }

        let mut stbl = Vec::new();
        for (fc, body) in [
            (b"stsd", &stsd),
            (b"stts", &stts),
            (b"stss", &stss),
            (b"stsc", &stsc),
            (b"stsz", &stsz),
            (b"stco", &stco),
        ] {
            stbl.extend(boxed(fc, body));
        }
        let minf = boxed(b"stbl", &stbl);
        let mut mdia = boxed(b"mdhd", &mdhd);
        mdia.extend(boxed(b"hdlr", &hdlr));
        mdia.extend(boxed(b"minf", &minf));
        let trak = boxed(b"mdia", &mdia);
        boxed(b"moov", &boxed(b"trak", &trak))
    }

    /// One length-prefixed NAL filling `size` bytes. The first payload byte
    /// has its MSB set — first_mb_in_slice == 0 — so slice NALs read as the
    /// start of an access unit (the transplant walker depends on this).
    pub(crate) fn nal_sample(size: u32, nal_type: u8, fill: u8) -> Vec<u8> {
        let mut s = (size - 4).to_be_bytes().to_vec();
        s.push(nal_type);
        s.resize(size as usize, fill);
        if size >= 6 {
            s[5] = 0x80;
        }
        s
    }

    /// Assemble a synthetic original mp4: ftyp(32) + free(8) + mdat + moov.
    pub(crate) fn synthetic_original() -> (Vec<u8>, Vec<u32>) {
        let sizes: Vec<u32> = vec![900, 40, 44, 38, 880, 42, 46, 36];
        let mut ftyp = Vec::new();
        ftyp.extend_from_slice(&32u32.to_be_bytes());
        ftyp.extend_from_slice(b"ftypisom");
        ftyp.extend_from_slice(&0x200u32.to_be_bytes());
        ftyp.extend_from_slice(b"isomiso2avc1mp41");
        let free = boxed(b"free", &[]);

        let mut media = Vec::new();
        for (i, &s) in sizes.iter().enumerate() {
            let nal_type = if i % 4 == 0 { 0x65 } else { 0x41 }; // IDR every 4
            media.extend(nal_sample(s, nal_type, i as u8));
        }
        let media_start = (ftyp.len() + free.len() + 8) as u32;
        let mdat = boxed(b"mdat", &media);
        let moov = test_moov(&[media_start], &sizes, &[1, 5]);

        let mut file = ftyp;
        file.extend(free);
        file.extend(mdat);
        file.extend(moov);
        (file, sizes)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn intact_file_is_intact() {
        let (file, _) = synthetic_original();
        assert!(matches!(analyze(&file), Analysis::Intact));
    }

    #[test]
    fn head_truncated_ftyp_only_recovers_via_mdat_anchor() {
        let (file, _) = synthetic_original();
        let cut = 32; // exactly the ftyp
        let head_truncated = &file[cut..];
        match analyze(head_truncated) {
            Analysis::HeadTruncated(b) => {
                assert_eq!(b.cut_bytes, cut as u64);
                assert_eq!(b.method, "mdat header anchor");
                assert_eq!(b.media_bytes_lost, 0);
                let rebuilt = reconstruct(head_truncated, &b).unwrap();
                assert_eq!(rebuilt.len(), file.len());
                // Media region and moov must be byte-identical to the original.
                assert_eq!(&rebuilt[48..], &file[48..]);
                assert!(matches!(analyze(&rebuilt), Analysis::Intact));
            }
            other => panic!("expected HeadTruncated, got {other:?}"),
        }
    }

    #[test]
    fn head_truncated_through_mdat_header_recovers_via_correlation() {
        let (file, sizes) = synthetic_original();
        let cut = 44; // ftyp(32) + free(8) + half the mdat header
        let head_truncated = &file[cut..];
        match analyze(head_truncated) {
            Analysis::HeadTruncated(b) => {
                assert_eq!(b.cut_bytes, cut as u64);
                assert_eq!(b.method, "stsz/NAL-chain correlation");
                assert_eq!(b.media_bytes_lost, 0);
                let rebuilt = reconstruct(head_truncated, &b).unwrap();
                assert_eq!(rebuilt.len(), file.len());
                assert_eq!(&rebuilt[48..], &file[48..]);
                assert!(matches!(analyze(&rebuilt), Analysis::Intact));
                let _ = sizes;
            }
            other => panic!("expected HeadTruncated, got {other:?}"),
        }
    }

    #[test]
    fn head_truncated_into_media_reports_damage_and_first_clean_keyframe() {
        let (file, sizes) = synthetic_original();
        let cut = 48 + 950; // into the media: kills sample 1 (900) + part of 2
        let head_truncated = &file[cut..];
        match analyze(head_truncated) {
            Analysis::HeadTruncated(b) => {
                assert_eq!(b.cut_bytes, cut as u64);
                assert_eq!(b.media_bytes_lost, 950);
                assert_eq!(b.damaged_keyframes, 1); // sync sample 1 died
                // First clean keyframe is sample 5 (index 4) at t = 4/30.
                let t = b.first_clean_keyframe_time.unwrap();
                assert!((t - 4.0 / 30.0).abs() < 1e-9, "t = {t}");
                let rebuilt = reconstruct(head_truncated, &b).unwrap();
                assert_eq!(rebuilt.len(), file.len());
                // Everything from the cut onward is preserved verbatim.
                assert_eq!(&rebuilt[cut..], &file[cut..]);
                // The destroyed media region is zero-filled silence.
                assert!(rebuilt[48 + 8..cut].iter().all(|&x| x == 0));
                assert!(matches!(analyze(&rebuilt), Analysis::Intact));
                let _ = sizes;
            }
            other => panic!("expected HeadTruncated, got {other:?}"),
        }
    }

    #[test]
    fn no_moov_is_honest_about_it() {
        let (file, _) = synthetic_original();
        // Cut from the back instead: lose the moov.
        let headless = &file[..file.len() - 200];
        assert!(matches!(analyze(headless), Analysis::NoMoov));
    }
}
