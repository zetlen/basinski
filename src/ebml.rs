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
    let mut val = (first & (0xFF >> len)) as u64;
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
}
