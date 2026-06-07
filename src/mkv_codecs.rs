// SPDX-License-Identifier: GPL-3.0-or-later
//! Codec identification and per-codec Matroska head synthesis for the
//! reconstructive (Tier 2) re-head. WebM-native codecs (VP8/VP9/AV1/Opus) are
//! parsed here; H.264-in-Matroska delegates to `h264.rs`/`divine.rs` (Plan 3).
// Items are consumed by matroska (Task 5); suppress dead-code lint until that
// module is added.
#![allow(dead_code)]

use crate::ebml::{self, el, uint};
use std::collections::BTreeSet;

/// What a track's surviving frames revealed about its codec.
#[derive(Debug, Clone, PartialEq)]
pub enum Codec {
    Vp9 { width: u32, height: u32 },
    Vp8 { width: u32, height: u32 },
    Av1 { width: u32, height: u32, config: Vec<u8> },
    Opus { channels: u8 },
    H264 { codec_private: Vec<u8>, width: u32, height: u32 },
    /// Parameters live in the lost `Tracks` and cannot be synthesized from
    /// frames (Vorbis codebooks, AAC AudioSpecificConfig) or could not be
    /// identified at all — needs a `--reference` donor.
    NeedsDonor { hint: &'static str },
}

/// Identify a track's codec from a sample of its frames. Each entry is
/// `(is_keyframe, frame_bytes)`. Plan 1 recognizes VP9 (video) and Opus (audio);
/// anything else returns `NeedsDonor`.
pub fn sniff(frames: &[(bool, Vec<u8>)]) -> Codec {
    // Video: a VP9 keyframe carries its own geometry. Try flagged keyframes
    // first, then any frame (Matroska keyframe flags can be unreliable).
    for &(key, ref f) in frames {
        if key && let Some((w, h)) = vp9_dims(f) {
            return Codec::Vp9 { width: w, height: h };
        }
    }
    for (_key, f) in frames {
        if let Some((w, h)) = vp9_dims(f) {
            return Codec::Vp9 { width: w, height: h };
        }
    }
    // Audio: Opus packets share a constant TOC config (top 5 bits) across a clip.
    // Matroska/WebM audio SimpleBlocks are ALWAYS keyframe-flagged, so require
    // at least one keyframe-flagged frame. VP9 inter frames (the false-positive
    // source) are never keyframe-flagged and happen to share a constant top-5-bit
    // value; VP9 detection above catches tracks that do have a keyframe, so this
    // guard closes the gap for inter-only samples.
    let has_keyframe = frames.iter().any(|(k, _)| *k);
    let configs: BTreeSet<u8> = frames
        .iter()
        .filter_map(|(_, f)| f.first().map(|b| b >> 3))
        .collect();
    if configs.len() == 1
        && has_keyframe
        && let Some((_, f0)) = frames.first()
        && let Some(&toc) = f0.first()
    {
        return Codec::Opus { channels: opus_channels(toc) };
    }
    Codec::NeedsDonor { hint: "unrecognized codec (Plan 1 handles VP9 + Opus)" }
}

/// Build a `TrackEntry` element for a sniffed codec. Panics on `NeedsDonor`
/// (callers must check and route to a donor before synthesizing a head).
pub fn track_entry(number: u64, codec: &Codec) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(uint(ebml::ID_TRACK_NUMBER, number));
    body.extend(uint(ebml::ID_TRACK_UID, number));
    match codec {
        Codec::Vp9 { width, height } => {
            body.extend(uint(ebml::ID_TRACK_TYPE, 1)); // video
            body.extend(ebml::ebml_string(ebml::ID_CODEC_ID, "V_VP9"));
            let video = [
                uint(ebml::ID_PIXEL_WIDTH, *width as u64),
                uint(ebml::ID_PIXEL_HEIGHT, *height as u64),
            ]
            .concat();
            body.extend(el(ebml::ID_VIDEO, &video));
        }
        Codec::Opus { channels } => {
            body.extend(uint(ebml::ID_TRACK_TYPE, 2)); // audio
            body.extend(ebml::ebml_string(ebml::ID_CODEC_ID, "A_OPUS"));
            body.extend(ebml::binary(ebml::ID_CODEC_PRIVATE, &opus_head(*channels)));
            let audio = [
                ebml::float32(ebml::ID_SAMPLING_FREQ, 48000.0),
                uint(ebml::ID_CHANNELS, *channels as u64),
            ]
            .concat();
            body.extend(el(ebml::ID_AUDIO, &audio));
        }
        Codec::NeedsDonor { .. } => {
            panic!("track_entry called on NeedsDonor — callers must route to a donor first")
        }
        other => panic!("track_entry: no synthesis arm yet for {other:?} (added in a later plan)"),
    }
    el(ebml::ID_TRACK_ENTRY, &body)
}

