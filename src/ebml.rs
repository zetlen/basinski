// SPDX-License-Identifier: GPL-3.0-or-later
//! EBML primitives: variable-length integers, element headers, and builders for
//! synthesizing a Matroska head. Hand-rolled on purpose; no ffmpeg.

/// Read an EBML variable-length integer (data size) at `pos`.
/// Returns `(value_without_marker, length_in_bytes)`, or `None` if malformed.
/// An all-ones value (the "unknown size" sentinel) is returned as its raw value;
/// callers that care use [`read_element`], which maps it to `size: None`.
pub fn read_vint(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let first = *buf.get(pos)?;
    if first == 0 {
        return None; // length > 8 is not supported / illegal lead byte
    }
    let len = first.leading_zeros() as usize + 1; // 1..=8
    if pos + len > buf.len() {
        return None;
    }
    // len can be 8 (an 8-byte vint, lead byte 0x01); 0xFFu8 >> 8 would panic, so
    // widen to u16 — for len==8 the first byte contributes no value bits (mask 0).
    let mask = (0xFFu16 >> len) as u8;
    let mut val = (first & mask) as u64;
    for k in 1..len {
        val = (val << 8) | buf[pos + k] as u64;
    }
    Some((val, len))
}

/// Read an element ID at `pos`, preserving the length-marker bits so IDs compare
/// against their canonical constants (e.g. `Segment` == `0x18538067`).
pub fn read_id(buf: &[u8], pos: usize) -> Option<(u32, usize)> {
    let first = *buf.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1; // 1..=4 for valid IDs
    if len > 4 || pos + len > buf.len() {
        return None;
    }
    let mut id = 0u32;
    for k in 0..len {
        id = (id << 8) | buf[pos + k] as u32;
    }
    Some((id, len))
}

/// A parsed element header. `size: None` means an unknown-size element
/// (legal for `Segment` and `Cluster`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Element {
    pub id: u32,
    pub size: Option<u64>,
    /// Bytes consumed by the ID + size fields.
    pub header_len: usize,
    /// Absolute offset of the element's payload.
    pub data_pos: usize,
}

/// Read one element header at `pos`. Does not descend into children.
pub fn read_element(buf: &[u8], pos: usize) -> Option<Element> {
    let (id, idlen) = read_id(buf, pos)?;
    let (raw, szlen) = read_vint(buf, pos + idlen)?;
    let unknown = raw == (1u64 << (7 * szlen as u32)) - 1;
    Some(Element {
        id,
        size: if unknown { None } else { Some(raw) },
        header_len: idlen + szlen,
        data_pos: pos + idlen + szlen,
    })
}

// --- Element IDs (with their length-marker bits, as stored on the wire) ---
pub const ID_EBML: u32 = 0x1A45DFA3;
pub const ID_EBML_VERSION: u32 = 0x4286;
pub const ID_EBML_READ_VERSION: u32 = 0x42F7;
pub const ID_EBML_MAX_ID_LEN: u32 = 0x42F2;
pub const ID_EBML_MAX_SIZE_LEN: u32 = 0x42F3;
pub const ID_DOCTYPE: u32 = 0x4282;
pub const ID_DOCTYPE_VERSION: u32 = 0x4287;
pub const ID_DOCTYPE_READ_VERSION: u32 = 0x4285;

pub const ID_SEGMENT: u32 = 0x18538067;
#[allow(dead_code)] // Defined for completeness / future tiers
pub const ID_SEEKHEAD: u32 = 0x114D9B74;
pub const ID_INFO: u32 = 0x1549A966;
pub const ID_TIMECODE_SCALE: u32 = 0x2AD7B1;
pub const ID_MUXING_APP: u32 = 0x4D80;
pub const ID_WRITING_APP: u32 = 0x5741;

pub const ID_TRACKS: u32 = 0x1654AE6B;
pub const ID_TRACK_ENTRY: u32 = 0xAE;
pub const ID_TRACK_NUMBER: u32 = 0xD7;
pub const ID_TRACK_UID: u32 = 0x73C5;
pub const ID_TRACK_TYPE: u32 = 0x83;
pub const ID_CODEC_ID: u32 = 0x86;
pub const ID_CODEC_PRIVATE: u32 = 0x63A2;
pub const ID_VIDEO: u32 = 0xE0;
pub const ID_PIXEL_WIDTH: u32 = 0xB0;
pub const ID_PIXEL_HEIGHT: u32 = 0xBA;
pub const ID_AUDIO: u32 = 0xE1;
pub const ID_SAMPLING_FREQ: u32 = 0xB5;
pub const ID_CHANNELS: u32 = 0x9F;

pub const ID_CLUSTER: u32 = 0x1F43B675;
pub const ID_TIMECODE: u32 = 0xE7;
pub const ID_SIMPLE_BLOCK: u32 = 0xA3;
pub const ID_BLOCK_GROUP: u32 = 0xA0;
#[allow(dead_code)] // Defined for completeness / future tiers
pub const ID_BLOCK: u32 = 0xA1;

/// Minimal canonical byte length of an element ID (its marker is intrinsic).
fn id_bytes(id: u32) -> Vec<u8> {
    let len = match id {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    };
    id.to_be_bytes()[4 - len..].to_vec()
}

