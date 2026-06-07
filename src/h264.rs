// SPDX-License-Identifier: GPL-3.0-or-later
//! H.264 bitstream surgery: Exp-Golomb reading and writing, emulation
//! prevention, SPS parsing — and SPS *patching*, for manufacturing organ
//! donors whose parameter sets match a stream no living encoder will emit.
//!
//! Hardware encoders pick SPS field widths (log2_max_frame_num,
//! log2_max_pic_order_cnt_lsb) that x264 never produces. When a head_truncated
//! stream's parameter sets are gone, we re-encode a donor with the right
//! macroblock geometry and then bit-splice the widths the slices actually
//! use. The splice shifts everything downstream, so emulation-prevention
//! bytes are stripped, the bits rewoven, and the escapes re-inserted.

// ---------------------------------------------------------------------------
// Bit reading
// ---------------------------------------------------------------------------

pub(crate) struct Bits<'a> {
    d: &'a [u8],
    bit: usize,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(d: &'a [u8]) -> Self {
        Self { d, bit: 0 }
    }
    pub(crate) fn pos(&self) -> usize {
        self.bit
    }
    pub(crate) fn u1(&mut self) -> Option<bool> {
        let byte = self.d.get(self.bit / 8)?;
        let v = (byte >> (7 - self.bit % 8)) & 1;
        self.bit += 1;
        Some(v == 1)
    }
    pub(crate) fn u(&mut self, n: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.u1()? as u64;
        }
        Some(v)
    }
    pub(crate) fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while !self.u1()? {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        Some((1u64 << zeros) - 1 + self.u(zeros)?)
    }
    pub(crate) fn se(&mut self) -> Option<i64> {
        let k = self.ue()?;
        let v = k.div_ceil(2) as i64;
        Some(if k % 2 == 1 { v } else { -v })
    }
}

