#!/usr/bin/env bash
# End-to-end: generate a real mp4, cut its head off two ways, rescue both.
# Requires ffmpeg + ffprobe. Run from the repo root: bash tests/e2e.sh
set -euo pipefail

BIN=${BIN:-target/release/basinski}
# Portable across BSD (macOS) and GNU (Linux/CI) mktemp: a full template with
# trailing X's works on both; `-t <prefix>` does not (GNU needs the X's).
DIR=$(mktemp -d "${TMPDIR:-/tmp}/basinski-e2e.XXXXXX")
trap 'rm -rf "$DIR"' EXIT
cd "$DIR"

echo "==> generating 6s fixture (h264+aac, keyframe every 1s, moov at end)"
ffmpeg -v error -y \
  -f lavfi -i "testsrc2=size=640x360:rate=30:duration=6" \
  -f lavfi -i "sine=frequency=440:duration=6" \
  -c:v libx264 -preset fast -g 30 -c:a aac -shortest original.mp4

echo "==> head-truncation: clean (40-byte header) and deep (4096 bytes, into the first keyframe)"
tail -c +41   original.mp4 > head_truncated_clean.mp4
tail -c +4097 original.mp4 > head_truncated_deep.mp4

echo "==> sanity: stock ffmpeg cannot open the deep cut"
if ffprobe -v error head_truncated_deep.mp4 2>/dev/null; then
  echo "FAIL: expected ffprobe to choke on head_truncated_deep.mp4" >&2; exit 1
fi

cd - >/dev/null
echo "==> rescue: clean head-truncation (expect bit-faithful, all frames)"
"$BIN" rescue "$DIR/head_truncated_clean.mp4" -o "$DIR/clean_rescued.mp4"

echo "==> rescue: deep head-truncation (expect clip at first clean keyframe)"
"$BIN" rescue "$DIR/head_truncated_deep.mp4" -o "$DIR/deep_rescued.mp4"

echo "==> verifying"
frames_orig=$(ffprobe -v error -select_streams v:0 -count_packets \
  -show_entries stream=nb_read_packets -of csv=p=0 "$DIR/original.mp4")
frames_clean=$(ffprobe -v error -select_streams v:0 -count_packets \
  -show_entries stream=nb_read_packets -of csv=p=0 "$DIR/clean_rescued.mp4")
[ "$frames_orig" = "$frames_clean" ] || { echo "FAIL: frame count $frames_clean != $frames_orig" >&2; exit 1; }

md5_orig=$(ffmpeg -v error -i "$DIR/original.mp4" -map 0:v -f framemd5 - | tail -n +2 | cksum)
md5_clean=$(ffmpeg -v error -i "$DIR/clean_rescued.mp4" -map 0:v -f framemd5 - | tail -n +2 | cksum)
[ "$md5_orig" = "$md5_clean" ] || { echo "FAIL: decoded frames differ from original" >&2; exit 1; }

for f in clean_rescued deep_rescued; do
  errs=$(ffmpeg -v error -i "$DIR/$f.mp4" -f null - 2>&1 | wc -l | tr -d ' ')
  [ "$errs" = "0" ] || { echo "FAIL: $f.mp4 has $errs decode error lines" >&2; exit 1; }
done

dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$DIR/deep_rescued.mp4")

echo "==> moov transplant: sibling reference (same encoder DNA, different tape)"
ffmpeg -v error -y \
  -f lavfi -i "testsrc2=size=640x360:rate=30:duration=3" \
  -f lavfi -i "sine=frequency=880:duration=3" \
  -c:v libx264 -preset fast -g 30 -c:a aac -shortest "$DIR/reference.mp4"

orig_bytes=$(wc -c < "$DIR/original.mp4")
head -c $(( orig_bytes * 60 / 100 )) "$DIR/original.mp4" > "$DIR/tail_cut.mp4"

echo "==> sanity: stock ffmpeg cannot open the tail cut (moov is gone)"
if ffprobe -v error "$DIR/tail_cut.mp4" 2>/dev/null; then
  echo "FAIL: expected ffprobe to choke on tail_cut.mp4" >&2; exit 1
