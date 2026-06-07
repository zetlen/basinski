# Design: Matroska / WebM re-heading

**Date:** 2026-06-07
**Status:** Approved (pending spec review)
**Author:** basinski + Claude

## Problem

`basinski rescue` on a beheaded Matroska/WebM file fails with:

```
error: identified as `Matroska / WebM (damaged front)` but basinski does not
know how to re-head that format yet
```

`forensics::scan_ebml` already *detects* the format (Cluster/Segment IDs survive
while the file no longer starts with EBML magic `0x1A`), but the rescue
orchestrator has no handler, so it hits the catch-all `bail!` in
`rescue.rs` (the `rescue_headerless_stream` format-dispatch `match`). This design
teaches basinski to reconstruct the lost container head.

## Casework that grounds this design

Diagnosed `samples/trey-bermudan-broke2.webm` (109 MB, a Wikitongues speaker clip;
deliberately beheaded test fixture):

- **Heavy beheading.** No `EBML` header, no `Segment`, no `Info`, no **`Tracks`**,
  no `Cues` survive anywhere — only `Cluster`s (first clean one at offset
  540387). The first ~528 KB is the orphaned back-half of the destroyed first
  cluster's frame data (its spurious `00 00 01` byte runs fooled the existing
  H.264 Annex-B scanner — the nonsense NAL-type histogram confirms they are not
  real NAL units).
- **Track 1 = VP9.** Cluster-leading frames carry `82 49 83 42` — VP9 keyframe
  marker `10` + sync code `0x498342`. Parsing the keyframe uncompressed header
  directly yields **1920×1080**, ~29.97 fps.
- **Track 2 = Opus.** Every audio frame's TOC byte is `0xfc` (config 31, stereo
  bit set); relative timecodes step 0/20/40/60 ms — Opus's 20 ms framing.

### The structural insight (why this is *easier* than the MP4 case)

MP4 surgical rescue is hard because `moov` uses **file-absolute** offsets, so
beheading invalidates the whole index and you must compute the lost byte count
`K`. Matroska's seek positions (`SeekHead`, `Cues`) are **Segment-relative**, and
media frames live verbatim inside `SimpleBlock`s. Therefore **re-heading never
decodes the codec** — it copies clusters byte-for-byte and rebuilds only the
container head.

### Feasibility proven before committing to Rust

A throwaway Python spike synthesized a 181-byte head (`EBML` + unknown-size
`Segment` + `Info` + `Tracks` describing `V_VP9` 1920×1080 and `A_OPUS` stereo
with a hand-built `OpusHead`) and appended the surviving clusters verbatim.
Results:

1. `ffprobe` reported exactly the synthesized streams (vp9 1920×1080, opus 48000
   stereo) — the head parses.
2. `ffmpeg -c copy` remux **succeeded** (108 MB `.mkv`). The `zero_bit out of
   range` lines are ffmpeg's VP9 *parser* (run even during stream copy to set
   frame flags); they are non-fatal noise, not demux failures.
3. The remuxed file **decoded with zero errors**, and an extracted frame was a
   clean, real 1080p picture.

This validates the entire architecture: **synthesize head → copy clusters
verbatim → remux through ffmpeg (which launders the parser noise) → clean file.**

## Scope

- **Extend `rescue`** (and `--reference`); no new subcommand. `identify` already
  detects the format.
- **Full three-tier ladder**, mirroring basinski's surgical → transplant →
  divine philosophy.
- **Tier-2 codec breadth:** VP8/VP9 + Opus + **AV1** + **H.264-in-MKV** all get
  no-donor reconstruction. Vorbis/AAC route to the Tier-3 donor.
- **Rescue preserves source codecs** via stream copy into `.webm`/`.mkv`. This is
  not transcoding, so it does not touch the `convert` / `--the-correct-format`
  mp4-or-mp3 rule.

## The three tiers

| Tier | Damage profile | Strategy | Donor |
|------|----------------|----------|-------|
| **1 — surgical** | Only the `EBML` header (and maybe `SeekHead`) lost; `Segment` + `Info` + `Tracks` survive | Synthesize a fresh `EBML` header and prepend it. Segment-relative offsets stay valid, so this is lossless and **codec-agnostic**. | No |
| **2 — reconstructive** | Cut landed inside/after `Tracks`; only `Cluster`s survive | Walk clusters, group `SimpleBlock`s by track number, sniff each track's codec from its frame bytes, synthesize a `Tracks` element from frame-derived params, prepend `EBML`+`Segment`+`Info`, copy clusters verbatim. | No, for self-describing codecs |
| **3 — transplant** | Tier 2 cannot synthesize a track's `CodecPrivate` (Vorbis/AAC), or codec sniffing is ambiguous | Harvest a donor sibling's `EBML`+`Info`+`Tracks` (codec-agnostic, like the MP4 moov transplant), verify the donor's `TrackNumber`s cover the surviving clusters' block track numbers, splice donor head + surviving clusters. | `--reference` |

Tier selection happens inside one `matroska::rehead` entry point: `LightBeheading`
→ Tier 1; `HeavyBeheading` with all tracks sniffable → Tier 2; `HeavyBeheading`
with a donor-only track → Tier 3 if `--reference` given, else a loud, specific
failure naming the next rung.

