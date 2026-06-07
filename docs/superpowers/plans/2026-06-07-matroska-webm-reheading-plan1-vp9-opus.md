# Matroska/WebM Re-heading — Plan 1: Foundation + Reconstructive VP9/Opus

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Teach `basinski rescue` to re-head a heavily-beheaded Matroska/WebM file whose `Tracks` element is gone but whose VP9 video + Opus audio clusters survive — by synthesizing a fresh container head from frame-derived parameters and copying the clusters verbatim. Fully rescues `samples/trey-bermudan-broke2.webm`.

**Architecture:** Three new modules mirroring the MP4 stack: `ebml.rs` (vint/element primitives + head builders), `mkv_codecs.rs` (codec sniff + per-codec head synthesis), `matroska.rs` (analyze + cluster walker + pure `reconstruct`). `rescue.rs`'s `NoMoov` arm routes Matroska beheadings to a new orchestrator that writes the reconstruction starting at the first true-keyframe-led cluster (clean playback from frame one, no ffmpeg clip), validates it through `ffx`, and stream-copy-remuxes. Reconstruction never decodes the codec — frames are copied byte-for-byte; ffmpeg only validates and laundering-remuxes.

**Tech Stack:** Rust (edition 2024), `anyhow`, embedded `#[cfg(test)]` unit tests, `tests/e2e.sh` (bash + ffmpeg/ffprobe). External deps quarantined in `ffx.rs`.

**Scope note:** This is plan 1 of 3. Plan 2 adds Tier-1 surgical re-head (EBML-header-only loss) and Tier-3 transplant (`--reference` donor for Vorbis/AAC). Plan 3 extends Tier-2 codecs (VP8, AV1, H.264-in-MKV). The `Codec` enum and `Analysis` types are defined here with all variants so later plans extend without churn. Spec: `docs/superpowers/specs/2026-06-07-matroska-webm-reheading-design.md`.

---

## File Structure

- **Create `src/ebml.rs`** — EBML byte primitives: `read_vint`, `read_id`, `read_element`, `write_vint`, element builders (`el`/`uint`/`ebml_string`/`binary`/`float32`), element ID constants. Pure, no ffmpeg.
- **Create `src/mkv_codecs.rs`** — `Codec` enum, `BitReader`, `vp9_dims`, `opus_channels`, `opus_head`, `sniff`, `track_entry`. Owns WebM-native bitstream parsing.
- **Create `src/matroska.rs`** — `Analysis`/`Heavy`/`Track` types, cluster walker, `sample_frames`, `analyze`, `build_head`, `reconstruct`. Pure analysis + reconstruction.
- **Modify `src/main.rs:8-16`** — add `mod ebml; mod matroska; mod mkv_codecs;`.
- **Modify `src/ffx.rs`** — add `remux_copy` (stream-copy into `.webm`/`.mkv`).
- **Modify `src/rescue.rs`** — route the `NoMoov` arm to `rescue_matroska` when `matroska::analyze` reports a beheading; add `rescue_matroska` orchestrator.
- **Modify `tests/e2e.sh`** — add a WebM VP9/Opus reconstructive round-trip case, guarded on `libvpx-vp9`+`libopus`.
- **Modify `README.md`** — short Matroska/WebM ladder note + honest-limitation line.

---

## Task 1: EBML scaffold — vint/ID readers and writer

**Files:**
- Create: `src/ebml.rs`
- Modify: `src/main.rs:8-16`

- [ ] **Step 1: Declare the new modules in main.rs**

In `src/main.rs`, the module block is at lines 8-16. Add **only** `mod ebml;` for now (alphabetical, matching existing style). The `mkv_codecs` and `matroska` declarations are added in Tasks 3 and 5, when those files are created — declaring a `mod` for a file that doesn't exist yet would break the build.

```rust
mod aac;
mod divine;
mod ebml;
mod ffx;
mod forensics;
mod gestalt;
mod h264;
mod mp4;
mod rescue;
mod transplant;
```

- [ ] **Step 2: Write the failing tests**