fi

echo "==> rescue: tail cut via moov transplant from the reference"
"$BIN" rescue "$DIR/tail_cut.mp4" --reference "$DIR/reference.mp4" -o "$DIR/transplanted.mp4"

frames_tx=$(ffprobe -v error -select_streams v:0 -count_packets \
  -show_entries stream=nb_read_packets -of csv=p=0 "$DIR/transplanted.mp4")
[ "$frames_tx" -ge 60 ] || { echo "FAIL: transplant recovered only $frames_tx frames" >&2; exit 1; }
errs=$(ffmpeg -v error -i "$DIR/transplanted.mp4" -f null - 2>&1 | wc -l | tr -d ' ')
[ "$errs" = "0" ] || { echo "FAIL: transplanted.mp4 has $errs decode error lines" >&2; exit 1; }

echo "==> divine: a double amputee — no container, no parameter sets, no donor"
ffmpeg -v error -y -f lavfi -i "testsrc2=size=854x480:rate=30:duration=4" \
  -f lavfi -i "sine=frequency=330:duration=4" \
  -c:v libx264 -preset medium -crf 21 -g 30 -c:a aac -shortest "$DIR/victim.mp4"
vsz=$(wc -c < "$DIR/victim.mp4")
tail -c +9000 "$DIR/victim.mp4" > "$DIR/victim.tail"   # (no tail|head pipe: SIGPIPE + pipefail)
head -c $(( vsz * 60 / 100 )) "$DIR/victim.tail" > "$DIR/headless.bin"
"$BIN" divine "$DIR/headless.bin" -o "$DIR/divined_donor.mp4" | grep -E "divined|donor written" | sed 's/^/    /'
"$BIN" rescue "$DIR/headless.bin" --reference "$DIR/divined_donor.mp4" -o "$DIR/resurrected.mp4" >/dev/null

errs=$(ffmpeg -v error -i "$DIR/resurrected.mp4" -f null - 2>&1 | wc -l | tr -d ' ')
[ "$errs" = "0" ] || { echo "FAIL: resurrected.mp4 has $errs decode error lines" >&2; exit 1; }
frames_div=$(ffprobe -v error -select_streams v:0 -count_packets \
  -show_entries stream=nb_read_packets -of csv=p=0 "$DIR/resurrected.mp4")
[ "$frames_div" -ge 30 ] || { echo "FAIL: divine rescue recovered only $frames_div frames" >&2; exit 1; }
res_div=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$DIR/resurrected.mp4")
[ "$res_div" = "854,480" ] || { echo "FAIL: divined resolution $res_div != 854,480" >&2; exit 1; }

echo "    clean rescue : $frames_clean/$frames_orig frames, decoded bit-identical"
echo "    deep rescue  : ${dur}s of 6s recovered, 0 decode errors"
echo "    transplant   : $frames_tx/$frames_orig frames from a 60% tail cut, 0 decode errors"
echo "    divine       : $frames_div frames at $res_div from a bare payload, no donor, 0 decode errors"

# --- Matroska/WebM reconstructive re-head (VP9 + Opus, heavy beheading) ---
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libvpx-vp9 \
   && ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libopus; then

  echo "==> WebM: synthesizing 6s VP9+Opus fixture"
  ffmpeg -v error -y \
    -f lavfi -i "testsrc2=size=640x360:rate=30:duration=6" \
    -f lavfi -i "sine=frequency=440:duration=6" \
    -c:v libvpx-vp9 -b:v 600k -g 24 -c:a libopus -b:a 64k \
    "$DIR/src.webm"

  echo "==> WebM: beheading (drop everything before the 2nd Cluster; Tracks element gone)"
  python3 - "$DIR/src.webm" "$DIR/beheaded.webm" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
