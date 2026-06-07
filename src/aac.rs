// SPDX-License-Identifier: GPL-3.0-or-later
//! Raw-AAC salvage from the interleave gaps.
//!
//! When the moov dies, so does the audio sample table — the per-frame sizes
//! that say where each AAC access unit begins. What survives is the audio as
//! the muxer left it: chunks of raw AAC sitting in the gaps *between* the
//! video samples the transplant walker steps over.
//!
//! Raw AAC carries no sync word and no length field, so in principle you
//! cannot find frame boundaries without an AAC parser. But there is a back
//! door. AAC-LC frames from a given encoder all open with the same syntax
//! element — for stereo, a CPE (channel-pair element), whose first byte is a
//! near-constant `0x21`. That lets us recognize an audio chunk. And the
//! decoder will swallow a whole chunk of *several* concatenated frames if we
//! hand it one ADTS header whose length spans the lot: libavcodec decodes
//! raw_data_block after raw_data_block until the packet runs out. So we don't
//! have to split the frames at all — we wrap each gap in one synthetic ADTS
//! header and let the decoder walk the blocks. (The downstream mux re-encodes
//! to one-AU-per-sample, so the multi-block packets never reach a player.)

/// ADTS sampling_frequency_index for the rates consumer AAC actually uses.
fn sr_index(sample_rate: u32) -> Option<u8> {
    Some(match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        _ => return None,
    })
}

/// A 7-byte ADTS header (MPEG-4, AAC-LC, no CRC) for a `payload_len`-byte
/// audio payload. ADTS is self-describing — sample rate and channels live
/// here — so no AudioSpecificConfig (no donor audio track) is needed.
fn adts_header(payload_len: usize, sr_idx: u8, channels: u8) -> [u8; 7] {
    let frame_len = (payload_len + 7) as u32;
    [
        0xFF,
        0xF1, // syncword, MPEG-4, Layer 0, protection_absent
        (1 << 6) | (sr_idx << 2) | ((channels >> 2) & 1), // profile AAC-LC(1)
        (((channels & 3) << 6) | ((frame_len >> 11) & 3) as u8),
        ((frame_len >> 3) & 0xFF) as u8,
        (((frame_len & 7) << 5) as u8) | 0x1F,
        0xFC, // buffer fullness VBR, 1 frame in ADTS frame
    ]
}

pub struct Recovery {
    /// One ADTS-wrapped audio chunk per accepted gap, concatenated.
    pub adts: Vec<u8>,
    /// Gaps that looked like AAC and were wrapped.
    pub chunks: usize,
    /// Gaps skipped (too small, or not AAC — walker-miss contamination).
    pub skipped: usize,
}

/// True if the gap opens like an AAC-LC stereo frame (CPE element).
fn looks_like_aac(chunk: &[u8]) -> bool {
    // id_syn_ele == CPE (0b001) in the top 3 bits, i.e. byte 0x20..=0x3F with
    // the 0x20 bit set; encoders settle on 0x21 (CPE, instance 0, common
    // window). Accept the whole CPE-with-instance range to be forgiving.
    chunk.len() > 16 && (chunk[0] & 0xE0) == 0x20
}

/// Wrap each audio gap in an ADTS header. `gaps` are (offset, len) into
/// `media`. Returns None if no rate index exists or nothing looked like AAC.
pub fn recover(media: &[u8], gaps: &[(usize, usize)], sample_rate: u32) -> Option<Recovery> {
    let sr_idx = sr_index(sample_rate)?;
    let mut adts = Vec::new();
    let (mut chunks, mut skipped) = (0usize, 0usize);
    for &(off, len) in gaps {
        let chunk = &media[off..off + len];
        if looks_like_aac(chunk) {
            adts.extend_from_slice(&adts_header(len, sr_idx, 2));
            adts.extend_from_slice(chunk);
            chunks += 1;
        } else {
            skipped += 1;
        }
    }
    if chunks == 0 {
        return None;
    }
    Some(Recovery {
        adts,
        chunks,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adts_header_round_trips_length_and_rate() {
        let h = adts_header(1000, 4, 2); // 44.1k stereo, 1000-byte payload
        assert_eq!(h[0], 0xFF);
        assert_eq!(h[1] & 0xF6, 0xF0); // sync + layer
        // sampling_frequency_index back out of bits
        assert_eq!((h[2] >> 2) & 0x0F, 4);
        // channel_configuration
        assert_eq!(((h[3] >> 6) & 3) | ((h[2] & 1) << 2), 2);
        // aac_frame_length back out of the 13 split bits
        let len = (((h[3] & 3) as u32) << 11) | ((h[4] as u32) << 3) | ((h[5] as u32) >> 5);
        assert_eq!(len, 1007);
    }

    fn chunk(lead: u8, fill: u8, n: usize) -> Vec<u8> {
        let mut v = vec![lead];
        v.extend(std::iter::repeat_n(fill, n));
        v
    }

    #[test]
    fn carves_aac_gaps_and_skips_garbage() {
        // media: [aac chunk 0x21..][non-aac 0x99..][aac chunk]
        let mut media = Vec::new();
        let aac1 = chunk(0x21, 0x5A, 200);
        let junk = chunk(0x99, 0x00, 200);
        let aac2 = chunk(0x21, 0x3C, 150);
        let g1 = (media.len(), aac1.len());
        media.extend_from_slice(&aac1);
        let g2 = (media.len(), junk.len());
        media.extend_from_slice(&junk);
        let g3 = (media.len(), aac2.len());
        media.extend_from_slice(&aac2);

        let r = recover(&media, &[g1, g2, g3], 44100).unwrap();
        assert_eq!(r.chunks, 2);
        assert_eq!(r.skipped, 1);
        // each kept gap gained a 7-byte ADTS header
        assert_eq!(r.adts.len(), aac1.len() + aac2.len() + 14);
        assert_eq!(r.adts[0], 0xFF);
    }

    #[test]
    fn no_aac_means_none() {
        let media = vec![0u8; 500];
        assert!(recover(&media, &[(0, 500)], 44100).is_none());
    }

    #[test]
    fn unknown_rate_means_none() {
        let media = chunk(0x21, 0x5A, 200);
        assert!(recover(&media, &[(0, media.len())], 12345).is_none());
    }
}