Create `src/ebml.rs` with only the test module and empty function stubs so it compiles but fails:

```rust
// SPDX-License-Identifier: GPL-3.0-or-later
//! EBML primitives: variable-length integers, element headers, and builders for
//! synthesizing a Matroska head. Hand-rolled on purpose; no ffmpeg.

/// Read an EBML variable-length integer (data size) at `pos`.
/// Returns `(value_without_marker, length_in_bytes)`, or `None` if malformed.
/// An all-ones value (the "unknown size" sentinel) is returned as its raw value;
/// callers that care use [`read_element`], which maps it to `size: None`.
pub fn read_vint(_buf: &[u8], _pos: usize) -> Option<(u64, usize)> {
    todo!()
}

/// Read an element ID at `pos`, preserving the length-marker bits so IDs compare
/// against their canonical constants (e.g. `Segment` == `0x18538067`).
pub fn read_id(_buf: &[u8], _pos: usize) -> Option<(u32, usize)> {
    todo!()
}

/// Encode `value` as a minimal-length EBML data-size vint.
pub fn write_vint(_value: u64) -> Vec<u8> {
    todo!()
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
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test ebml:: 2>&1 | tail -20`
Expected: compiles, tests panic with `not yet implemented` (the `todo!()`s).

- [ ] **Step 4: Implement the three functions**

Replace the three `todo!()` bodies:

```rust
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test ebml:: 2>&1 | tail -20`
Expected: all 6 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/ebml.rs src/main.rs
git commit -m "feat(ebml): vint/ID readers and minimal-length vint writer"
```

---

## Task 2: EBML element headers and builders

**Files:**
- Modify: `src/ebml.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/ebml.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ebml:: 2>&1 | tail -20`
Expected: compile error — `read_element`, `Element`, `ID_*`, `el`, `uint`, `float32` undefined.

- [ ] **Step 3: Implement element headers, constants, and builders**

Add to `src/ebml.rs` (above the `tests` module):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ebml:: 2>&1 | tail -20`
Expected: all tests (including the 3 new) pass.

- [ ] **Step 5: Commit**

```bash
git add src/ebml.rs
git commit -m "feat(ebml): element header reader, ID constants, and builders"
```

---

## Task 3: Codec primitives — VP9 dimensions and Opus head

**Files:**
- Create: `src/mkv_codecs.rs`
- Modify: `src/main.rs` (add `mod mkv_codecs;`)

- [ ] **Step 0: Declare the module**

In `src/main.rs`, add `mod mkv_codecs;` to the module block (alphabetical: after `mod h264;`, before `mod mp4;`). Without this, `cargo test` won't compile the new file.

- [ ] **Step 1: Write the failing tests**

Create `src/mkv_codecs.rs`:

```rust
// SPDX-License-Identifier: GPL-3.0-or-later
//! Codec identification and per-codec Matroska head synthesis for the
//! reconstructive (Tier 2) re-head. WebM-native codecs (VP8/VP9/AV1/Opus) are
//! parsed here; H.264-in-Matroska delegates to `h264.rs`/`divine.rs` (Plan 3).

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

pub fn vp9_dims(_frame: &[u8]) -> Option<(u32, u32)> {
    todo!()
}

pub fn opus_channels(_toc: u8) -> u8 {
    todo!()
}

pub fn opus_head(_channels: u8) -> Vec<u8> {
    todo!()
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
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test mkv_codecs:: 2>&1 | tail -20`
Expected: tests panic with `not yet implemented`.

- [ ] **Step 3: Implement the bit reader and codec primitives**

