// SPDX-License-Identifier: GPL-3.0-or-later
//! Matroska/WebM analysis and re-heading. Walks surviving clusters, sniffs each
//! track's codec from frame bytes, and synthesizes a fresh container head
//! (EBML + Segment + Info + Tracks) — copying clusters verbatim, never decoding.
// Consumers wired in rescue.rs (a later task); suppress dead-code until then.
#![allow(dead_code)]

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::ebml::{self, read_element};
use crate::mkv_codecs::{self, Codec};

/// One sampled block: which track, whether flagged a keyframe, and the first
/// (un-laced) frame's bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct SampledBlock {
    pub track: u64,
    pub keyframe: bool,
    pub frame: Vec<u8>,
}

/// Walk one cluster at `pos`, invoking `f` for each SimpleBlock. Only unlaced
/// blocks are reported (laced blocks are skipped for sampling; they are still
/// copied verbatim during reconstruction). Returns the offset just past the
/// cluster, or `None` if the header is malformed.
pub fn walk_cluster(data: &[u8], pos: usize, mut f: impl FnMut(SampledBlock)) -> Option<usize> {
    let cluster = read_element(data, pos)?;
    if cluster.id != ebml::ID_CLUSTER {
        return None;
    }
    let end = match cluster.size {
        Some(s) => (cluster.data_pos + s as usize).min(data.len()),
        None => data.len(),
    };
    let mut p = cluster.data_pos;
    while p < end {
        let e = read_element(data, p)?;
        let body_end = match e.size {
            Some(s) => e.data_pos + s as usize,
            None => break, // unknown-size child inside a cluster: stop
        };
        if body_end > data.len() {
            break;
        }
        if e.id == ebml::ID_SIMPLE_BLOCK
            && let Some(block) = parse_simpleblock(&data[e.data_pos..body_end])
        {
            f(block);
        }
        // Timecode, BlockGroup, etc. are skipped for sampling.
        p = body_end;
    }
    Some(end)
}

/// Parse a SimpleBlock payload: `[track vint][int16 rel timecode][flags][frame]`.
/// Returns `None` for laced blocks (lacing bits `0x06` set) — sampling only
/// needs unlaced frames.
fn parse_simpleblock(buf: &[u8]) -> Option<SampledBlock> {
    let (track, tlen) = ebml::read_vint(buf, 0)?;
    let mut p = tlen + 2; // skip int16 relative timecode
    let flags = *buf.get(p)?;
    p += 1;
    if flags & 0x06 != 0 {
        return None; // laced
    }
    Some(SampledBlock {
        track,
        keyframe: flags & 0x80 != 0,
        frame: buf.get(p..)?.to_vec(),
    })
}

/// Collect up to `per_track` sample frames per track, scanning clusters from
/// `first_cluster` forward until every seen track has enough samples or the
/// data runs out.
pub fn sample_frames(
    data: &[u8],
    first_cluster: usize,
    per_track: usize,
) -> BTreeMap<u64, Vec<(bool, Vec<u8>)>> {
    let mut samples: BTreeMap<u64, Vec<(bool, Vec<u8>)>> = BTreeMap::new();
    let mut pos = first_cluster;
    let mut clusters_scanned = 0;
    while pos < data.len() && clusters_scanned < 8 {
        let Some(next) = walk_cluster(data, pos, |b| {
            let entry = samples.entry(b.track).or_default();
            if entry.len() < per_track {
                entry.push((b.keyframe, b.frame));
            }
        }) else {
            break;
        };
        clusters_scanned += 1;
        if next <= pos {
            break;
        }
        pos = next;
    }
    samples
}

/// One reconstructed track: its block number and sniffed codec.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub number: u64,
    pub codec: Codec,
}

/// A heavy beheading: the head is gone, but clusters survive from `first_cluster`.
#[derive(Debug, Clone, PartialEq)]
pub struct Heavy {
    /// Offset of the first verified surviving cluster.
    pub first_cluster: usize,
    /// Offset of the first cluster led by a true video keyframe — the clean
    /// playback start. `None` if no keyframe-led cluster was found in the scan
    /// window (then playback must start mid-GOP and may show leading artifacts).
    pub first_keyframe_cluster: Option<usize>,
    pub tracks: Vec<Track>,
    /// Bytes discarded before the chosen playback start.
    pub bytes_lost: usize,
}

