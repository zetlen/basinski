// SPDX-License-Identifier: GPL-3.0-or-later
//! Matroska/WebM analysis and re-heading. Walks surviving clusters, sniffs each
//! track's codec from frame bytes, and synthesizes a fresh container head
//! (EBML + Segment + Info + Tracks) — copying clusters verbatim, never decoding.
// Consumers wired in rescue.rs (a later task); suppress dead-code until then.
#![allow(dead_code)]

use std::collections::BTreeMap;

use crate::ebml::{self, read_element};

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
}
