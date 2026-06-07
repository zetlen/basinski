// SPDX-License-Identifier: GPL-3.0-or-later
//! Forensic media identification.
//!
//! Most identifiers look at magic bytes at offset 0 and give up. Decayed media
//! rarely cooperates: headers get truncated, prepended with garbage, or
//! overwritten. The scanners here instead hunt for *codec-level* structure —
//! frame sync chains, NAL units, atom skeletons — anywhere in the file, the
//! way a tape restorer listens for the music underneath the hiss.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// What we think this is, e.g. "MP3 audio (MPEG-1 Layer III)".
    pub kind: String,
    /// "audio", "video", or "container".
    pub class: String,
    /// Byte offset of the first corroborating structure.
    pub offset: u64,
    /// 0.0..=1.0 — how sure we are.
    pub confidence: f64,
    /// Human-readable evidence trail.
    pub evidence: String,
}

/// Run every scanner over the buffer and return findings, best first.
pub fn identify(data: &[u8]) -> Vec<Finding> {
    let mut findings = Vec::new();

    findings.extend(scan_magic(data));
    findings.extend(scan_mp4_atoms(data));
    findings.extend(scan_mp3(data));
    findings.extend(scan_adts_aac(data));
    findings.extend(scan_mpegts(data));
    findings.extend(scan_h264_annexb(data));
    findings.extend(scan_avcc_lengths(data));
    findings.extend(scan_ebml(data));
    findings.extend(scan_ogg(data));
    findings.extend(scan_flac_frames(data));

    findings.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    findings
}