## Architecture — new modules

These slot into the dependency stack documented in `CLAUDE.md`, mirroring the MP4
side.

### `ebml.rs` — the byte-level foundation

The EBML analog of the box helpers in `forensics.rs`/`mp4.rs`. Pure, hand-rolled,
no ffmpeg.

- `read_vint(buf, pos) -> Option<(u64, usize)>` and an ID variant that preserves
  the length-marker bits.
- `write_vint(value) -> Vec<u8>` — minimal-length data-size vint.
- Element walking: yield `(id, size, data_offset)` triples; handle unknown-size
  elements (`Segment`/`Cluster` may legally carry unknown size).
- ID constants for every element we read or write (`EBML`, `Segment`, `Info`,
  `TimecodeScale`, `Tracks`, `TrackEntry`, `TrackNumber`, `TrackType`, `CodecID`,
  `CodecPrivate`, `Video`/`PixelWidth`/`PixelHeight`, `Audio`/`SamplingFrequency`/
  `Channels`, `Cluster`, `Timecode`, `SimpleBlock`, `BlockGroup`, `Block`).
- Head **builders**: `el(id, payload)`, `uint(id, v)`, `string(id, s)`,
  `float(id, v)` — used to synthesize the reconstructed head.

### `matroska.rs` — analyzer + reconstructor

The analog of `mp4.rs` + `transplant.rs`.

- `analyze(data) -> MatroskaAnalysis`:
  - `Intact` — valid `EBML` header at offset 0; not our job (rescue's existing
    intact/ffmpeg path handles it).
  - `LightBeheading { segment_offset, segment_size }` — `Tracks` survive.
  - `HeavyBeheading { first_cluster, cluster_offsets, tracks: Vec<SniffedTrack>, bytes_lost }`.
  - `NoStructure` — give up.
- **Cluster walker**: enumerate clusters from `first_cluster` to EOF, yielding
  per block `(track_number, is_keyframe, rel_timecode, first_frame_bytes)`.
  Handles `SimpleBlock` and `BlockGroup`, and Xiph/EBML/fixed lacing well enough
  to recover the first laced frame for sniffing. (Clusters are copied verbatim,
  so full de-lacing is not required — only enough to read track numbers and a
  sample frame per track.)
- `rehead_surgical` (Tier 1): build `EBML` header, prepend to the surviving
  `Segment`. If the `Segment` size field was a known value that no longer
  matches, rewrite it as unknown-size; otherwise leave it.
- `rehead_reconstructive` (Tier 2): build `EBML` + `Segment`(unknown size) +
  `Info`(TimecodeScale 1_000_000, MuxingApp/WritingApp `basinski`) + `Tracks`
  (one `TrackEntry` per sniffed track via `mkv_codecs`), append verbatim
  clusters.
- `rehead_transplant` (Tier 3): extract donor `EBML`+`Info`+`Tracks`; drop the
  donor's `SeekHead`/`Cues` (their positions are wrong for the spliced file);
  verify track-number coverage; splice; append verbatim clusters.

### `mkv_codecs.rs` — codec sniff + per-codec head synthesis

The Tier-2 codec knowledge. Named `mkv_codecs` (not `webm_codecs`) because it also
covers H.264-in-Matroska. H.264 work is **delegated to the existing `h264.rs`**
(and `divine.rs`); this module owns the WebM-native bitstream parsing.

- `sniff(track_sample_frames) -> Codec`
- Per codec, an extractor that produces the `TrackEntry` body:
  - **VP9** — confirm `frame_marker == 10` and keyframe sync `0x498342`; read
    `frame_width_minus_1`/`frame_height_minus_1` from the keyframe uncompressed
    header. `CodecID "V_VP9"`, `Video{PixelWidth, PixelHeight}`, no
    `CodecPrivate`. (Proven on the sample: 1920×1080.)
  - **VP8** — keyframe start code `0x9d 01 2a`; 14-bit width/height. `V_VP8`.
  - **AV1** — parse the OBUs in the first keyframe's temporal unit (OBU header:
    `obu_type`, `obu_has_size_field`, LEB128 size), locate the
    `sequence_header_obu`, parse enough of it (`seq_profile`, `seq_level_idx`,
    bit-depth, `monochrome`, chroma subsampling, `max_frame_width/height`) to
    build the `av1C` `AV1CodecConfigurationRecord` and the `Video` dimensions.
    `V_AV1`. (Heaviest sniffer; included in the first cut.)
  - **Opus** — channel count from the TOC stereo bit (default 2), 48 kHz; emit a
    19-byte `OpusHead` `CodecPrivate`. `A_OPUS`,
    `Audio{SamplingFrequency 48000, Channels}`.
  - **H.264** — sniff the AVCC length-prefixed NAL chain (reuse
    `forensics::avcc_chain_len`). Harvest in-band SPS/PPS if present → build
    `avcC` via `h264.rs` (which also yields width/height). If SPS/PPS are not
    carried in-band (common for Matroska H.264), degrade to `divine.rs` or, if a
    `--reference` is supplied, Tier 3. `V_MPEG4/ISO/AVC`.
  - **Vorbis / AAC** — `CodecPrivate` (Vorbis codebooks / AAC
    `AudioSpecificConfig`) is unrecoverable from frame bytes → mark
    `NeedsDonor`, which forces Tier 3 or a loud failure.