// ---------------------------------------------------------------------------
// Bit writing
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn put(&mut self, b: bool) {
        self.bits.push(b);
    }
    pub(crate) fn ue(&mut self, v: u64) {
        let v = v + 1;
        let n = 64 - v.leading_zeros();
        for _ in 1..n {
            self.put(false);
        }
        for i in (0..n).rev() {
            self.put(v >> i & 1 == 1);
        }
    }
    /// Copy bits [from, to) of `src` (bit indices) into the stream.
    pub(crate) fn copy_bits(&mut self, src: &[u8], from: usize, to: usize) {
        for i in from..to.min(src.len() * 8) {
            self.put(src[i / 8] >> (7 - i % 8) & 1 == 1);
        }
    }
    pub(crate) fn se(&mut self, v: i64) {
        let k = if v > 0 {
            2 * v as u64 - 1
        } else {
            (-2 * v) as u64
        };
        self.ue(k);
    }
    pub(crate) fn u(&mut self, v: u64, n: u32) {
        for i in (0..n).rev() {
            self.put(v >> i & 1 == 1);
        }
    }
    /// Pack to bytes, zero-padded to a byte boundary.
    pub(crate) fn finish(self) -> Vec<u8> {
        let mut out = vec![0u8; self.bits.len().div_ceil(8)];
        for (i, &b) in self.bits.iter().enumerate() {
            if b {
                out[i / 8] |= 1 << (7 - i % 8);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Emulation prevention
// ---------------------------------------------------------------------------

/// Remove emulation-prevention bytes (00 00 03 → 00 00).
pub(crate) fn strip_emulation(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len());
    let mut zeros = 0;
    for &b in d {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Re-insert emulation-prevention bytes (00 00 0x → 00 00 03 0x).
pub(crate) fn add_emulation(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len() + 8);
    let mut zeros = 0;
    for &b in d {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

// ---------------------------------------------------------------------------
// SPS
// ---------------------------------------------------------------------------

pub(crate) struct SpsInfo {
    pub(crate) log2_max_frame_num: u32,
    pub(crate) poc_type: u64,
    pub(crate) log2_max_poc_lsb: u32,
    pub(crate) frame_mbs_only: bool,
    pub(crate) separate_colour_plane: bool,
}

const HIGH_PROFILES: [u64; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Parse the fields of an SPS NAL (header byte included) that slice-header
/// parsing depends on. Returns the bit position where the width fields begin
/// and end (within the *stripped* RBSP) alongside the info — the patcher
/// splices exactly that span.
fn parse_sps_inner(nal: &[u8]) -> Option<(SpsInfo, Vec<u8>, usize, usize)> {
    let rbsp = strip_emulation(nal.get(1..)?);
    let mut b = Bits::new(&rbsp);
    let profile = b.u(8)?;
    b.u(16)?; // constraint flags + level
    b.ue()?; // sps_id
    let mut separate_colour_plane = false;
    if HIGH_PROFILES.contains(&profile) {
        let chroma = b.ue()?;
        if chroma == 3 {
            separate_colour_plane = b.u1()?;
        }
        b.ue()?; // bit_depth_luma
        b.ue()?; // bit_depth_chroma
        b.u1()?; // qpprime_y_zero_transform_bypass
        if b.u1()? {
            // seq_scaling_matrix: skip the lists
            let lists = if chroma == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.u1()? {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    let width_start = b.pos();
    let log2_max_frame_num = (b.ue()? + 4) as u32;
    let poc_type = b.ue()?;
    let mut log2_max_poc_lsb = 0;
    match poc_type {
        0 => log2_max_poc_lsb = (b.ue()? + 4) as u32,
        1 => {
            b.u1()?;
            b.se()?;
            b.se()?;
            let n = b.ue()?;
            for _ in 0..n {
                b.se()?;
            }
        }
        _ => {}
    }
    let width_end = b.pos();
    b.ue()?; // max_num_ref_frames
    b.u1()?; // gaps_in_frame_num_value_allowed
    b.ue()?; // pic_width_in_mbs
    b.ue()?; // pic_height_in_map_units
    let frame_mbs_only = b.u1()?;
    Some((
        SpsInfo {
            log2_max_frame_num,
            poc_type,
            log2_max_poc_lsb,
            frame_mbs_only,
            separate_colour_plane,
        },
        rbsp,
        width_start,
        width_end,
    ))
}

pub(crate) fn parse_sps(nal: &[u8]) -> Option<SpsInfo> {
    parse_sps_inner(nal).map(|(info, ..)| info)
}

/// Hunt down the avcC inside a verbatim stsd box (or any buffer) and parse
/// its first SPS.
pub(crate) fn parse_sps_from_stsd(stsd: &[u8]) -> Option<SpsInfo> {
    extract_avcc_params(stsd).and_then(|(sps, _)| parse_sps(&sps))
}

/// First SPS and PPS NALs from the first avcC box found in the buffer.
pub(crate) fn extract_avcc_params(d: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let idx = d.windows(4).position(|w| w == b"avcC")?;
    let p = idx + 4;
    // configurationVersion(1) profile(1) compat(1) level(1) lengthSize(1) numSPS(1)
    if *d.get(p)? != 1 || d.get(p + 5)? & 0x1F == 0 {
        return None;
    }
    let sps_len = u16::from_be_bytes([*d.get(p + 6)?, *d.get(p + 7)?]) as usize;
    let sps = d.get(p + 8..p + 8 + sps_len)?.to_vec();
    let q = p + 8 + sps_len;
    if *d.get(q)? == 0 {
        return None; // no PPS
    }
    let pps_len = u16::from_be_bytes([*d.get(q + 1)?, *d.get(q + 2)?]) as usize;
    let pps = d.get(q + 3..q + 3 + pps_len)?.to_vec();
    Some((sps, pps))
}

/// Rewrite an SPS NAL's frame_num / POC width fields:
/// log2_max_frame_num = 4 + fn_m4; `poc_m4 = Some(p)` sets
/// pic_order_cnt_type 0 with log2_max_pic_order_cnt_lsb = 4 + p, while
/// `None` sets pic_order_cnt_type 2 — the no-B-frames layout in which slice
/// headers carry no POC field at all. Everything downstream shifts;
/// emulation prevention is redone from scratch.
pub(crate) fn patch_sps_widths(nal: &[u8], fn_m4: u64, poc_m4: Option<u64>) -> Option<Vec<u8>> {
    let (_, rbsp, start, end) = parse_sps_inner(nal)?;
    let mut w = BitWriter::new();
    w.copy_bits(&rbsp, 0, start);
    w.ue(fn_m4);
    match poc_m4 {
        Some(p) => {
            w.ue(0); // poc_type 0
            w.ue(p);
        }
        None => w.ue(2), // poc_type 2: display order IS decode order
    }
    w.copy_bits(&rbsp, end, rbsp.len() * 8);
    let body = w.finish();
    let mut out = vec![nal[0]];
    out.extend(add_emulation(&body));
    Some(out)
}

// ---------------------------------------------------------------------------
// PPS — synthesized whole, never borrowed
// ---------------------------------------------------------------------------

/// Everything a PPS says that matters to slice parsing and entropy decode.
/// These are exactly the unknowns `divine` grids over, so we write the PPS
/// from scratch instead of borrowing an encoder's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PpsParams {
    pub(crate) cabac: bool,
    /// Default reference count for list 0 (P-slice headers parse against it).
    pub(crate) ref_l0: u32,
    /// Slices carry pred-weight tables (hardware: rare; x264 medium+: yes).
    pub(crate) weighted_pred: bool,
    /// CABAC seeds its contexts from this; CAVLC merely dequantizes with it.
    pub(crate) init_qp: i64,
    pub(crate) chroma_qp_offset: i64,
    /// transform_8x8_mode — changes I-macroblock parsing (High profile).
    pub(crate) transform_8x8: bool,
}

pub(crate) fn build_pps(p: PpsParams) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pps_id
    w.ue(0); // sps_id
    w.put(p.cabac);
    w.put(false); // bottom_field_pic_order_in_frame_present
    w.ue(0); // num_slice_groups_minus1
    w.ue(p.ref_l0.saturating_sub(1) as u64);
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.put(p.weighted_pred);
    w.u(2, 2); // weighted_bipred_idc: implicit (no tables in B headers)
    w.se(p.init_qp - 26);
    w.se(0); // pic_init_qs
    w.se(p.chroma_qp_offset);
    w.put(true); // deblocking_filter_control_present (slices carry deblock fields)
    w.put(false); // constrained_intra_pred
    w.put(false); // redundant_pic_cnt_present
    if p.transform_8x8 {
        w.put(true); // transform_8x8_mode
        w.put(false); // pic_scaling_matrix_present
        w.se(p.chroma_qp_offset); // second_chroma_qp_index_offset
    }
    w.put(true); // rbsp stop bit
    let mut nal = vec![0x68];
    nal.extend(add_emulation(&w.finish()));
    nal
}

// ---------------------------------------------------------------------------
// Transplanting patched parameter sets back into an mp4
// ---------------------------------------------------------------------------

/// Replace the first SPS and PPS inside the file's avcC, growing or
/// shrinking every ancestor box on the way up. Requires moov to follow mdat
/// (so chunk offsets don't move) — true of every donor this tool encodes.
pub(crate) fn replace_avcc_params(mp4: &[u8], new_sps: &[u8], new_pps: &[u8]) -> Option<Vec<u8>> {
    let avcc_at = mp4.windows(4).position(|w| w == b"avcC")? - 4;
    let sps_len_at = avcc_at + 8 + 6;
    let old_sps = u16::from_be_bytes([*mp4.get(sps_len_at)?, *mp4.get(sps_len_at + 1)?]) as usize;
    let pps_count_at = sps_len_at + 2 + old_sps;
    if *mp4.get(pps_count_at)? == 0 {
        return None;
    }
    let pps_len_at = pps_count_at + 1;
    let old_pps = u16::from_be_bytes([*mp4.get(pps_len_at)?, *mp4.get(pps_len_at + 1)?]) as usize;
    mp4.get(pps_len_at + 2 + old_pps - 1)?; // bounds
    let diff = (new_sps.len() + new_pps.len()) as i64 - (old_sps + old_pps) as i64;

    let mut d = mp4.to_vec();
    // Bump every box containing the avcC.
    fn bump(d: &mut [u8], mut pos: usize, end: usize, target: usize, diff: i64) {
        while pos + 8 <= end {
            let size = u32::from_be_bytes(d[pos..pos + 4].try_into().unwrap()) as usize;
            if size < 8 {
                return;
            }
            if pos <= target && target < pos + size {
                let new = (size as i64 + diff) as u32;
                d[pos..pos + 4].copy_from_slice(&new.to_be_bytes());
                bump(d, pos + 8, pos + size, target, diff);
                return;
            }
            pos += size;
        }
    }
    bump(&mut d, 0, mp4.len(), avcc_at, diff);
    // Replace PPS first (later offset), then SPS — earlier edits don't shift it.
    d[pps_len_at..pps_len_at + 2].copy_from_slice(&(new_pps.len() as u16).to_be_bytes());
    d.splice(
        pps_len_at + 2..pps_len_at + 2 + old_pps,
        new_pps.iter().copied(),
    );
    d[sps_len_at..sps_len_at + 2].copy_from_slice(&(new_sps.len() as u16).to_be_bytes());
    d.splice(
        sps_len_at + 2..sps_len_at + 2 + old_sps,
        new_sps.iter().copied(),
    );
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ue_roundtrip() {
        for v in [0u64, 1, 2, 3, 7, 8, 254, 255, 1000] {
            let mut w = BitWriter::new();
            w.ue(v);
            let bytes = w.finish();
            assert_eq!(Bits::new(&bytes).ue(), Some(v), "ue({v})");
        }
    }

    #[test]
    fn emulation_roundtrip() {
        let raw = [0u8, 0, 0, 1, 0, 0, 2, 0xFF, 0, 0, 3, 9, 0, 0, 0];
        let escaped = add_emulation(&raw);
        assert_eq!(strip_emulation(&escaped), raw);
        // No forbidden sequences survive in the escaped form.
        assert!(
            !escaped
                .windows(3)
                .any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2)
        );
    }

    /// A minimal but real baseline SPS, hand-assembled: profile 66,
    /// log2_max_frame_num=4+1, poc_type 0 with lsb width 4+2, 1 ref,
    /// 80x45 MBs (1280x720), frame_mbs_only=1.
    fn tiny_sps() -> Vec<u8> {
        let mut w = BitWriter::new();
        for v in [66u64, 0, 30] {
            for i in (0..8).rev() {
                w.put(v >> i & 1 == 1);
            }
        }
        w.ue(0); // sps_id
        w.ue(1); // log2_max_frame_num_minus4
        w.ue(0); // poc_type
        w.ue(2); // log2_max_poc_lsb_minus4
        w.ue(1); // max_num_ref_frames
        w.put(false); // gaps_in_frame_num
        w.ue(79); // width mbs - 1
        w.ue(44); // height map units - 1
        w.put(true); // frame_mbs_only
        w.put(false); // direct_8x8
        w.put(false); // cropping
        w.put(false); // vui
        w.put(true); // rbsp stop bit
        let mut nal = vec![0x67];
        nal.extend(add_emulation(&w.finish()));
        nal
    }

    #[test]
    fn sps_parses_and_patches() {
        let sps = tiny_sps();
        let info = parse_sps(&sps).unwrap();
        assert_eq!(info.log2_max_frame_num, 5);
        assert_eq!(info.poc_type, 0);
        assert_eq!(info.log2_max_poc_lsb, 6);
        assert!(info.frame_mbs_only);

        let patched = patch_sps_widths(&sps, 0, Some(4)).unwrap();
        let info2 = parse_sps(&patched).unwrap();
        assert_eq!(info2.log2_max_frame_num, 4);
        assert_eq!(info2.poc_type, 0);
        assert_eq!(info2.log2_max_poc_lsb, 8);
        assert_eq!(info2.frame_mbs_only, info.frame_mbs_only);

        // poc_type 2: no POC field in slice headers at all
        let poc2 = patch_sps_widths(&sps, 1, None).unwrap();
        let info3 = parse_sps(&poc2).unwrap();
        assert_eq!(info3.log2_max_frame_num, 5);
        assert_eq!(info3.poc_type, 2);
        assert_eq!(info3.frame_mbs_only, info.frame_mbs_only);
    }

    #[test]
    fn mp4_sps_replacement_bumps_ancestors() {
        // moov[ trak[ stbl[ stsd[ avc1[ avcC ]]]]] — sizes must all grow.
        let sps = tiny_sps();
        let mut avcc = vec![1u8, 66, 0, 30, 0xFF, 0xE1];
        avcc.extend((sps.len() as u16).to_be_bytes());
        avcc.extend(&sps);
        avcc.push(1); // one PPS
        avcc.extend(2u16.to_be_bytes());
        avcc.extend([0x68, 0xCE]);
        let boxed = |fourcc: &[u8; 4], body: &[u8]| {
            let mut b = (body.len() as u32 + 8).to_be_bytes().to_vec();
            b.extend_from_slice(fourcc);
            b.extend_from_slice(body);
            b
        };
        let file = boxed(
            b"moov",
            &boxed(
                b"trak",
                &boxed(
                    b"stbl",
                    &boxed(b"stsd", &boxed(b"avc1", &boxed(b"avcC", &avcc))),
                ),
            ),
        );

        // Widths that force the SPS to grow, plus a fresh PPS — the box
        // chain must follow both.
        let patched_sps = patch_sps_widths(&sps, 0, Some(8)).unwrap();
        let new_pps = build_pps(PpsParams {
            cabac: true,
            ref_l0: 3,
            weighted_pred: true,
            init_qp: 23,
            chroma_qp_offset: -2,
            transform_8x8: true,
        });
        let out = replace_avcc_params(&file, &patched_sps, &new_pps).unwrap();
        let diff = (patched_sps.len() + new_pps.len()) as i64 - (sps.len() + 2) as i64;
        assert_eq!(out.len() as i64, file.len() as i64 + diff);
        // outermost size grew by diff
        let outer = u32::from_be_bytes(out[0..4].try_into().unwrap()) as i64;
        let orig = u32::from_be_bytes(file[0..4].try_into().unwrap()) as i64;
        assert_eq!(outer, orig + diff);
        // and both parameter sets inside the result read back correctly
        let (sps2, pps2) = extract_avcc_params(&out).unwrap();
        assert_eq!(parse_sps(&sps2).unwrap().log2_max_poc_lsb, 12);
        assert_eq!(pps2, new_pps);
    }

    #[test]
    fn pps_fields_read_back() {
        let pps = build_pps(PpsParams {
            cabac: true,
            ref_l0: 3,
            weighted_pred: true,
            init_qp: 21,
            chroma_qp_offset: -2,
            transform_8x8: false,
        });
        let rbsp = strip_emulation(&pps[1..]);
        let mut b = Bits::new(&rbsp);
        assert_eq!(b.ue(), Some(0)); // pps_id
        assert_eq!(b.ue(), Some(0)); // sps_id
        assert_eq!(b.u1(), Some(true)); // cabac
        assert_eq!(b.u1(), Some(false)); // pic_order_present
        assert_eq!(b.ue(), Some(0)); // slice groups
        assert_eq!(b.ue(), Some(2)); // ref_l0 - 1
        assert_eq!(b.ue(), Some(0)); // ref_l1 - 1
        assert_eq!(b.u1(), Some(true)); // weighted_pred
        assert_eq!(b.u(2), Some(2)); // weighted_bipred
        assert_eq!(b.se(), Some(-5)); // init_qp - 26
        assert_eq!(b.se(), Some(0)); // init_qs
        assert_eq!(b.se(), Some(-2)); // chroma offset
    }
}