Add to `src/mkv_codecs.rs` (above `tests`):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test mkv_codecs:: 2>&1 | tail -20`
Expected: all 5 tests pass (`vp9_dims_reads_1080p_keyframe` proves the bit reader against real data).

- [ ] **Step 5: Commit**

```bash
git add src/mkv_codecs.rs
git commit -m "feat(mkv_codecs): VP9 keyframe geometry parse + Opus head synthesis"
```

---

## Task 4: Codec sniffing and TrackEntry synthesis

**Files:**
- Modify: `src/mkv_codecs.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/mkv_codecs.rs`:

```rust
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

    /// Tiny substring search for assertions.
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test mkv_codecs:: 2>&1 | tail -20`
Expected: compile error — `sniff` and `track_entry` undefined.

- [ ] **Step 3: Implement `sniff` and `track_entry`**

Add to `src/mkv_codecs.rs` (above `tests`):

```rust
use crate::ebml::{self, el, uint};
use std::collections::BTreeSet;

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
    let configs: BTreeSet<u8> = frames
        .iter()
        .filter_map(|(_, f)| f.first().map(|b| b >> 3))
        .collect();
    if configs.len() == 1
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
        other => panic!("track_entry called on a codec needing a donor: {other:?}"),
    }
    el(ebml::ID_TRACK_ENTRY, &body)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test mkv_codecs:: 2>&1 | tail -20`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/mkv_codecs.rs
git commit -m "feat(mkv_codecs): codec sniffing and TrackEntry synthesis"
```

---

## Task 5: Matroska cluster walker and frame sampling

**Files:**
- Create: `src/matroska.rs`
- Modify: `src/main.rs` (add `mod matroska;`)

- [ ] **Step 0: Declare the module**

In `src/main.rs`, add `mod matroska;` to the module block (alphabetical: after `mod h264;`/`mod mkv_codecs;` ordering — place it after `mod h264;` and before `mod mkv_codecs;`). Without this, `cargo test` won't compile the new file.

- [ ] **Step 1: Write the failing tests**

Create `src/matroska.rs`:

```rust
// SPDX-License-Identifier: GPL-3.0-or-later
//! Matroska/WebM analysis and re-heading. Walks surviving clusters, sniffs each
//! track's codec from frame bytes, and synthesizes a fresh container head
//! (EBML + Segment + Info + Tracks) — copying clusters verbatim, never decoding.

use std::collections::BTreeMap;

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
        if e.id == ebml::ID_SIMPLE_BLOCK {
            if let Some(block) = parse_simpleblock(&data[e.data_pos..body_end]) {
                f(block);
            }
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
```

- [ ] **Step 2: Run tests to verify they fail, then pass**

The implementation is included above (this task is mostly mechanical and well-covered by the tests). Run:

Run: `cargo test matroska:: 2>&1 | tail -20`
Expected: compiles and both tests pass. If `read_vint`/`read_element` visibility errors appear, confirm Tasks 1-2 are committed.

- [ ] **Step 3: Commit**

```bash
git add src/matroska.rs
git commit -m "feat(matroska): cluster walker and per-track frame sampling"
```

---

## Task 6: Matroska analysis (Intact / Heavy / NoStructure)

**Files:**
- Modify: `src/matroska.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/matroska.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test matroska:: 2>&1 | tail -20`
Expected: compile error — `analyze`, `Analysis`, `Heavy`, `Track` undefined.

- [ ] **Step 3: Implement the analysis types and `analyze`**

Add to `src/matroska.rs` (above the `tests` module):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test matroska:: 2>&1 | tail -20`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/matroska.rs
git commit -m "feat(matroska): analyze beheading vs intact vs no-structure"
```

---

## Task 7: Head synthesis and reconstruction

**Files:**
- Modify: `src/matroska.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/matroska.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test matroska:: 2>&1 | tail -20`
Expected: compile error — `build_head`, `reconstruct` undefined.

- [ ] **Step 3: Implement `build_head` and `reconstruct`**

Add to `src/matroska.rs` (above the `tests` module):

```rust
use anyhow::{Result, bail};

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
        if let Codec::NeedsDonor { hint } = t.codec {
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test matroska:: 2>&1 | tail -20`
Expected: all tests pass.

- [ ] **Step 5: Verify the whole crate still builds and lints**

Run: `cargo build 2>&1 | tail -5 && cargo clippy 2>&1 | tail -15`
Expected: builds; no new clippy warnings in the new modules. Fix any clippy nits inline.