/// Big-endian MSB-first bit reader over a byte slice. Reads past the end yield 0.
struct BitReader<'a> {
    buf: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, bit: 0 }
    }
    fn read_bit(&mut self) -> u32 {
        let byte = self.buf.get(self.bit >> 3).copied().unwrap_or(0);
        let b = (byte >> (7 - (self.bit & 7))) & 1;
        self.bit += 1;
        b as u32
    }
    fn read_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_bit();
        }
        v
    }
}

/// Parse a VP9 uncompressed header and, for a keyframe, return `(width, height)`.
/// Returns `None` for non-VP9 data, inter frames, or `show_existing_frame`
/// (none of which carry frame size at the front). VP9 spec §6.2.
pub fn vp9_dims(frame: &[u8]) -> Option<(u32, u32)> {
    let mut r = BitReader::new(frame);
    if r.read_bits(2) != 0b10 {
        return None; // frame_marker
    }
    let profile_low = r.read_bit();
    let profile_high = r.read_bit();
    let profile = (profile_high << 1) | profile_low;
    if profile == 3 {
        r.read_bit(); // reserved_zero
    }
    if r.read_bit() == 1 {
        return None; // show_existing_frame: no header follows
    }
    if r.read_bit() != 0 {
        return None; // frame_type != KEY_FRAME
    }
    r.read_bit(); // show_frame
    r.read_bit(); // error_resilient_mode
    // frame_sync_code
    if r.read_bits(8) != 0x49 || r.read_bits(8) != 0x83 || r.read_bits(8) != 0x42 {
        return None;
    }
    // color_config
    if profile >= 2 {
        r.read_bit(); // ten_or_twelve_bit
    }
    let color_space = r.read_bits(3);
    if color_space != 7 {
        r.read_bit(); // color_range
        if profile == 1 || profile == 3 {
            r.read_bits(3); // subsampling_x, subsampling_y, reserved_zero
        }
    } else if profile == 1 || profile == 3 {
        r.read_bit(); // reserved_zero
    }
    // frame_size
    let w = r.read_bits(16) + 1;
    let h = r.read_bits(16) + 1;
    Some((w, h))
}

/// Opus channel count from the TOC byte's stereo flag (bit `0x04`). This matches
/// the standard 1- or 2-channel case; multistream layouts are out of scope.
pub fn opus_channels(toc: u8) -> u8 {
    if toc & 0x04 != 0 { 2 } else { 1 }
}