### `rescue.rs` wiring

In the `Analysis::NoMoov` arm (before falling into `rescue_headerless_stream`),
call `matroska::analyze`. If it returns a beheading variant, route to
`matroska::rehead(input, output, &data, opts)`; otherwise continue to the
existing headerless-stream path. With this interception, a Matroska file no
longer reaches the format-dispatch `match` in `rescue_headerless_stream`, so it
stops hitting the catch-all `bail!` that produced the original error; that
catch-all stays in place for genuinely unknown formats.

### `ffx.rs` additions

- `remux_copy(input, output)` — stream-copy (`-c copy`) into Matroska. Output
  container is `.webm` when every track is WebM-legal (VP8/VP9/AV1 +
  Opus/Vorbis), else `.mkv`. Reuse existing `probe`, `decode_errors`,
  `keyframes`, `clip_from_keyframe`.
- `decode_errors` must be robust to the benign VP9 parser warnings observed in
  the spike (count genuine decode failures, not parser chatter).

## Data flow (Tier 2, the primary case)

1. `forensics` flags Matroska damaged-front → `rescue` calls `matroska::analyze`.
2. Confirm no `EBML`/`Segment`/`Tracks` survive; locate and validate the first
   clean `Cluster` (parse its `Timecode` + a few `SimpleBlock`s); enumerate
   cluster offsets to EOF.
3. For each track number seen, collect sample frames; sniff codec; synthesize the
   `TrackEntry`; collect any `NeedsDonor` verdicts.
4. If any track needs a donor and none is supplied → `bail!` with specific
   guidance (which track, which codec, why, and "pass `--reference <sibling from
   the same encoder>`").
5. Build the head (`EBML` DocType `webm` or `matroska` per codec set; `Segment`
   unknown-size; `Info`; `Tracks`).
6. Write head + verbatim clusters (first clean cluster → EOF) to a temp file.
7. **Validate #1:** `ffx::probe` must report the expected streams.
8. **Validate #2:** `ffx::remux_copy` to the final container must succeed.
9. **Validate #3:** `ffx::decode_errors` on the remuxed file ≈ 0.
10. Clip to the first empirically clean keyframe (leading clusters start mid-GOP
    referencing lost frames) unless `--no-clip`, reusing
    `first_clean_keyframe`/`clip_from_keyframe`.
11. Report bytes/seconds discarded from the orphaned pre-first-cluster region.
12. `finish()`.

## Error handling / honesty contract

Per `CLAUDE.md`: verify reconstructions against surviving bytes and fail loudly
when evidence is insufficient.

- Codec sniffing requires corroboration: VP9 sync on ≥N keyframe-flagged blocks;
  Opus TOC config consistent across ≥N frames; AVCC chain parse rate above
  threshold. Ambiguous sniff + `--reference` present → prefer the donor's
  `Tracks`.
- Each tier validates against the decoder before claiming success.
- The orphaned pre-first-cluster bytes are discarded and the loss reported in
  bytes and approximate seconds.
- Donor-only codecs without a donor produce a specific, actionable error.

## Testing

- `ebml.rs` unit tests: vint read/write round-trips, builder byte-exactness,
  element walk over a hand-built fixture.
- `mkv_codecs.rs` unit tests: dimension/param extraction on small embedded
  fixtures — a VP9 keyframe header (→ 1920×1080), a VP8 keyframe, an AV1
  sequence-header OBU, an Opus TOC, an AVCC NAL chain with in-band SPS/PPS.
- `tests/e2e.sh` extension: synthesize a WebM (`libvpx-vp9` + `libopus`), behead
  three ways — drop only the `EBML` header (Tier 1), cut through `Tracks`
  (Tier 2), and a Tier-3 case rescued with `--reference` — then rescue each and
  assert `ffprobe` reports the expected streams with zero decode errors. Add an
  MKV/H.264 case and an AV1 case, each guarded on encoder availability (matching
  the existing `libx264` guard; skip gracefully when `libvpx-vp9`/`libopus`/
  `libaom`/`libsvtav1` are absent).
- `samples/trey-bermudan-broke2.webm` becomes a casework note in
  `samples/NOTES.md` (and auto-memory).

## Documentation

- README: extend the mermaid decision tree with the Matroska/WebM ladder and add
  a section explaining the Segment-relative-offset advantage (re-heading is often
  lossless; codecs copied verbatim, never decoded — except H.264 SPS/PPS, which
  may need `divine`).
- README "Honest limitations": Vorbis/AAC re-heading needs a `--reference` donor
  from the same encoder.

## Out of scope

- Decoding/transcoding any codec during rescue (stream copy only).
- Recovering the orphaned pre-first-cluster bytes (discarded; reported).
- Repairing mid-file (interior) Matroska corruption — this design targets a
  damaged front only.