- [ ] **Step 6: Commit**

```bash
git add src/matroska.rs
git commit -m "feat(matroska): synthesize container head and reconstruct file bytes"
```

---

## Task 8: ffx stream-copy remux

**Files:**
- Modify: `src/ffx.rs` (add after `remux`, around line 410)

- [ ] **Step 1: Add `remux_copy`**

In `src/ffx.rs`, immediately after the existing `remux` function (ends at line 410), add:

```rust
/// Stream-copy every track into a Matroska/WebM container (no transcode). The
/// container is chosen by `output`'s extension (`.webm` or `.mkv`). This also
/// launders benign codec-parser chatter: ffmpeg's own muxer rewrites the blocks,
/// so the result decodes clean even when the input made the VP9 parser grumble.
pub fn remux_copy(input: &Path, output: &Path) -> Result<()> {
    run_ffmpeg(&["-map", "0", "-c", "copy"], input, output, &[])
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build 2>&1 | tail -5`
Expected: builds clean. (No unit test — this is a thin ffmpeg wrapper, exercised by the e2e test in Task 10, matching how `remux`/`to_correct_format` are tested.)

- [ ] **Step 3: Commit**

```bash
git add src/ffx.rs
git commit -m "feat(ffx): remux_copy — stream-copy into Matroska/WebM"
```

---

## Task 9: Wire the rescue pipeline

**Files:**
- Modify: `src/rescue.rs:72` (the `NoMoov` arm) and add a `rescue_matroska` orchestrator.

- [ ] **Step 1: Add the imports**

In `src/rescue.rs`, the imports block ends around line 18 (`use crate::transplant;`). Add:

```rust
use crate::matroska;
use crate::mkv_codecs::Codec;
```

- [ ] **Step 2: Route the `NoMoov` arm to Matroska when applicable**

In `src/rescue.rs`, the `rescue` function's match has (line 72):

```rust
        Analysis::NoMoov => rescue_headerless_stream(input, output, &data, &findings, opts),
```

Replace that single arm with:

```rust
        Analysis::NoMoov => match matroska::analyze(&data) {
            matroska::Analysis::Heavy(h) => rescue_matroska(input, output, &data, h, opts),
            // Intact Matroska / no structure: let the headerless path probe it
            // (ffprobe reads intact Matroska; truly unknown data falls through).
            _ => rescue_headerless_stream(input, output, &data, &findings, opts),
        },
```

- [ ] **Step 3: Add the `rescue_matroska` orchestrator**

In `src/rescue.rs`, add this function after `rescue_beheaded_mp4` (which ends around line 168), so it sits with the other reconstruction orchestrators:

