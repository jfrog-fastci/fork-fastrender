#!/usr/bin/env bash
set -euo pipefail

# Smoke-test `scripts/chrome_baseline.sh` against a real Chrome/Chromium binary.
#
# This verifies the headless window "viewport padding" workaround:
# - Chrome's `--window-size=WxH` controls the outer window size
# - but in headless screenshot mode the CSS/layout viewport height is shorter by a fixed amount
#   (see HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX).
# - `scripts/chrome_baseline.sh` compensates by adding the pad then cropping the PNG back down.
#
# This script:
# 1) writes a tiny test HTML page with a solid red bar pinned to the bottom of the viewport,
# 2) runs `scripts/chrome_baseline.sh` on it (for multiple DPRs),
# 3) also runs `scripts/chrome_baseline.sh` against a representative offline fixture HTML
#    (`tests/pages/fixtures/br_linebreak/index.html`) to validate the cached-HTML path,
# 3) asserts the output PNG dimensions match the requested viewport exactly, and
# 4) asserts the bottom strip is red (heuristic that catches pad mismatch).
#
# Usage:
#   scripts/verify_chrome_baseline_viewport.sh
#
# Environment:
#   CHROME_BIN=/path/to/chrome
#   VIEWPORT=320x240
#   DPRS=1.0,1.333
#   FIXTURE_HTML=tests/pages/fixtures/br_linebreak/index.html
#   HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX=88  (override if your Chrome/OS differs)
#   KEEP_TMP=1  (keep the temporary output directory for debugging)

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

VIEWPORT="${VIEWPORT:-320x240}"
DPRS="${DPRS:-1.0,1.333}"
FIXTURE_HTML="${FIXTURE_HTML:-tests/pages/fixtures/br_linebreak/index.html}"
KEEP_TMP="${KEEP_TMP:-0}"

if ! [[ "${VIEWPORT}" =~ ^[0-9]+x[0-9]+$ ]]; then
  echo "invalid VIEWPORT: ${VIEWPORT} (expected WxH like 320x240)" >&2
  exit 2
fi

VIEWPORT_W="${VIEWPORT%x*}"
VIEWPORT_H="${VIEWPORT#*x}"

IFS=',' read -r -a DPR_VALUES <<<"${DPRS}"
if [[ "${#DPR_VALUES[@]}" -eq 0 ]]; then
  echo "invalid DPRS: ${DPRS} (expected a comma-separated list like 1.0,2.0)" >&2
  exit 2