/// What `analyze` concluded about a buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum Analysis {
    /// Valid EBML magic at offset 0 — not a beheading; leave it to ffmpeg.
    Intact,
    /// Only clusters survive; the `Tracks` element must be reconstructed.
    Heavy(Heavy),
    /// No usable Matroska structure found.
    NoStructure,
}

/// True if `pos` begins a cluster whose first child is a plausible Timecode or
/// block — guards against the 4-byte cluster ID appearing inside frame data.
fn validate_cluster(data: &[u8], pos: usize) -> bool {
    let Some(cluster) = read_element(data, pos) else {
        return false;
    };
    if cluster.id != ebml::ID_CLUSTER {
        return false;
    }
    match read_element(data, cluster.data_pos) {
        Some(child) => matches!(
            child.id,
            ebml::ID_TIMECODE | ebml::ID_SIMPLE_BLOCK | ebml::ID_BLOCK_GROUP
        ),
        None => false,
    }
}

/// Find the first offset that begins a verified cluster.
fn find_first_cluster(data: &[u8]) -> Option<usize> {
    const SIG: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
    let mut i = 0;
    while i + 4 <= data.len() {
        if data[i..i + 4] == SIG && validate_cluster(data, i) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The block number of the first video track, if any.
fn video_track(tracks: &[Track]) -> Option<u64> {
    tracks
        .iter()
        .find(|t| {
            matches!(
                t.codec,
                Codec::Vp9 { .. } | Codec::Vp8 { .. } | Codec::Av1 { .. } | Codec::H264 { .. }
            )
        })
        .map(|t| t.number)
}

/// First cluster (at/after `start`) whose first `video_track` block is a true
/// keyframe — the clean playback start. With no video track, audio frames are
/// all keyframes, so `start` itself is clean. Plan 1 detects keyframes via the
/// VP9 header; Plan 3 generalizes this per codec.
fn find_keyframe_cluster(data: &[u8], start: usize, video_track: Option<u64>) -> Option<usize> {
    let Some(vt) = video_track else {
        return Some(start);
    };
    let mut pos = start;
    let mut scanned = 0;
    while pos < data.len() && scanned < 32 {
        let mut found_video = false;
        let mut is_keyframe = false;
        let next = walk_cluster(data, pos, |b| {
            if b.track == vt && !found_video {
                found_video = true;
                is_keyframe = mkv_codecs::vp9_dims(&b.frame).is_some();
            }
        })?;
        if found_video && is_keyframe {
            return Some(pos);
        }
        scanned += 1;
        if next <= pos {
            break;
        }
        pos = next;
    }
    None
}

/// Diagnose a buffer that `mp4::analyze` could not place (no moov).
pub fn analyze(data: &[u8]) -> Analysis {
    if data.len() >= 4 && data[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Analysis::Intact;
    }
    let Some(first) = find_first_cluster(data) else {
        return Analysis::NoStructure;
    };
    let samples = sample_frames(data, first, 6);
    if samples.is_empty() {
        return Analysis::NoStructure;
    }
    let tracks: Vec<Track> = samples
        .into_iter()
        .map(|(number, frames)| Track {
            number,
            codec: mkv_codecs::sniff(&frames),
        })
        .collect();
    let first_keyframe_cluster = find_keyframe_cluster(data, first, video_track(&tracks));
    let bytes_lost = first_keyframe_cluster.unwrap_or(first);
    Analysis::Heavy(Heavy {
        first_cluster: first,
        first_keyframe_cluster,
        tracks,
        bytes_lost,
    })
}

/// True if every track's codec is legal in a `.webm` (so we pick DocType "webm";
/// otherwise "matroska"). Plan 1 only ever produces VP9/Opus, but this keeps the
/// DocType honest as codecs are added.
pub fn all_webm_legal(tracks: &[Track]) -> bool {
    tracks.iter().all(|t| {
        matches!(
            t.codec,
            Codec::Vp9 { .. } | Codec::Vp8 { .. } | Codec::Av1 { .. } | Codec::Opus { .. }
        )
    })
}

/// Synthesize the container head: EBML header + Segment(unknown size) + Info +
/// Tracks. Errors if any track still needs a donor.
pub fn build_head(tracks: &[Track], doctype: &str) -> Result<Vec<u8>> {
    for t in tracks {
        if let Codec::NeedsDonor { hint } = &t.codec {
            bail!("track {} cannot be re-headed without a donor ({hint})", t.number);
        }
    }

    let ebml_hdr = ebml::el(
        ebml::ID_EBML,
        &[
            ebml::uint(ebml::ID_EBML_VERSION, 1),
            ebml::uint(ebml::ID_EBML_READ_VERSION, 1),
            ebml::uint(ebml::ID_EBML_MAX_ID_LEN, 4),
            ebml::uint(ebml::ID_EBML_MAX_SIZE_LEN, 8),
            ebml::ebml_string(ebml::ID_DOCTYPE, doctype),
            ebml::uint(ebml::ID_DOCTYPE_VERSION, 2),
            ebml::uint(ebml::ID_DOCTYPE_READ_VERSION, 2),
        ]
        .concat(),
    );

    let info = ebml::el(
        ebml::ID_INFO,
        &[
            ebml::uint(ebml::ID_TIMECODE_SCALE, 1_000_000),
            ebml::ebml_string(ebml::ID_MUXING_APP, "basinski"),
            ebml::ebml_string(ebml::ID_WRITING_APP, "basinski"),
        ]
        .concat(),
    );

    let mut track_elements = Vec::new();
    for t in tracks {
        track_elements.extend(mkv_codecs::track_entry(t.number, &t.codec));
    }
    let tracks_el = ebml::el(ebml::ID_TRACKS, &track_elements);

    // Segment with the 8-byte unknown-size sentinel: ffmpeg reads children until
    // EOF, which is what we want (we don't know the post-clip length yet).
    let segment_open = {
        let mut v = ebml::el(ebml::ID_SEGMENT, &[]); // produces [id][0x80]
        v.truncate(v.len() - 1); // drop the 0-length size byte
        v.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        v
    };

    let mut head = ebml_hdr;
    head.extend(segment_open);
    head.extend(info);
    head.extend(tracks_el);
    Ok(head)
}

/// Full reconstructed file bytes: synthesized head followed by the surviving
/// clusters from `start_offset` to EOF, copied verbatim. Pure — no ffmpeg, no
/// filesystem. `start_offset` is a cluster boundary chosen by the caller (the
/// first keyframe-led cluster for a clean start, or `first_cluster` for
/// `--no-clip`).
pub fn reconstruct(data: &[u8], h: &Heavy, start_offset: usize) -> Result<Vec<u8>> {
    let doctype = if all_webm_legal(&h.tracks) { "webm" } else { "matroska" };
    let mut out = build_head(&h.tracks, doctype)?;
    out.extend_from_slice(&data[start_offset..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ebml::{el, uint, ID_CLUSTER, ID_TIMECODE};

    /// Build a synthetic SimpleBlock element: track 1, unlaced, given keyframe
    /// flag and frame bytes.
    fn simple_block(track: u64, key: bool, frame: &[u8]) -> Vec<u8> {
        let mut body = ebml::write_vint(track);
        body.extend_from_slice(&[0x00, 0x00]); // rel timecode
        body.push(if key { 0x80 } else { 0x00 }); // flags
        body.extend_from_slice(frame);
        el(ebml::ID_SIMPLE_BLOCK, &body)
    }

    #[test]
    fn walks_cluster_blocks() {
        let mut cluster_body = uint(ID_TIMECODE, 1000);
        cluster_body.extend(simple_block(1, true, b"VIDEO"));
        cluster_body.extend(simple_block(2, false, b"AUDIO"));
        let cluster = el(ID_CLUSTER, &cluster_body);

        let mut got = Vec::new();
        let end = walk_cluster(&cluster, 0, |b| got.push(b));
        assert_eq!(end, Some(cluster.len()));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], SampledBlock { track: 1, keyframe: true, frame: b"VIDEO".to_vec() });
        assert_eq!(got[1], SampledBlock { track: 2, keyframe: false, frame: b"AUDIO".to_vec() });
    }

    #[test]
    fn samples_frames_across_a_cluster() {
        let mut cluster_body = uint(ID_TIMECODE, 0);
        cluster_body.extend(simple_block(1, true, b"K"));
        cluster_body.extend(simple_block(1, false, b"P"));
        let cluster = el(ID_CLUSTER, &cluster_body);

        let s = sample_frames(&cluster, 0, 4);
        assert_eq!(s.len(), 1);
        assert_eq!(s[&1], vec![(true, b"K".to_vec()), (false, b"P".to_vec())]);
    }

    const VP9_KEYFRAME: &[u8] = &[
        0x82, 0x49, 0x83, 0x42, 0x40, 0x77, 0xF0, 0x43, 0x74, 0x18, 0x27, 0xA0,
    ];

    #[test]
    fn analyze_reports_intact_for_ebml_magic() {
        let buf = [0x1A, 0x45, 0xDF, 0xA3, 0x01, 0x02];
        assert!(matches!(analyze(&buf), Analysis::Intact));
    }

    #[test]
    fn analyze_reports_nostructure_for_garbage() {
        let buf = [0u8; 64];
        assert!(matches!(analyze(&buf), Analysis::NoStructure));
    }

    #[test]
    fn analyze_finds_heavy_beheading_with_vp9_and_opus() {
        // [8 bytes garbage][cluster: VP9 keyframe on track 1, Opus frame on track 2]
        let mut body = uint(ID_TIMECODE, 0);
        body.extend(simple_block(1, true, VP9_KEYFRAME));
        body.extend(simple_block(2, true, &[0xFC, 0xEA, 0x73]));
        let cluster = el(ID_CLUSTER, &body);

        let mut buf = vec![0xAB; 8];
        let cluster_off = buf.len();
        buf.extend_from_slice(&cluster);

        let Analysis::Heavy(h) = analyze(&buf) else {
            panic!("expected Heavy");
        };
        assert_eq!(h.first_cluster, cluster_off);
        // The single cluster is led by a real VP9 keyframe, so it is also the
        // clean playback start.
        assert_eq!(h.first_keyframe_cluster, Some(cluster_off));
        assert_eq!(h.tracks.len(), 2);
        assert_eq!(h.tracks[0].number, 1);
        assert_eq!(h.tracks[0].codec, Codec::Vp9 { width: 1920, height: 1080 });
        assert_eq!(h.tracks[1].number, 2);
        assert_eq!(h.tracks[1].codec, Codec::Opus { channels: 2 });
    }

    #[test]
    fn build_head_emits_ebml_and_tracks() {
        let tracks = vec![
            Track { number: 1, codec: Codec::Vp9 { width: 1920, height: 1080 } },
            Track { number: 2, codec: Codec::Opus { channels: 2 } },
        ];
        let head = build_head(&tracks, "webm").unwrap();
        // Starts with EBML magic.
        assert_eq!(&head[0..4], &[0x1A, 0x45, 0xDF, 0xA3]);
        // Declares DocType webm and both codecs.
        assert!(window(&head, b"webm").is_some());
        assert!(window(&head, b"V_VP9").is_some());
        assert!(window(&head, b"A_OPUS").is_some());
        // Contains a Segment ID and a Tracks ID.
        assert!(window(&head, &[0x18, 0x53, 0x80, 0x67]).is_some());
        assert!(window(&head, &[0x16, 0x54, 0xAE, 0x6B]).is_some());
    }

    #[test]
    fn build_head_refuses_donor_only_track() {
        let tracks = vec![Track {
            number: 1,
            codec: Codec::NeedsDonor { hint: "x" },
        }];
        assert!(build_head(&tracks, "webm").is_err());
    }

    #[test]
    fn reconstruct_prepends_head_and_copies_clusters_verbatim() {
        let mut body = uint(ID_TIMECODE, 0);
        body.extend(simple_block(1, true, VP9_KEYFRAME));
        body.extend(simple_block(2, true, &[0xFC, 0xEA]));
        let cluster = el(ID_CLUSTER, &body);
        let mut buf = vec![0xAB; 8];
        buf.extend_from_slice(&cluster);

        let Analysis::Heavy(h) = analyze(&buf) else { panic!() };
        let out = reconstruct(&buf, &h, h.first_cluster).unwrap();
        // The tail of the output is the surviving clusters, byte-for-byte.
        assert!(out.ends_with(&cluster));
        assert_eq!(&out[0..4], &[0x1A, 0x45, 0xDF, 0xA3]);
    }

    fn window(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