```rust
// ---------------------------------------------------------------------------
// Reconstructive: heavily-beheaded Matroska/WebM (only clusters survive)
// ---------------------------------------------------------------------------

fn rescue_matroska(
    input: &Path,
    output: Option<PathBuf>,
    data: &[u8],
    h: matroska::Heavy,
    opts: &Options,
) -> Result<()> {
    println!("\n  diagnosis: beheaded Matroska/WebM — only clusters survived");
    println!(
        "    bytes lost before first clean cluster : {} (~head + first cluster)",
        h.bytes_lost
    );
    for t in &h.tracks {
        println!("    track {}: {:?}", t.number, t.codec);
    }

    // Honest failure: a track whose parameters lived in the lost header and
    // cannot be synthesized from frames. Transplant (donor) arrives in Plan 2.
    for t in &h.tracks {
        if let Codec::NeedsDonor { hint } = t.codec {
            bail!(
                "track {} ({hint}) keeps its parameters in the lost Tracks header.\n\
                 basinski can't synthesize them from the frames alone. A donor-based\n\
                 transplant (--reference <sibling from the same encoder>) is the next\n\
                 rung, but is not yet implemented for Matroska.",
                t.number
            );
        }
    }

    let webm = matroska::all_webm_legal(&h.tracks);
    let ext = if webm { "webm" } else { "mkv" };
    let out = output.unwrap_or_else(|| default_output(input, "rescued", ext));

    // Choose the playback start at the container level: the first cluster led by
    // a true keyframe gives clean playback from frame one (no ffmpeg clip).
    // --no-clip keeps everything from the first surviving cluster instead.
    let start = if opts.no_clip {
        h.first_cluster
    } else {
        h.first_keyframe_cluster.unwrap_or(h.first_cluster)
    };
    if opts.no_clip {
        println!("  (--no-clip: starting at first surviving cluster; leading mid-GOP frames may artifact)");
    } else if h.first_keyframe_cluster.is_none() {
        println!("  ⚠ no keyframe-led cluster found in scan window — starting mid-GOP, expect leading artifacts");
    } else if start > h.first_cluster {
        println!("  ✂ starting at first keyframe-led cluster (offset {start})");
    }

    let rebuilt = matroska::reconstruct(data, &h, start)?;
    let head_len = rebuilt.len() - (data.len() - start);
    let temp = out.with_extension(format!("rehead.{ext}"));
    fs::write(&temp, &rebuilt).with_context(|| format!("writing {}", temp.display()))?;
    println!(
        "\n  ☼ synthesized {head_len}-byte head (EBML + Segment + Info + Tracks) → {}",
        temp.display()
    );

    let probe = ffx::probe(&temp)?
        .context("reconstruction produced a file ffprobe cannot read — head may be wrong")?;
    println!("  container restored: {}", probe.summary());

    // Stream-copy remux: ffmpeg's own muxer rewrites the blocks cleanly and
    // launders the benign VP9 parser chatter, yielding a pristine container.
    ffx::remux_copy(&temp, &out)?;

    // Validate the reconstruction against the decoder (the basinski contract).
    let errs = ffx::decode_errors(&out).unwrap_or(0);
    if errs == 0 {
        println!("  ✓ decodes clean (0 errors)");
    } else {
        println!("  ⚠ {errs} decode error line(s) remain — inspect the output");
    }

    if !opts.keep_temp {
        let _ = fs::remove_file(&temp);
    }
    finish(input, &out, opts)
}
```

- [ ] **Step 4: Verify the crate builds**

Run: `cargo build 2>&1 | tail -10`
Expected: builds clean. (`Codec` comes from the `use crate::mkv_codecs::Codec;` added in Step 1.)

- [ ] **Step 5: Run the full unit suite**

Run: `cargo test 2>&1 | tail -20`
Expected: all tests pass (existing + new ebml/mkv_codecs/matroska tests).

- [ ] **Step 6: Smoke-test against the real casualty**

Run:
```bash
cargo build --release 2>&1 | tail -2
target/release/basinski rescue samples/trey-bermudan-broke2.webm --keep-temp 2>&1 | tail -25
```
Expected: diagnosis prints VP9 (1920×1080) + Opus; a `.rescued.webm` is written. Verify it:
```bash
ffprobe -v error -show_entries stream=codec_name,codec_type,width,height,channels -of csv samples/trey-bermudan-broke2.rescued.webm
ffmpeg -v error -i samples/trey-bermudan-broke2.rescued.webm -f null - 2>&1 | wc -l
```
Expected: streams report `vp9,video,1920,1080` and `opus,audio,...,2`; decode error-line count is 0. Clean up: `rm -f samples/trey-bermudan-broke2.rescued* samples/trey-bermudan-broke2.*rehead* samples/trey-bermudan-broke2.*full*`.

- [ ] **Step 7: Commit**

```bash
git add src/rescue.rs
git commit -m "feat(rescue): route beheaded Matroska/WebM to reconstructive re-head"
```

---

## Task 10: End-to-end round-trip test

**Files:**
- Modify: `tests/e2e.sh`

- [ ] **Step 1: Read the existing harness**

Run: `sed -n '1,60p' tests/e2e.sh` and note: how `$BIN` is set, the libx264 availability guard, the synth/behead/rescue/assert helper pattern, and how pass/fail is reported. Mirror that style exactly.

- [ ] **Step 2: Add an encoder guard and a WebM reconstructive case**