/// Build the 19-byte `OpusHead` for `CodecPrivate`. Opus always decodes at
/// 48 kHz; pre-skip 3840 matches libopus/ffmpeg defaults. RFC 7845 §5.1.
pub fn opus_head(channels: u8) -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1); // version
    h.push(channels); // output channel count
    h.extend_from_slice(&3840u16.to_le_bytes()); // pre-skip
    h.extend_from_slice(&48000u32.to_le_bytes()); // input sample rate (informational)
    h.extend_from_slice(&0i16.to_le_bytes()); // output gain
    h.push(0); // channel mapping family
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First bytes of a real VP9 keyframe from samples/trey-bermudan-broke2.webm
    /// (cluster @ 622530). Encodes 1920x1080.
    const VP9_KEYFRAME: &[u8] = &[
        0x82, 0x49, 0x83, 0x42, 0x40, 0x77, 0xF0, 0x43, 0x74, 0x18, 0x27, 0xA0,
    ];

    #[test]
    fn vp9_dims_reads_1080p_keyframe() {
        assert_eq!(vp9_dims(VP9_KEYFRAME), Some((1920, 1080)));
    }

    #[test]
    fn vp9_dims_rejects_inter_frame() {
        // 0x86 -> frame_marker 10, but frame_type bit = 1 (inter): no size here.
        assert_eq!(vp9_dims(&[0x86, 0x00, 0x40, 0x96, 0xA8]), None);
    }

    #[test]
    fn vp9_dims_rejects_non_vp9() {
        assert_eq!(vp9_dims(&[0x00, 0x00, 0x00, 0x01, 0x67]), None);
    }

    #[test]
    fn opus_channels_from_toc_stereo_bit() {
        assert_eq!(opus_channels(0xFC), 2); // stereo bit (0x04) set
        assert_eq!(opus_channels(0xF8), 1); // stereo bit clear
    }

    #[test]
    fn opus_head_is_19_bytes() {
        let h = opus_head(2);
        assert_eq!(h.len(), 19);
        assert_eq!(&h[0..8], b"OpusHead");
        assert_eq!(h[8], 1); // version
        assert_eq!(h[9], 2); // channel count
        assert_eq!(&h[12..16], &48000u32.to_le_bytes()); // input sample rate
        assert_eq!(h[18], 0); // channel mapping family
    }

    #[test]
    fn sniff_identifies_vp9_from_keyframe() {
        let frames = vec![(true, VP9_KEYFRAME.to_vec())];
        assert_eq!(sniff(&frames), Codec::Vp9 { width: 1920, height: 1080 });
    }

    #[test]
    fn sniff_identifies_opus_from_consistent_toc() {
        // Audio frames with a constant TOC config (top 5 bits) -> Opus.
        let frames = vec![
            (true, vec![0xFC, 0xEA, 0x73]),
            (true, vec![0xFC, 0x13, 0xFE]),
            (true, vec![0xFC, 0x01, 0x0A]),
        ];
        assert_eq!(sniff(&frames), Codec::Opus { channels: 2 });
    }

    #[test]
    fn sniff_unknown_needs_donor() {
        let frames = vec![(false, vec![0xDE, 0xAD, 0xBE, 0xEF])];
        assert!(matches!(sniff(&frames), Codec::NeedsDonor { .. }));
    }

    #[test]
    fn track_entry_for_vp9_has_codec_id_and_dims() {
        let bytes = track_entry(1, &Codec::Vp9 { width: 1920, height: 1080 });
        // Contains the CodecID string and a TrackType=1 (video).
        assert!(find(&bytes, b"V_VP9").is_some());
        assert!(find(&bytes, &crate::ebml::uint(crate::ebml::ID_PIXEL_WIDTH, 1920)).is_some());
        assert!(find(&bytes, &crate::ebml::uint(crate::ebml::ID_TRACK_TYPE, 1)).is_some());
    }

    #[test]
    fn track_entry_for_opus_has_opus_head() {
        let bytes = track_entry(2, &Codec::Opus { channels: 2 });
        assert!(find(&bytes, b"A_OPUS").is_some());
        assert!(find(&bytes, b"OpusHead").is_some());
        assert!(find(&bytes, &crate::ebml::uint(crate::ebml::ID_TRACK_TYPE, 2)).is_some());
    }

    #[test]
    fn sniff_rejects_unflagged_constant_config_as_opus() {
        // Non-keyframe frames sharing a constant top-5-bit value look like a VP9
        // inter-frame run, not Opus (audio frames are always keyframe-flagged).
        let frames = vec![
            (false, vec![0xFC, 0x11, 0x22]),
            (false, vec![0xFC, 0x33, 0x44]),
        ];
        assert!(matches!(sniff(&frames), Codec::NeedsDonor { .. }));
    }

    /// Tiny substring search for assertions.
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