/// `[id][size][payload]` for an element with the given raw payload.
pub fn el(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = id_bytes(id);
    out.extend(write_vint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

/// An unsigned-integer element, big-endian, minimal length (>= 1 byte).
pub fn uint(id: u32, value: u64) -> Vec<u8> {
    let mut payload: Vec<u8> = value.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect();
    if payload.is_empty() {
        payload.push(0);
    }
    el(id, &payload)
}

/// A UTF-8 string element.
pub fn ebml_string(id: u32, s: &str) -> Vec<u8> {
    el(id, s.as_bytes())
}

/// A binary element (e.g. `CodecPrivate`).
pub fn binary(id: u32, data: &[u8]) -> Vec<u8> {
    el(id, data)
}

/// A 32-bit IEEE-754 float element.
pub fn float32(id: u32, value: f32) -> Vec<u8> {
    el(id, &value.to_be_bytes())
}

/// Encode `value` as a minimal-length EBML data-size vint.
pub fn write_vint(value: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        // Largest representable value at this length is (2^(7*len) - 1), which is
        // reserved as the "unknown size" sentinel — so real values must be < it.
        let cap = (1u64 << (7 * len)) - 1;
        if value < cap {
            let len = len as usize;
            let mut bytes = value.to_be_bytes()[8 - len..].to_vec();
            bytes[0] |= 0x80 >> (len - 1);
            return bytes;
        }
    }
    panic!("value {value} too large for an EBML vint");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_reads_one_byte() {
        assert_eq!(read_vint(&[0x81], 0), Some((1, 1)));
        assert_eq!(read_vint(&[0xFE], 0), Some((126, 1)));
    }

    #[test]
    fn vint_reads_two_bytes() {
        // 0x40 marks a 2-byte vint; value bits = 0x007F = 127.
        assert_eq!(read_vint(&[0x40, 0x7F], 0), Some((127, 2)));
        // 0x4001 -> value 1, length 2.
        assert_eq!(read_vint(&[0x40, 0x01], 0), Some((1, 2)));
    }

    #[test]
    fn vint_reads_eight_byte_without_panicking() {
        // Lead byte 0x01 marks an 8-byte vint; value bits are the low 7 bytes.
        assert_eq!(read_vint(&[0x01, 0, 0, 0, 0, 0, 0, 0x05], 0), Some((5, 8)));
    }

    #[test]
    fn vint_rejects_truncated_and_zero() {
        assert_eq!(read_vint(&[0x40], 0), None); // claims 2 bytes, only 1 present
        assert_eq!(read_vint(&[0x00], 0), None); // 0x00 is not a legal vint lead
    }

    #[test]
    fn id_preserves_marker() {
        assert_eq!(read_id(&[0x1A, 0x45, 0xDF, 0xA3], 0), Some((0x1A45DFA3, 4)));
        assert_eq!(read_id(&[0x18, 0x53, 0x80, 0x67], 0), Some((0x18538067, 4)));
        assert_eq!(read_id(&[0xA3], 0), Some((0xA3, 1)));
        assert_eq!(read_id(&[0x4D, 0x80], 0), Some((0x4D80, 2)));
    }

    #[test]
    fn write_vint_is_minimal_length() {
        assert_eq!(write_vint(0), vec![0x80]);
        assert_eq!(write_vint(1), vec![0x81]);
        assert_eq!(write_vint(126), vec![0xFE]);
        // 127 is the 1-byte unknown-size sentinel, so a real 127 needs 2 bytes.
        assert_eq!(write_vint(127), vec![0x40, 0x7F]);
        assert_eq!(write_vint(0x3FFE), vec![0x7F, 0xFE]);
    }

    #[test]
    fn vint_round_trips() {
        for v in [0u64, 1, 5, 126, 127, 300, 16383, 16384, 1_000_000] {
            let bytes = write_vint(v);
            assert_eq!(read_vint(&bytes, 0), Some((v, bytes.len())), "v={v}");
        }
    }

    #[test]
    fn reads_element_header() {
        // Info element (0x1549A966) with a 1-byte size of 5.
        let buf = [0x15, 0x49, 0xA9, 0x66, 0x85, 1, 2, 3, 4, 5];
        let e = read_element(&buf, 0).unwrap();
        assert_eq!(e.id, ID_INFO);
        assert_eq!(e.size, Some(5));
        assert_eq!(e.header_len, 5);
        assert_eq!(e.data_pos, 5);
    }

    #[test]
    fn reads_unknown_size_element() {
        // Segment (0x18538067) with the 1-byte unknown-size sentinel 0xFF.
        let buf = [0x18, 0x53, 0x80, 0x67, 0xFF];
        let e = read_element(&buf, 0).unwrap();
        assert_eq!(e.id, ID_SEGMENT);
        assert_eq!(e.size, None);
        assert_eq!(e.data_pos, 5);
    }

    #[test]
    fn builders_emit_canonical_bytes() {
        // el(): CodecID "V_VP9" -> id 0x86, size 0x85, payload.
        assert_eq!(el(ID_CODEC_ID, b"V_VP9"), vec![0x86, 0x85, b'V', b'_', b'V', b'P', b'9']);
        // uint(): PixelWidth 1920 = 0x0780 -> id 0xB0, size 0x82, big-endian value.
        assert_eq!(uint(ID_PIXEL_WIDTH, 1920), vec![0xB0, 0x82, 0x07, 0x80]);
        // uint() of zero is one zero byte, not empty.
        assert_eq!(uint(ID_CHANNELS, 0), vec![0x9F, 0x81, 0x00]);
        // float32(): 48000.0 -> id 0xB5, size 0x84, IEEE-754 big-endian.
        assert_eq!(
            float32(ID_SAMPLING_FREQ, 48000.0),
            {
                let mut v = vec![0xB5, 0x84];
                v.extend_from_slice(&48000.0f32.to_be_bytes());
                v
            }
        );
    }
}