Add near the other dependency checks: a guard that skips the WebM case (with a printed notice, not a failure) unless ffmpeg lists both encoders:

```bash
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libvpx-vp9 \
   && ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libopus; then
  WEBM_OK=1
else
  echo "skip: WebM reconstructive case needs libvpx-vp9 + libopus"
  WEBM_OK=0
fi
```

Then, gated on `[ "$WEBM_OK" = 1 ]`, add the case (adapt variable names to the script's conventions):

```bash
# --- Matroska/WebM reconstructive re-head (VP9 + Opus, heavy beheading) ---
WEBM_SRC="$TMP/src.webm"
ffmpeg -v error -y -f lavfi -i "testsrc2=size=640x360:rate=30:duration=6" \
  -f lavfi -i "sine=frequency=440:duration=6" \
  -c:v libvpx-vp9 -b:v 600k -g 24 -c:a libopus -b:a 64k "$WEBM_SRC"

# Behead: drop everything before the 2nd surviving cluster so the Tracks element
# is gone and only clusters remain (mirrors the real casualty).
python3 - "$WEBM_SRC" "$TMP/beheaded.webm" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
sig = bytes.fromhex("1F43B675")
first = data.find(sig)
second = data.find(sig, first + 1)
cut = second if second != -1 else first
open(sys.argv[2], "wb").write(data[cut:])
PY

"$BIN" rescue "$TMP/beheaded.webm" -o "$TMP/rescued.webm"
# Assert: ffprobe reads VP9 + Opus and the decode is clean.
probe=$(ffprobe -v error -show_entries stream=codec_name -of csv=p=0 "$TMP/rescued.webm" | tr '\n' ',')
echo "$probe" | grep -q vp9  || { echo "FAIL: no vp9 in rescued.webm"; exit 1; }
echo "$probe" | grep -q opus || { echo "FAIL: no opus in rescued.webm"; exit 1; }
errs=$(ffmpeg -v error -i "$TMP/rescued.webm" -f null - 2>&1 | grep -c . || true)
[ "$errs" = 0 ] || { echo "FAIL: rescued.webm decoded with $errs error lines"; exit 1; }
echo "PASS: WebM VP9/Opus reconstructive re-head"
```

- [ ] **Step 3: Run the e2e script**

Run: `bash tests/e2e.sh 2>&1 | tail -30`
Expected: existing MP4 cases still pass; the new WebM case prints `PASS: WebM VP9/Opus reconstructive re-head` (or the skip notice if encoders are missing).

- [ ] **Step 4: Commit**

```bash
git add tests/e2e.sh
git commit -m "test(e2e): WebM VP9/Opus reconstructive re-head round-trip"
```

---

## Task 11: Documentation and casework note

**Files:**
- Modify: `README.md`
- Modify: `samples/NOTES.md` (machine-local; gitignored — update but do not commit)

- [ ] **Step 1: Add a Matroska/WebM note to the README**

Find the section that describes the rescue ladder / supported formats (search: `rg -n "untrunc|transplant|ladder|moov" README.md`). Add a subsection explaining the new capability. Use this content:

```markdown
### Matroska / WebM (beheaded)

A beheaded `.mkv`/`.webm` loses its `EBML` header and often the `Tracks` element
that names the codecs. basinski re-heads it: it walks the surviving `Cluster`s,
identifies each track's codec from the frame bytes themselves (VP9 geometry from
the keyframe, Opus channels from the TOC byte), synthesizes a fresh
`EBML`+`Segment`+`Info`+`Tracks` head, and copies the clusters back verbatim —
**it never re-encodes**. This works because Matroska's seek offsets are
Segment-relative, so a regrown head doesn't invalidate anything.

Currently reconstructed without a donor: **VP9 video, Opus audio**. (VP8, AV1,
and H.264-in-MKV, plus donor-based transplant for Vorbis/AAC, are on the way.)
```

If a "Honest limitations" section exists, add:

```markdown
- Beheaded Matroska with **Vorbis or AAC** audio can't be re-headed from frames
  alone — those codecs hide their setup data (codebooks / AudioSpecificConfig) in
  the lost header. A donor-based transplant is planned.
```

- [ ] **Step 2: Verify the README reads correctly**

Run: `rg -n "Matroska / WebM \(beheaded\)" README.md`
Expected: the new heading is present.

- [ ] **Step 3: Record the casework (machine-local)**

Append to `samples/NOTES.md` (create it if absent). This file is gitignored and is also mirrored into auto-memory, so it is not committed:

```markdown
## trey-bermudan-broke2.webm — SOLVED (Plan 1)

Heavy beheading: no EBML/Segment/Info/Tracks survive; first clean cluster at
offset 540387. Track 1 = VP9 1920x1080 (~29.97fps), track 2 = Opus stereo
(TOC config 31). Reconstructive re-head (no donor): synthesize head from
frame-derived params + copy clusters verbatim → remux → clip to first clean
keyframe. ffprobe reads vp9+opus; decode errors = 0; extracted frame is a clean
1080p picture (Wikitongues speaker, "CARDINALS" shirt).
```

- [ ] **Step 4: Commit the README (NOTES.md stays uncommitted)**

```bash
git add README.md
git commit -m "docs: Matroska/WebM re-heading (VP9/Opus) in README"
```

---

## Self-Review (completed during planning)

- **Spec coverage (Plan 1 slice):** EBML primitives ✓ (Tasks 1-2). Cluster walker ✓ (Task 5). `analyze` tiering — Heavy/Intact/NoStructure ✓ (Task 6); LightBeheading deferred to Plan 2 (noted). VP9 + Opus sniff/synthesis ✓ (Tasks 3-4); VP8/AV1/H.264/Vorbis/AAC are explicitly Plan 2/3 and surface here as `NeedsDonor` with a loud failure. `remux_copy` + `.webm`/`.mkv` selection ✓ (Tasks 8-9). rescue wiring removes the original `bail!` path for Matroska ✓ (Task 9). Validation contract — probe → remux_copy → decode-errors ✓ (Task 9); the "clip to first clean keyframe" step is replaced by a stronger container-level choice (start at the first true-keyframe-led cluster, detected via `vp9_dims`), which needs no ffmpeg clip and sidesteps the MP4-only `-movflags`/`.probe.mp4` assumptions in `clip_from_keyframe`/`first_clean_keyframe`. Honest failure for donor-only codecs ✓ (Task 9). Tests + e2e + docs ✓ (Tasks 10-11).
- **Placeholder scan:** No `TODO`/`TBD` in shipped code (the `todo!()`s exist only as the red-test step and are replaced in the same task). All code blocks are complete.
- **Type consistency:** `Codec` (mkv_codecs) variants used identically across tasks; `Analysis`/`Heavy`/`Track` defined in Task 6 (with `Heavy.first_keyframe_cluster: Option<usize>`) and consumed in Tasks 7/9; `sniff(&[(bool, Vec<u8>)])`, `track_entry(u64, &Codec)`, `build_head(&[Track], &str)`, `reconstruct(&[u8], &Heavy, usize)`, `all_webm_legal(&[Track])`, `remux_copy(&Path,&Path)` signatures match every call site.

---

## Deferred to later plans (tracked, not dropped)

- **Plan 2 — Tier 1 (surgical) + Tier 3 (transplant):** `analyze` gains `LightBeheading` (Segment+Tracks survive → regrow only the EBML header); `rescue_matroska` gains a donor path (`--reference`) that harvests EBML+Info+Tracks and splices, replacing the "transplant not yet implemented" failure. Codec-agnostic; unblocks Vorbis/AAC.
- **Plan 3 — Tier-2 codec breadth:** VP8 (`vp8_dims`), AV1 (OBU walk + sequence-header → `av1C`), and H.264-in-MKV (in-band SPS/PPS harvest via `h264.rs`, else `divine.rs`). Each adds a `sniff` branch + `track_entry` arm + an e2e case guarded on encoder availability.