fi
for raw_dpr in "${DPR_VALUES[@]}"; do
  dpr="$(echo "${raw_dpr}" | xargs)"
  if [[ -z "${dpr}" ]]; then
    echo "invalid DPRS entry: empty value in ${DPRS}" >&2
    exit 2
  fi
  if ! [[ "${dpr}" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    echo "invalid DPR value: ${dpr} (from DPRS=${DPRS}; expected a positive number like 1.0)" >&2
    exit 2
  fi
done

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required." >&2
  exit 2
fi

tmp_root="$(mktemp -d)"
cleanup() {
  if [[ "${KEEP_TMP}" == "1" ]]; then
    echo "Keeping temp dir: ${tmp_root}" >&2
  else
    rm -rf "${tmp_root}"
  fi
}
trap cleanup EXIT

html_dir="${tmp_root}/html"
out_dir="${tmp_root}/out"
mkdir -p "${html_dir}" "${out_dir}"

base_stem="viewport_pad_smoke"
html_template='<!doctype html>
<meta charset="utf-8">
<title>chrome_baseline viewport pad smoke</title>
<style>
  html, body {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100%;
    background: rgb(0, 255, 0);
  }

  #bottom {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    height: 24px;
    background: rgb(255, 0, 0);
  }
</style>
<div id="bottom"></div>'

# Write the per-DPR HTML snapshots + sidecars. (The content is identical; we duplicate so the
# outputs have distinct stems.)
for raw_dpr in "${DPR_VALUES[@]}"; do
  dpr="$(echo "${raw_dpr}" | xargs)"
  dpr_stem="${dpr//./_}"
  stem="${base_stem}_dpr_${dpr_stem}"
  printf '%s\n' "${html_template}" >"${html_dir}/${stem}.html"
  # Provide a harmless base URL so `<base href=...>` injection is exercised.
  echo "url: file://${REPO_ROOT}/" >"${html_dir}/${stem}.html.meta"
done

fixture_stem="viewport_pad_fixture"
fixture_src="${REPO_ROOT}/${FIXTURE_HTML}"
if [[ -f "${fixture_src}" ]]; then
  cp "${fixture_src}" "${html_dir}/${fixture_stem}.html"
  fixture_dir="$(cd "$(dirname "${fixture_src}")" && pwd)"
  echo "url: file://${fixture_dir}/" >"${html_dir}/${fixture_stem}.html.meta"
else
  echo "warning: fixture not found: ${FIXTURE_HTML} (skipping fixture dimension check)" >&2
  fixture_stem=""
fi

echo "Viewport: ${VIEWPORT}"
echo "DPR(s):   ${DPRS}"
if [[ -n "${fixture_stem}" ]]; then
  echo "Fixture:  ${FIXTURE_HTML}"
fi
if [[ -n "${CHROME_BIN:-}" ]]; then
  echo "Chrome:    ${CHROME_BIN}"
fi
if [[ -n "${HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX:-}" ]]; then
  echo "Pad px:    ${HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX}"
fi
echo

for raw_dpr in "${DPR_VALUES[@]}"; do
  dpr="$(echo "${raw_dpr}" | xargs)"
  dpr_stem="${dpr//./_}"
  stem="${base_stem}_dpr_${dpr_stem}"

  echo "Running chrome_baseline.sh (dpr=${dpr})..."
  if ! scripts/chrome_baseline.sh \
    --html-dir "${html_dir}" \
    --out-dir "${out_dir}" \
    --viewport "${VIEWPORT}" \
    --dpr "${dpr}" \
    --timeout 30 \
    -- \
    "${stem}"; then
    echo "chrome_baseline.sh failed; log follows:" >&2
    cat "${out_dir}/${stem}.chrome.log" >&2 || true
    exit 1
  fi

  png="${out_dir}/${stem}.png"
  log="${out_dir}/${stem}.chrome.log"
  if [[ ! -s "${png}" ]]; then
    echo "missing output PNG: ${png}" >&2
    cat "${log}" >&2 || true
    exit 1
  fi

  python3 - "${png}" "${VIEWPORT_W}" "${VIEWPORT_H}" "${dpr}" <<'PY'
import struct
import sys
import zlib
import math

png_path = sys.argv[1]
viewport_w_css = int(sys.argv[2])
viewport_h_css = int(sys.argv[3])
dpr = float(sys.argv[4])

def round_half_up(x: float) -> int:
    return int(math.floor(x + 0.5))

expected_w = max(1, round_half_up(viewport_w_css * dpr))
expected_h = max(1, round_half_up(viewport_h_css * dpr))

PNG_SIG = b"\x89PNG\r\n\x1a\n"

def parse_chunks(data: bytes):
    if not data.startswith(PNG_SIG):
        raise AssertionError("input is not a PNG (bad signature)")
    off = len(PNG_SIG)
    while off + 8 <= len(data):
        length = struct.unpack(">I", data[off : off + 4])[0]
        ctype = data[off + 4 : off + 8]
        off += 8
        if off + length + 4 > len(data):
            raise AssertionError(f"truncated PNG chunk {ctype!r}")
        chunk = data[off : off + length]
        off += length
        off += 4  # crc
        yield ctype, chunk
        if ctype == b"IEND":
            break

def paeth(a: int, b: int, c: int) -> int:
    p = a + b - c
    pa = abs(p - a)
    pb = abs(p - b)
    pc = abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c

def unfilter(raw: bytes, width: int, height: int, bpp: int):
    stride = width * bpp
    expected = height * (stride + 1)
    if len(raw) < expected:
        raise AssertionError(
            f"decompressed IDAT is too small: {len(raw)} bytes, expected {expected}"
        )
    raw = raw[:expected]
    rows = []
    prev = bytearray(stride)
    i = 0
    for _ in range(height):
        f = raw[i]
        i += 1
        scan = raw[i : i + stride]
        i += stride
        cur = bytearray(stride)
        if f == 0:  # None
            cur[:] = scan
        elif f == 1:  # Sub
            for x in range(stride):
                left = cur[x - bpp] if x >= bpp else 0
                cur[x] = (scan[x] + left) & 0xFF
        elif f == 2:  # Up
            for x in range(stride):
                cur[x] = (scan[x] + prev[x]) & 0xFF
        elif f == 3:  # Average
            for x in range(stride):
                left = cur[x - bpp] if x >= bpp else 0
                up = prev[x]
                cur[x] = (scan[x] + ((left + up) // 2)) & 0xFF
        elif f == 4:  # Paeth
            for x in range(stride):
                left = cur[x - bpp] if x >= bpp else 0
                up = prev[x]
                up_left = prev[x - bpp] if x >= bpp else 0
                cur[x] = (scan[x] + paeth(left, up, up_left)) & 0xFF
        else:
            raise AssertionError(f"unsupported PNG filter type {f}")
        rows.append(cur)
        prev = cur
    return rows

data = open(png_path, "rb").read()
ihdr = None
idat = bytearray()
for ctype, chunk in parse_chunks(data):
    if ctype == b"IHDR":
        ihdr = chunk
    elif ctype == b"IDAT":
        idat.extend(chunk)

if ihdr is None:
    raise AssertionError("missing IHDR chunk")

width, height, bit_depth, color_type, compression, filter_method, interlace = struct.unpack(
    ">IIBBBBB", ihdr
)
if (width, height) != (expected_w, expected_h):
    raise AssertionError(
        f"output PNG is {width}x{height}, expected {expected_w}x{expected_h}"
    )

if compression != 0 or filter_method != 0 or interlace != 0:
    raise AssertionError("unsupported PNG encoding (expected compression=0, filter=0, interlace=0)")

if bit_depth != 8:
    raise AssertionError(f"unsupported PNG bit depth {bit_depth} (expected 8)")

if color_type == 6:  # RGBA
    bpp = 4
elif color_type == 2:  # RGB
    bpp = 3
else:
    raise AssertionError(f"unsupported PNG color type {color_type} (expected 2 or 6)")

raw = zlib.decompress(bytes(idat))
rows = unfilter(raw, width, height, bpp)

strip_h = min(5, height)
start = height - strip_h
redish = 0
total = 0

for y in range(start, height):
    row = rows[y]
    for x in range(width):
        idx = x * bpp
        r = row[idx]
        g = row[idx + 1]
        b = row[idx + 2]
        # Loose thresholds to avoid being brittle across encoders/color profiles.
        if r >= 200 and g <= 80 and b <= 80:
            redish += 1
        total += 1

ratio = redish / total if total else 0.0
if ratio < 0.95:
    raise AssertionError(
        f"bottom strip is not red enough (redish_ratio={ratio:.3f}, expected >= 0.95). "
        "This usually means the headless viewport height pad/crop logic is wrong. "
        "Try overriding HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX if you see a persistent "
        "white bar at the bottom."
    )

print(f"ok: {width}x{height}, redish_ratio={ratio:.3f}")
PY
done

if [[ -n "${fixture_stem}" ]]; then
  echo "Running chrome_baseline.sh (fixture, dpr=1.0)..."
  if ! scripts/chrome_baseline.sh \
    --html-dir "${html_dir}" \
    --out-dir "${out_dir}" \
    --viewport "${VIEWPORT}" \
    --dpr 1.0 \
    --timeout 30 \
    -- \
    "${fixture_stem}"; then
    echo "chrome_baseline.sh failed for fixture; log follows:" >&2
    cat "${out_dir}/${fixture_stem}.chrome.log" >&2 || true
    exit 1
  fi

  fixture_png="${out_dir}/${fixture_stem}.png"
  if [[ ! -s "${fixture_png}" ]]; then
    echo "missing output PNG for fixture: ${fixture_png}" >&2
    cat "${out_dir}/${fixture_stem}.chrome.log" >&2 || true
    exit 1
  fi

  python3 - "${fixture_png}" "${VIEWPORT_W}" "${VIEWPORT_H}" <<'PY'
import struct
import sys

png_path = sys.argv[1]
expected_w = int(sys.argv[2])
expected_h = int(sys.argv[3])

data = open(png_path, "rb").read()
if not data.startswith(b"\x89PNG\r\n\x1a\n"):
    raise AssertionError("fixture output is not a PNG (bad signature)")

off = 8
length = struct.unpack(">I", data[off : off + 4])[0]
off += 4
ctype = data[off : off + 4]
off += 4
if ctype != b"IHDR":
    raise AssertionError("fixture PNG missing IHDR chunk")
ihdr = data[off : off + length]
width, height = struct.unpack(">II", ihdr[:8])

if (width, height) != (expected_w, expected_h):
    raise AssertionError(
        f"fixture output PNG is {width}x{height}, expected {expected_w}x{expected_h}"
    )

print(f"fixture ok: {width}x{height}")
PY
fi

echo
echo "Success."