fn be32(d: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

// ---------------------------------------------------------------------------
// Intact magic at offset 0 — the easy case, checked first for completeness.
// ---------------------------------------------------------------------------

fn scan_magic(d: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();
    if d.len() < 12 {
        return out;
    }
    let hit = |kind: &str, class: &str, evidence: &str| Finding {
        kind: kind.into(),
        class: class.into(),
        offset: 0,
        confidence: 1.0,
        evidence: evidence.into(),
    };
    if &d[4..8] == b"ftyp" {
        let brand = String::from_utf8_lossy(&d[8..12]).into_owned();
        out.push(hit(
            &format!("ISO BMFF / MP4 (brand {brand})"),
            "container",
            "intact ftyp box at offset 0",
        ));
    } else if &d[0..4] == b"RIFF" && &d[8..12] == b"WAVE" {
        out.push(hit("WAV audio", "audio", "RIFF/WAVE header at offset 0"));
    } else if &d[0..4] == b"RIFF" && &d[8..11] == b"AVI" {
        out.push(hit("AVI video", "video", "RIFF/AVI header at offset 0"));
    } else if &d[0..3] == b"ID3" {
        out.push(hit(
            "MP3 audio (ID3-tagged)",
            "audio",
            "ID3v2 tag at offset 0",
        ));
    } else if &d[0..4] == b"fLaC" {
        out.push(hit("FLAC audio", "audio", "fLaC stream marker at offset 0"));
    } else if &d[0..4] == b"OggS" {
        out.push(hit("Ogg container", "container", "OggS page at offset 0"));
    } else if d[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        out.push(hit(
            "Matroska / WebM",
            "container",
            "EBML magic at offset 0",
        ));
    } else if &d[0..4] == b"FORM" && &d[8..12] == b"AIFF" {
        out.push(hit("AIFF audio", "audio", "FORM/AIFF header at offset 0"));
    }
    out
}

// ---------------------------------------------------------------------------
// MP4 atom skeleton — finds box structure even when the front of the file
// is gone. The moov box (and friends) are unmistakable.
// ---------------------------------------------------------------------------

pub(crate) const KNOWN_ATOMS: [&[u8; 4]; 12] = [
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"moof", b"mfra", b"udta", b"uuid",
    b"pnot", b"sidx",
];

#[derive(Debug, Clone, Copy)]
pub struct AtomHit {
    /// Offset of the size field (atom start).
    pub offset: u64,
    pub size: u64,
    pub fourcc: [u8; 4],
    /// Whether offset + size lands exactly on EOF or another known atom.
    pub chains: bool,
}

/// Scan the whole buffer for plausible top-level MP4 atoms.
pub fn scan_atoms(d: &[u8]) -> Vec<AtomHit> {
    let len = d.len() as u64;
    let mut hits = Vec::new();
    if d.len() < 8 {
        return hits;
    }
    for i in 4..d.len() - 3 {
        let fourcc: [u8; 4] = d[i..i + 4].try_into().unwrap();
        if !KNOWN_ATOMS.contains(&&fourcc) {
            continue;
        }
        let start = i - 4;
        let size32 = be32(d, start) as u64;
        // size == 1 means 64-bit largesize follows the fourcc;
        // size == 0 means "extends to EOF" (legal for a final mdat).
        let size = match size32 {
            0 => len - start as u64,
            1 => {
                if i + 12 > d.len() {
                    continue;
                }
                u64::from_be_bytes(d[i + 4..i + 12].try_into().unwrap())
            }
            s => s,
        };
        if size < 8 || start as u64 + size > len {
            continue;
        }
        let end = start as u64 + size;
        let chains = end == len
            || (end as usize + 8 <= d.len()
                && KNOWN_ATOMS
                    .contains(&&d[end as usize + 4..end as usize + 8].try_into().unwrap()));
        hits.push(AtomHit {
            offset: start as u64,
            size,
            fourcc,
            chains,
        });
    }
    hits
}

fn scan_mp4_atoms(d: &[u8]) -> Vec<Finding> {
    let hits = scan_atoms(d);
    let chained: Vec<&AtomHit> = hits.iter().filter(|h| h.chains).collect();
    if chained.is_empty() {
        return vec![];
    }
    let has_moov = chained.iter().any(|h| &h.fourcc == b"moov");
    let has_mdat = chained.iter().any(|h| &h.fourcc == b"mdat");
    let starts_clean = hits.first().map(|h| h.offset == 0).unwrap_or(false);
    let mut confidence: f64 = 0.5;
    if has_moov {
        confidence += 0.3;
    }
    if has_mdat {
        confidence += 0.15;
    }
    let desc = chained
        .iter()
        .map(|h| {
            format!(
                "{}@{} ({} bytes)",
                String::from_utf8_lossy(&h.fourcc),
                h.offset,
                h.size
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let kind = if starts_clean {
        "ISO BMFF / MP4".to_string()
    } else {
        "ISO BMFF / MP4 (damaged front)".to_string()
    };
    vec![Finding {
        kind,
        class: "container".into(),
        offset: chained[0].offset,
        confidence: confidence.min(0.99),
        evidence: format!("self-consistent atom chain: {desc}"),
    }]
}

// ---------------------------------------------------------------------------
// MP3 — the classic self-synchronizing format. Each frame header predicts
// the next frame's position; a chain of agreeing headers is near-proof.
// ---------------------------------------------------------------------------

const MP3_BITRATES_V1L3: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const MP3_BITRATES_V2L3: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];
const MP3_RATES_V1: [u32; 4] = [44100, 48000, 32000, 0];
const MP3_RATES_V2: [u32; 4] = [22050, 24000, 16000, 0];
const MP3_RATES_V25: [u32; 4] = [11025, 12000, 8000, 0];

/// Parse an MPEG audio frame header at `i`; return (frame_len, description).
fn mp3_frame_at(d: &[u8], i: usize) -> Option<(usize, String)> {
    if i + 4 > d.len() {
        return None;
    }
    let h = be32(d, i);
    if h >> 21 & 0x7FF != 0x7FF {
        return None; // no sync
    }
    let version = (h >> 19) & 0b11; // 0=2.5, 2=2, 3=1
    let layer = (h >> 17) & 0b11; // 1=III, 2=II, 3=I
    let bitrate_idx = ((h >> 12) & 0xF) as usize;
    let rate_idx = ((h >> 10) & 0b11) as usize;
    let padding = (h >> 9) & 1;
    if version == 1 || layer == 0 || bitrate_idx == 0 || bitrate_idx == 15 || rate_idx == 3 {
        return None;
    }
    // Only Layer III matters for rescue purposes; II/I are vanishingly rare.
    if layer != 1 {
        return None;
    }
    let (bitrate, rate, samples_per_frame, vname) = match version {
        3 => (
            MP3_BITRATES_V1L3[bitrate_idx],
            MP3_RATES_V1[rate_idx],
            1152,
            "MPEG-1",
        ),
        2 => (
            MP3_BITRATES_V2L3[bitrate_idx],
            MP3_RATES_V2[rate_idx],
            576,
            "MPEG-2",
        ),
        _ => (
            MP3_BITRATES_V2L3[bitrate_idx],
            MP3_RATES_V25[rate_idx],
            576,
            "MPEG-2.5",
        ),
    };
    if bitrate == 0 || rate == 0 {
        return None;
    }
    let frame_len = samples_per_frame / 8 * bitrate * 1000 / rate + padding;
    if frame_len < 24 {
        return None;
    }
    Some((
        frame_len as usize,
        format!("{vname} Layer III, {bitrate} kbps, {rate} Hz"),
    ))
}

/// Count how many consecutive frames chain from position `i`.
fn chain_frames(
    d: &[u8],
    mut i: usize,
    max: usize,
    frame_at: impl Fn(&[u8], usize) -> Option<(usize, String)>,
) -> (usize, Option<String>) {
    let mut count = 0;
    let mut desc = None;
    while count < max {
        match frame_at(d, i) {
            Some((len, info)) => {
                desc.get_or_insert(info);
                i += len;
                count += 1;
                if i >= d.len() {
                    break;
                }
            }
            None => break,
        }
    }
    (count, desc)
}

fn scan_mp3(d: &[u8]) -> Vec<Finding> {
    // Step a byte at a time until we find a position where >= 4 frames chain.
    let mut i = 0;
    while i + 4 <= d.len() {
        if d[i] == 0xFF && d[i + 1] & 0xE0 == 0xE0 {
            let (count, desc) = chain_frames(d, i, 8, mp3_frame_at);
            if count >= 4 {
                let confidence = if count >= 8 {
                    0.95
                } else {
                    0.7 + count as f64 * 0.03
                };
                return vec![Finding {
                    kind: format!("MP3 audio ({})", desc.unwrap_or_default()),
                    class: "audio".into(),
                    offset: i as u64,
                    confidence,
                    evidence: format!("{count}+ consecutive frame headers chain from offset {i}"),
                }];
            }
        }
        i += 1;
    }
    vec![]
}

// ---------------------------------------------------------------------------
// AAC in ADTS framing — same chaining trick, different header.
// ---------------------------------------------------------------------------

fn adts_frame_at(d: &[u8], i: usize) -> Option<(usize, String)> {
    if i + 7 > d.len() {
        return None;
    }
    if d[i] != 0xFF || d[i + 1] & 0xF6 != 0xF0 {
        return None; // sync 0xFFF + layer 00
    }
    let profile = (d[i + 2] >> 6) & 0b11;
    let freq_idx = (d[i + 2] >> 2) & 0xF;
    if freq_idx >= 13 {
        return None;
    }
    let frame_len =
        ((d[i + 3] as usize & 0x3) << 11) | (d[i + 4] as usize) << 3 | (d[i + 5] as usize >> 5);
    if frame_len < 7 {
        return None;
    }
    const FREQS: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    const PROFILES: [&str; 4] = ["Main", "LC", "SSR", "LTP"];
    Some((
        frame_len,
        format!(
            "AAC {} {} Hz",
            PROFILES[profile as usize], FREQS[freq_idx as usize]
        ),
    ))
}

fn scan_adts_aac(d: &[u8]) -> Vec<Finding> {
    let mut i = 0;
    while i + 7 <= d.len() {
        if d[i] == 0xFF && d[i + 1] & 0xF6 == 0xF0 {
            let (count, desc) = chain_frames(d, i, 8, adts_frame_at);
            if count >= 4 {
                let confidence = if count >= 8 {
                    0.93
                } else {
                    0.65 + count as f64 * 0.03
                };
                return vec![Finding {
                    kind: format!("AAC audio in ADTS ({})", desc.unwrap_or_default()),
                    class: "audio".into(),
                    offset: i as u64,
                    confidence,
                    evidence: format!("{count}+ consecutive ADTS headers chain from offset {i}"),
                }];
            }
        }
        i += 1;
    }
    vec![]
}

// ---------------------------------------------------------------------------
// MPEG-TS — 0x47 sync byte at strict 188-byte cadence.
// ---------------------------------------------------------------------------

fn scan_mpegts(d: &[u8]) -> Vec<Finding> {
    const PKT: usize = 188;
    if d.len() < PKT * 5 {
        return vec![];
    }
    for start in 0..PKT.min(d.len()) {
        let mut count = 0;
        let mut i = start;
        while i < d.len() && d[i] == 0x47 {
            count += 1;
            i += PKT;
        }
        if count >= 5 {
            return vec![Finding {
                kind: "MPEG transport stream".into(),
                class: "container".into(),
                offset: start as u64,
                confidence: (0.5 + count as f64 * 0.05).min(0.97),
                evidence: format!("{count} sync bytes at 188-byte cadence from offset {start}"),
            }];
        }
    }
    vec![]
}

// ---------------------------------------------------------------------------
// H.264 Annex B — start codes 00 00 (00) 01 followed by sane NAL types.
// Seeing SPS+PPS+IDR is essentially a signed confession.
// ---------------------------------------------------------------------------

fn scan_h264_annexb(d: &[u8]) -> Vec<Finding> {
    // Random data — and especially MP4 index-table wreckage, which is full
    // of small big-endian integers — manufactures 00 00 01 sequences for
    // free. Demand full NAL-header consistency and a plausible payload
    // opening before counting anything.
    fn valid_nal_at(d: &[u8], hdr: usize) -> Option<u8> {
        let nal = *d.get(hdr)?;
        let ty = nal & 0x1F;
        let ref_idc = nal >> 5;
        if nal & 0x80 != 0 || !(1..=23).contains(&ty) {
            return None;
        }
        match ty {
            5 | 7 | 8 if ref_idc == 0 => return None, // IDR/SPS/PPS are references
            6 | 9..=12 if ref_idc != 0 => return None, // SEI/AUD/end/filler never are
            _ => {}
        }
        match ty {
            // Slices: a zero first payload byte means first_mb_in_slice has
            // 8+ leading Exp-Golomb zeros — not a thing in real streams.
            1..=5 if *d.get(hdr + 1)? == 0 => None,
            // SPS opens with a known profile_idc.
            7 if !matches!(
                *d.get(hdr + 1)?,
                66 | 77 | 88 | 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128
            ) =>
            {
                None
            }
            _ => Some(ty),
        }
    }

    let mut hits: Vec<(u64, u8)> = Vec::new();
    let mut i = 0;
    while i + 4 < d.len() && hits.len() < 64 {
        if d[i] == 0 && d[i + 1] == 0 && (d[i + 2] == 1 || (d[i + 2] == 0 && d[i + 3] == 1)) {
            let hdr = if d[i + 2] == 1 { i + 3 } else { i + 4 };
            if let Some(ty) = valid_nal_at(d, hdr) {
                hits.push((i as u64, ty));
                i = hdr;
            }
        }
        i += 1;
    }
    let sps = hits.iter().any(|&(_, t)| t == 7);
    let pps = hits.iter().any(|&(_, t)| t == 8);
    let idr = hits.iter().any(|&(_, t)| t == 5);
    // The Annex B signature: an SPS with a PPS start code right behind it.
    let sps_pps_adjacent = hits
        .windows(2)
        .any(|w| w[0].1 == 7 && w[1].1 == 8 && w[1].0 - w[0].0 < 128);
    if hits.len() >= 4 && (sps || idr) {
        let confidence = if sps_pps_adjacent && idr {
            0.96
        } else if sps && pps && idr {
            0.55 // all present but scattered — could be coincidence
        } else {
            0.5
        };
        return vec![Finding {
            kind: "H.264 elementary stream (Annex B)".into(),
            class: "video".into(),
            offset: hits.first().map(|&(o, _)| o).unwrap_or(0),
            confidence,
            evidence: format!(
                "{} start codes; SPS:{sps} PPS:{pps} IDR:{idr} SPS→PPS:{sps_pps_adjacent}",
                hits.len()
            ),
        }];
    }
    vec![]
}

// ---------------------------------------------------------------------------
// AVCC-style H.264 — what lives inside an MP4's mdat: 4-byte big-endian
// length prefixes, each followed by a valid NAL header. A chain of these
// means "this is the body of an MP4 whose head has been cut off".
// ---------------------------------------------------------------------------

/// From position `i`, count how many length-prefixed NAL units chain.
pub fn avcc_chain_len(d: &[u8], mut i: usize, max: usize) -> usize {
    let mut count = 0;
    while count < max && i + 5 <= d.len() {
        let len = be32(d, i) as usize;
        if len == 0 || len > d.len() - i - 4 {
            break;
        }
        let nal = d[i + 4];
        let nal_type = nal & 0x1F;
        if nal & 0x80 != 0 || !(1..=23).contains(&nal_type) {
            break;
        }
        i += 4 + len;
        count += 1;
    }
    count
}

fn scan_avcc_lengths(d: &[u8]) -> Vec<Finding> {
    // Sample a handful of alignments near the start; mdat payloads usually
    // begin within the first few hundred bytes of a head_truncated file.
    let limit = d.len().min(65536);
    let mut i = 0;
    while i + 5 <= limit {
        let count = avcc_chain_len(d, i, 16);
        if count >= 6 {
            return vec![Finding {
                kind: "H.264 in MP4 framing (length-prefixed NALs)".into(),
                class: "video".into(),
                offset: i as u64,
                confidence: (0.55 + count as f64 * 0.025).min(0.9),
                evidence: format!(
                    "{count} length-prefixed NAL units chain from offset {i} — \
                     looks like mdat payload from a head_truncated MP4"
                ),
            }];
        }
        i += 1;
    }
    vec![]
}

// ---------------------------------------------------------------------------
// EBML / Matroska — cluster IDs survive even when the EBML header doesn't.
// ---------------------------------------------------------------------------

fn scan_ebml(d: &[u8]) -> Vec<Finding> {
    const CLUSTER: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
    const SEGMENT: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
    let mut clusters = 0;
    let mut first = None;
    for i in 0..d.len().saturating_sub(4) {
        if d[i..i + 4] == CLUSTER || d[i..i + 4] == SEGMENT {
            first.get_or_insert(i as u64);
            clusters += 1;
            if clusters >= 2 {
                break;
            }
        }
    }
    if clusters > 0 && d.first() != Some(&0x1A) {
        return vec![Finding {
            kind: "Matroska / WebM (damaged front)".into(),
            class: "container".into(),
            offset: first.unwrap(),
            confidence: 0.6,
            evidence: format!(
                "EBML segment/cluster IDs found at offset {}",
                first.unwrap()
            ),
        }];
    }
    vec![]
}

// ---------------------------------------------------------------------------
// Ogg pages mid-file.
// ---------------------------------------------------------------------------

fn scan_ogg(d: &[u8]) -> Vec<Finding> {
    if d.len() >= 4 && &d[0..4] == b"OggS" {
        return vec![]; // already reported by scan_magic
    }
    let mut count = 0;
    let mut first = None;
    for i in 0..d.len().saturating_sub(4) {
        if &d[i..i + 4] == b"OggS" {
            first.get_or_insert(i as u64);
            count += 1;
            if count >= 3 {
                break;
            }
        }
    }
    if count >= 2 {
        return vec![Finding {
            kind: "Ogg container (damaged front)".into(),
            class: "container".into(),
            offset: first.unwrap(),
            confidence: 0.8,
            evidence: format!("{count}+ OggS page markers found mid-file"),
        }];
    }
    vec![]
}

// ---------------------------------------------------------------------------
// Bare FLAC frames — 14-bit sync 0b11111111111110.
// ---------------------------------------------------------------------------

fn scan_flac_frames(d: &[u8]) -> Vec<Finding> {
    if d.len() >= 4 && &d[0..4] == b"fLaC" {
        return vec![];
    }
    let mut count = 0;
    let mut first = None;
    for i in 0..d.len().saturating_sub(2) {
        if d[i] == 0xFF && d[i + 1] & 0xFC == 0xF8 {
            // blocking strategy bit may be 0 or 1; check reserved bits in next bytes
            if i + 4 < d.len() && d[i + 2] >> 4 != 0 {
                first.get_or_insert(i as u64);
                count += 1;
            }
        }
    }
    // FLAC sync is weak (14 bits); require a lot of hits and no stronger signal.
    if count >= 16 {
        return vec![Finding {
            kind: "possible bare FLAC frames".into(),
            class: "audio".into(),
            offset: first.unwrap(),
            confidence: 0.35,
            evidence: format!("{count} candidate FLAC frame syncs (weak signal)"),
        }];
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp3_frame_header_parses() {
        // MPEG-1 Layer III, 128 kbps, 44100 Hz, no padding: FF FB 90 00
        let d = [0xFF, 0xFB, 0x90, 0x00];
        let (len, desc) = mp3_frame_at(&d, 0).expect("valid header");
        assert_eq!(len, 417);
        assert!(desc.contains("128 kbps"));
    }

    #[test]
    fn mp3_chain_detected_after_garbage() {
        // Build 6 valid 417-byte frames preceded by junk that contains a
        // false sync (FF Ex) that does NOT chain.
        let mut data = vec![0x00, 0xFF, 0xE2, 0x11, 0x22, 0x33];
        for _ in 0..6 {
            let mut frame = vec![0xFF, 0xFB, 0x90, 0x00];
            frame.resize(417, 0xAB);
            data.extend_from_slice(&frame);
        }
        let findings = scan_mp3(&data);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].offset, 6);
        assert!(findings[0].confidence > 0.7);
    }

    #[test]
    fn atom_scan_finds_chained_atoms() {
        // free(16) + mdat(24) + moov(8, ends at EOF)
        let mut d = Vec::new();
        d.extend_from_slice(&16u32.to_be_bytes());
        d.extend_from_slice(b"free");
        d.extend_from_slice(&[0u8; 8]);
        d.extend_from_slice(&24u32.to_be_bytes());
        d.extend_from_slice(b"mdat");
        d.extend_from_slice(&[1u8; 16]);
        d.extend_from_slice(&8u32.to_be_bytes());
        d.extend_from_slice(b"moov");
        let hits = scan_atoms(&d);
        let chained: Vec<_> = hits.iter().filter(|h| h.chains).collect();
        assert_eq!(chained.len(), 3);
        assert_eq!(&chained[1].fourcc, b"mdat");
    }

    #[test]
    fn avcc_chain_counts() {
        // Three length-prefixed NALs: lengths 5, 3, 4.
        let mut d = Vec::new();
        for (len, nal_type) in [(5u32, 0x65u8), (3, 0x41), (4, 0x01)] {
            d.extend_from_slice(&len.to_be_bytes());
            d.push(nal_type);
            d.extend(std::iter::repeat_n(0xCC, len as usize - 1));
        }
        assert_eq!(avcc_chain_len(&d, 0, 10), 3);
        assert_eq!(avcc_chain_len(&d, 1, 10), 0);
    }
}