sig = bytes.fromhex("1F43B675")
first = data.find(sig)
second = data.find(sig, first + 1)
cut = second if second != -1 else first
open(sys.argv[2], "wb").write(data[cut:])
PY

  echo "==> WebM: rescue beheaded.webm -> rescued.webm"
  "$BIN" rescue "$DIR/beheaded.webm" -o "$DIR/rescued.webm"

  echo "==> WebM: verifying codec streams and decode quality"
  probe=$(ffprobe -v error -show_entries stream=codec_name -of csv=p=0 "$DIR/rescued.webm" | tr '\n' ',')
  echo "$probe" | grep -q vp9  || { echo "FAIL: no vp9 stream in rescued.webm (got: $probe)" >&2; exit 1; }
  echo "$probe" | grep -q opus || { echo "FAIL: no opus stream in rescued.webm (got: $probe)" >&2; exit 1; }
  webm_errs=$(ffmpeg -v error -i "$DIR/rescued.webm" -f null - 2>&1 | wc -l | tr -d ' ')
  [ "$webm_errs" = "0" ] || { echo "FAIL: rescued.webm has $webm_errs decode error lines" >&2; exit 1; }

  echo "    WebM VP9+Opus: reconstructive re-head, 0 decode errors"
  echo "PASS: WebM VP9/Opus reconstructive re-head"

else
  echo "skip: WebM reconstructive case needs libvpx-vp9 + libopus (not found)"
fi

# --- Honest-refusal test: unsupported codec beheaded WebM must exit non-zero ---
# Preferred: VP9 video + Vorbis audio (exercises audio-codec mislabel guard).
# Fallback:  VP8 video + Opus audio (exercises video-codec mislabel guard).
# Skip if neither encoder is available.
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libvpx-vp9 \
   && ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libvorbis; then

  echo "==> WebM refusal: synthesizing VP9+Vorbis fixture (unsupported audio codec)"
  ffmpeg -v error -y \
    -f lavfi -i "testsrc2=size=320x240:rate=30:duration=3" \
    -f lavfi -i "sine=frequency=440:duration=3" \
    -c:v libvpx-vp9 -b:v 200k -g 24 -c:a libvorbis -b:a 64k \
    "$DIR/src_unsupported.webm"

  python3 - "$DIR/src_unsupported.webm" "$DIR/beheaded_unsupported.webm" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
sig = bytes.fromhex("1F43B675")
first = data.find(sig)
second = data.find(sig, first + 1)
cut = second if second != -1 else first
open(sys.argv[2], "wb").write(data[cut:])
PY

  echo "==> WebM refusal: basinski rescue must refuse (non-zero exit) on VP9+Vorbis"
  if "$BIN" rescue "$DIR/beheaded_unsupported.webm" -o "$DIR/should_not_exist.webm" 2>/dev/null; then
    echo "FAIL: basinski produced output for an unsupported-codec beheaded WebM (should refuse)" >&2
    exit 1
  fi
  echo "PASS: unsupported-codec beheaded WebM refused loudly"

elif ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libvpx \
     && ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libopus; then

  echo "==> WebM refusal: synthesizing VP8+Opus fixture (unsupported video codec)"
  ffmpeg -v error -y \
    -f lavfi -i "testsrc2=size=320x240:rate=30:duration=3" \
    -f lavfi -i "sine=frequency=440:duration=3" \
    -c:v libvpx -b:v 200k -g 24 -c:a libopus -b:a 64k \
    "$DIR/src_unsupported.webm"

  python3 - "$DIR/src_unsupported.webm" "$DIR/beheaded_unsupported.webm" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
sig = bytes.fromhex("1F43B675")
first = data.find(sig)
second = data.find(sig, first + 1)
cut = second if second != -1 else first
open(sys.argv[2], "wb").write(data[cut:])
PY

  echo "==> WebM refusal: basinski rescue must refuse (non-zero exit) on VP8+Opus"
  if "$BIN" rescue "$DIR/beheaded_unsupported.webm" -o "$DIR/should_not_exist.webm" 2>/dev/null; then
    echo "FAIL: basinski produced output for an unsupported-codec beheaded WebM (should refuse)" >&2
    exit 1
  fi
  echo "PASS: unsupported-codec beheaded WebM refused loudly"

else
  echo "skip: unsupported-codec WebM refusal test needs libvorbis (VP9+Vorbis) or libvpx/VP8 (VP8+Opus) — neither found"
fi

echo "PASS"
