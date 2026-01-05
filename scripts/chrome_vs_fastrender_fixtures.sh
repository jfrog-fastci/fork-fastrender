#!/usr/bin/env bash
set -euo pipefail

# Convenience wrapper around `cargo xtask fixture-chrome-diff`.
#
# This script exists mainly for backwards-compatible flags and muscle memory. The canonical
# implementation of the offline fixture evidence loop lives in the `xtask` subcommand; keep this
# wrapper thin so it inherits new validation/selection logic automatically.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

usage() {
  cat <<'EOF'
usage: scripts/chrome_vs_fastrender_fixtures.sh [options] [--] [fixture_glob...]

Options:
  --fixtures-dir <dir>      Fixture root (default: tests/pages/fixtures)
  --out-dir <dir>           Base output dir (default: target/fixture_chrome_diff)
  --chrome-out-dir <dir>    (legacy) Must be <out-dir>/chrome
  --fastr-out-dir <dir>     (legacy) Must be <out-dir>/fastrender
  --report-html <path>      (legacy) Must be <out-dir>/report.html
  --report-json <path>      (legacy) Must be <out-dir>/report.json
  --viewport <WxH>          Viewport size (default: inherited from `cargo xtask fixture-chrome-diff`)
  --dpr <float>             Device pixel ratio (default: inherited from `cargo xtask fixture-chrome-diff`)
  --media <screen|print>    Media type for both Chrome + FastRender (default: inherited from `cargo xtask fixture-chrome-diff`)
  --jobs <n>, -j <n>        Parallelism forwarded to render_fixtures
  --write-snapshot          Also write render_fixtures snapshots/diagnostics (for diff_snapshots)
  --timeout <secs>          Per-fixture timeout (Chrome + FastRender) (default: inherited from `cargo xtask fixture-chrome-diff`)
  --chrome <path>           Chrome/Chromium binary (default: auto-detect)
  --js <on|off>             Enable JavaScript in Chrome (default: inherited from `cargo xtask fixture-chrome-diff`)
  --shard <index>/<total>   Only process a deterministic shard of fixtures (0-based)
  --tolerance <0-255>       Pixel diff tolerance (passed to diff_renders)
  --max-diff-percent <f64>  Allowed diff percent (passed to diff_renders)
  --max-perceptual-distance <f64>
                            Allowed perceptual distance (passed to diff_renders)
  --ignore-alpha            Ignore alpha differences (passed to diff_renders)
  --sort-by <mode>          Sort report entries (pixel|percent|perceptual) (passed to diff_renders)
  --no-chrome               Skip generating Chrome baseline renders (reuse existing --chrome-out-dir)
  --no-fastrender           Skip generating FastRender renders (reuse existing --fastr-out-dir)
  --diff-only               Alias for --no-chrome --no-fastrender
  --fail-on-differences     Exit non-zero when diff_renders reports differences (default: keep report and exit 0)
  --no-build                Skip `cargo build --release --bin diff_renders` (reuse an existing binary)
  --no-clean                (deprecated; ignored) Output dirs are managed by `cargo xtask fixture-chrome-diff`.
  -h, --help                Show help

Filtering:
  Positional args are treated as fixture directory globs (matched against
  <fixtures-dir>/<glob>/index.html), then forwarded to `cargo xtask fixture-chrome-diff --fixtures <csv>`.
  If omitted, fixture selection defaults to the xtask implementation.
  Some additional `fixture-chrome-diff` selection/validation flags (e.g. `--all-fixtures`, `--from-progress`)
  are also accepted and forwarded. For everything else, run `cargo xtask fixture-chrome-diff --help`.

Output layout (matches `cargo xtask fixture-chrome-diff`):
  <out>/chrome/        Chrome PNGs/logs/metadata
  <out>/fastrender/    FastRender PNGs/logs/diagnostics
  <out>/report.html    diff_renders HTML report
  <out>/report.json    diff_renders JSON report
EOF
}

# Allow `--help` to work even on older bash versions (macOS ships bash 3.2).
for arg in "$@"; do
  case "$arg" in
    -h|--help)
      usage
      exit 0
      ;;
  esac
done

# The wrapper uses bash 4+ features (associative arrays, `${var,,}`).
if [[ "${BASH_VERSINFO[0]:-0}" -lt 4 ]]; then
  echo "error: ${0##*/} requires bash >= 4 (found ${BASH_VERSION:-unknown})." >&2
  echo "On macOS, install a newer bash (e.g. \`brew install bash\`) and ensure it is first in PATH." >&2
  exit 2
fi

FIXTURES_DIR_SET=0
if [[ -n "${FIXTURES_DIR:-}" ]]; then
  FIXTURES_DIR_SET=1
else
  FIXTURES_DIR="tests/pages/fixtures"
fi

OUT_DIR_SET=0
if [[ -n "${OUT_DIR:-}" ]]; then
  OUT_DIR_SET=1
else
  OUT_DIR="target/fixture_chrome_diff"
fi
OUT_DIR_EXPLICIT=0
VIEWPORT="${VIEWPORT:-}"
DPR="${DPR:-}"
MEDIA="${MEDIA:-}"
JOBS=""
WRITE_SNAPSHOT=0
TIMEOUT="${TIMEOUT:-}"
CHROME_BIN="${CHROME_BIN:-}"
JS="${JS:-}"
SHARD=""
TOLERANCE=""
MAX_DIFF_PERCENT=""
MAX_PERCEPTUAL_DISTANCE=""
IGNORE_ALPHA=0
SORT_BY=""
FAIL_ON_DIFFERENCES=0
NO_CHROME=0
NO_FASTRENDER=0
DIFF_ONLY=0
NO_BUILD=0
NO_CLEAN=0

LEGACY_CHROME_OUT_DIR=""
LEGACY_FASTR_OUT_DIR=""
LEGACY_REPORT_HTML=""
LEGACY_REPORT_JSON=""

EXTRA_XTASK_ARGS=()
EXPLICIT_FIXTURES=""
HAS_EXPLICIT_FIXTURES=0
DISABLE_POSITIONAL_FIXTURES=0
FIT_CANVAS_TO_CONTENT=0

FILTERS=()
PARSE_FLAGS=1
while [[ $# -gt 0 ]]; do
  if [[ "${PARSE_FLAGS}" -eq 1 ]]; then
    case "$1" in
      -h|--help)
        usage
        exit 0
        ;;
      --fixtures-dir=*)
        FIXTURES_DIR="${1#*=}"; FIXTURES_DIR_SET=1; shift; continue ;;
      --fixtures-dir)
        FIXTURES_DIR="${2:-}"; FIXTURES_DIR_SET=1; shift 2; continue ;;
      --out-dir=*)
        OUT_DIR="${1#*=}"; OUT_DIR_SET=1; OUT_DIR_EXPLICIT=1; shift; continue ;;
      --out-dir)
        OUT_DIR="${2:-}"; OUT_DIR_SET=1; OUT_DIR_EXPLICIT=1; shift 2; continue ;;
      --chrome-out-dir)
        LEGACY_CHROME_OUT_DIR="${2:-}"; shift 2; continue ;;
      --fastr-out-dir)
        LEGACY_FASTR_OUT_DIR="${2:-}"; shift 2; continue ;;
      --report-html)
        LEGACY_REPORT_HTML="${2:-}"; shift 2; continue ;;
      --report-json)
        LEGACY_REPORT_JSON="${2:-}"; shift 2; continue ;;
      --viewport=*)
        VIEWPORT="${1#*=}"; shift; continue ;;
      --viewport)
        VIEWPORT="${2:-}"; shift 2; continue ;;
      --dpr=*)
        DPR="${1#*=}"; shift; continue ;;
      --dpr)
        DPR="${2:-}"; shift 2; continue ;;
      --media=*)
        MEDIA="${1#*=}"; shift; continue ;;
      --media)
        MEDIA="${2:-}"; shift 2; continue ;;
      --jobs=*)
        JOBS="${1#*=}"; shift; continue ;;
      --jobs)
        JOBS="${2:-}"; shift 2; continue ;;
      -j=*)
        JOBS="${1#*=}"; shift; continue ;;
      -j)
        JOBS="${2:-}"; shift 2; continue ;;
      --write-snapshot)
        WRITE_SNAPSHOT=1; shift; continue ;;
      --timeout=*)
        TIMEOUT="${1#*=}"; shift; continue ;;
      --timeout)
        TIMEOUT="${2:-}"; shift 2; continue ;;
      --chrome=*)
        CHROME_BIN="${1#*=}"; shift; continue ;;
      --chrome)
        CHROME_BIN="${2:-}"; shift 2; continue ;;
      --js=*)
        JS="${1#*=}"; shift; continue ;;
      --js)
        JS="${2:-}"; shift 2; continue ;;
      --shard=*)
        SHARD="${1#*=}"; shift; continue ;;
      --shard)
        SHARD="${2:-}"; shift 2; continue ;;
      --tolerance=*)
        TOLERANCE="${1#*=}"; shift; continue ;;
      --tolerance)
        TOLERANCE="${2:-}"; shift 2; continue ;;
      --max-diff-percent=*)
        MAX_DIFF_PERCENT="${1#*=}"; shift; continue ;;
      --max-diff-percent)
        MAX_DIFF_PERCENT="${2:-}"; shift 2; continue ;;
      --max-perceptual-distance=*)
        MAX_PERCEPTUAL_DISTANCE="${1#*=}"; shift; continue ;;
      --max-perceptual-distance)
        MAX_PERCEPTUAL_DISTANCE="${2:-}"; shift 2; continue ;;
      --ignore-alpha)
        IGNORE_ALPHA=1; shift; continue ;;
      --sort-by=*)
        SORT_BY="${1#*=}"; shift; continue ;;
      --sort-by)
        SORT_BY="${2:-}"; shift 2; continue ;;
      --no-chrome)
        NO_CHROME=1; shift; continue ;;
      --no-fastrender)
        NO_FASTRENDER=1; shift; continue ;;
      --diff-only)
        DIFF_ONLY=1; shift; continue ;;
      --fail-on-differences)
        FAIL_ON_DIFFERENCES=1; shift; continue ;;
      --no-build)
        NO_BUILD=1; shift; continue ;;
      --no-clean)
        NO_CLEAN=1; shift; continue ;;
      --fit-canvas-to-content)
        FIT_CANVAS_TO_CONTENT=1; shift; continue ;;
      --fixtures=*)
        EXPLICIT_FIXTURES="${1#*=}"; HAS_EXPLICIT_FIXTURES=1; shift; continue ;;
      --fixtures)
        EXPLICIT_FIXTURES="${2:-}"; HAS_EXPLICIT_FIXTURES=1; shift 2; continue ;;
      --all-fixtures)
        EXTRA_XTASK_ARGS+=(--all-fixtures); DISABLE_POSITIONAL_FIXTURES=1; shift; continue ;;
      --from-progress=*)
        EXTRA_XTASK_ARGS+=(--from-progress "${1#*=}"); DISABLE_POSITIONAL_FIXTURES=1; shift; continue ;;
      --from-progress)
        EXTRA_XTASK_ARGS+=(--from-progress "${2:-}"); DISABLE_POSITIONAL_FIXTURES=1; shift 2; continue ;;
      --only-failures)
        EXTRA_XTASK_ARGS+=(--only-failures); DISABLE_POSITIONAL_FIXTURES=1; shift; continue ;;
      --top-worst-accuracy=*)
        EXTRA_XTASK_ARGS+=(--top-worst-accuracy "${1#*=}"); DISABLE_POSITIONAL_FIXTURES=1; shift; continue ;;
      --top-worst-accuracy)
        EXTRA_XTASK_ARGS+=(--top-worst-accuracy "${2:-}"); DISABLE_POSITIONAL_FIXTURES=1; shift 2; continue ;;
      --min-diff-percent=*)
        EXTRA_XTASK_ARGS+=(--min-diff-percent "${1#*=}"); shift; continue ;;
      --min-diff-percent)
        EXTRA_XTASK_ARGS+=(--min-diff-percent "${2:-}"); shift 2; continue ;;
      --skip-missing-fixtures)
        EXTRA_XTASK_ARGS+=(--skip-missing-fixtures); DISABLE_POSITIONAL_FIXTURES=1; shift; continue ;;
      --require-fastrender-metadata)
        EXTRA_XTASK_ARGS+=(--require-fastrender-metadata); shift; continue ;;
      --allow-stale-fastrender-renders)
        EXTRA_XTASK_ARGS+=(--allow-stale-fastrender-renders); shift; continue ;;
      --require-chrome-metadata)
        EXTRA_XTASK_ARGS+=(--require-chrome-metadata); shift; continue ;;
      --allow-stale-chrome-baselines)
        EXTRA_XTASK_ARGS+=(--allow-stale-chrome-baselines); shift; continue ;;
      --chrome-dir=*)
        EXTRA_XTASK_ARGS+=(--chrome-dir "${1#*=}"); shift; continue ;;
      --chrome-dir)
        EXTRA_XTASK_ARGS+=(--chrome-dir "${2:-}"); shift 2; continue ;;
      --dry-run)
        EXTRA_XTASK_ARGS+=(--dry-run); shift; continue ;;
      -*)
        echo "unknown option: $1" >&2
        echo "Run \`cargo xtask fixture-chrome-diff --help\` for the canonical flag set." >&2
        echo "If you meant to pass a fixture glob that begins with '-', put it after '--'." >&2
        exit 2
        ;;
      --)
        PARSE_FLAGS=0
        shift
        continue
        ;;
    esac
  fi

  FILTERS+=("$1")
  shift
done

refuse_unsafe_path() {
  local label="$1"
  local value="$2"
  if [[ -z "${value}" || "${value}" == "/" ]]; then
    echo "refusing to use unsafe ${label}: ${value}" >&2
    exit 2
  fi
}

refuse_unsafe_path "fixtures dir" "${FIXTURES_DIR}"
refuse_unsafe_path "out dir" "${OUT_DIR}"

if [[ "${NO_CLEAN}" -eq 1 ]]; then
  echo "warning: --no-clean is deprecated and ignored (use --no-chrome/--no-fastrender to reuse outputs)." >&2
fi

if [[ "${MEDIA,,}" == "print" ]]; then
  # Historically the wrapper auto-enabled this for print mode so multi-page fixtures weren't clipped.
  FIT_CANVAS_TO_CONTENT=1
fi

resolve_fixture_patterns() {
  local -a patterns=("$@")
  local -a fixtures=()
  declare -A seen=()

  shopt -s nullglob
  for pat in "${patterns[@]}"; do
    local matched=0
    for dir in "${FIXTURES_DIR}"/${pat}; do
      if [[ -d "${dir}" && -f "${dir}/index.html" ]]; then
        local stem
        stem="$(basename "${dir}")"
        if [[ -z "${seen[${stem}]:-}" ]]; then
          seen["${stem}"]=1
          fixtures+=("${stem}")
        fi
        matched=1
      fi
    done
    if [[ "${matched}" -eq 0 ]]; then
      echo "no fixtures matched pattern: ${pat}" >&2
      exit 1
    fi
  done

  printf '%s\n' "${fixtures[@]}" | sort -u
}

infer_out_dir_from_legacy_path() {
  local kind="$1"
  local path="$2"
  local expected_basename="$3"
  if [[ -z "${path}" ]]; then
    return 0
  fi
  refuse_unsafe_path "${kind}" "${path}"
  local base
  base="$(basename -- "${path}")"
  if [[ "${base}" != "${expected_basename}" ]]; then
    echo "${kind} must be ${expected_basename} to map onto the xtask output layout; got: ${path}" >&2
    echo "Use --out-dir to control the output root instead." >&2
    exit 2
  fi
  dirname -- "${path}"
}

LEGACY_OUT_DIRS=()
if [[ -n "${LEGACY_CHROME_OUT_DIR}" ]]; then
  LEGACY_OUT_DIRS+=("$(infer_out_dir_from_legacy_path "--chrome-out-dir" "${LEGACY_CHROME_OUT_DIR}" "chrome")")
fi
if [[ -n "${LEGACY_FASTR_OUT_DIR}" ]]; then
  LEGACY_OUT_DIRS+=("$(infer_out_dir_from_legacy_path "--fastr-out-dir" "${LEGACY_FASTR_OUT_DIR}" "fastrender")")
fi
if [[ -n "${LEGACY_REPORT_HTML}" ]]; then
  LEGACY_OUT_DIRS+=("$(infer_out_dir_from_legacy_path "--report-html" "${LEGACY_REPORT_HTML}" "report.html")")
fi
if [[ -n "${LEGACY_REPORT_JSON}" ]]; then
  LEGACY_OUT_DIRS+=("$(infer_out_dir_from_legacy_path "--report-json" "${LEGACY_REPORT_JSON}" "report.json")")
fi

if [[ "${#LEGACY_OUT_DIRS[@]}" -gt 0 ]]; then
  first="${LEGACY_OUT_DIRS[0]}"
  for inferred in "${LEGACY_OUT_DIRS[@]}"; do
    if [[ "${inferred}" != "${first}" ]]; then
      echo "legacy output flags refer to different output roots:" >&2
      printf '  %s\n' "${LEGACY_OUT_DIRS[@]}" >&2
      echo "Use --out-dir to set a single output directory." >&2
      exit 2
    fi
  done
  if [[ "${OUT_DIR_EXPLICIT}" -eq 1 && "${OUT_DIR}" != "${first}" ]]; then
    echo "--out-dir (${OUT_DIR}) conflicts with legacy output flags (implying ${first})." >&2
    echo "Use either --out-dir, or the legacy flags, but not both." >&2
    exit 2
  fi
  OUT_DIR="${first}"
  OUT_DIR_SET=1
  refuse_unsafe_path "out dir" "${OUT_DIR}"
fi

CHROME_OUT_DIR="${OUT_DIR}/chrome"
FASTR_OUT_DIR="${OUT_DIR}/fastrender"
REPORT_HTML="${OUT_DIR}/report.html"
REPORT_JSON="${OUT_DIR}/report.json"
xtask_args=(fixture-chrome-diff)
if [[ "${FIXTURES_DIR_SET}" -eq 1 ]]; then
  xtask_args+=(--fixtures-dir "${FIXTURES_DIR}")
fi
if [[ "${OUT_DIR_SET}" -eq 1 ]]; then
  xtask_args+=(--out-dir "${OUT_DIR}")
fi
if [[ -n "${VIEWPORT}" ]]; then
  xtask_args+=(--viewport "${VIEWPORT}")
fi
if [[ -n "${DPR}" ]]; then
  xtask_args+=(--dpr "${DPR}")
fi
if [[ -n "${MEDIA}" ]]; then
  xtask_args+=(--media "${MEDIA}")
fi
if [[ -n "${TIMEOUT}" ]]; then
  xtask_args+=(--timeout "${TIMEOUT}")
fi
if [[ -n "${JS}" ]]; then
  xtask_args+=(--js "${JS}")
fi
if [[ -n "${JOBS}" ]]; then
  xtask_args+=(--jobs "${JOBS}")
fi
if [[ "${WRITE_SNAPSHOT}" -eq 1 ]]; then
  xtask_args+=(--write-snapshot)
fi
if [[ "${FIT_CANVAS_TO_CONTENT}" -eq 1 ]]; then
  xtask_args+=(--fit-canvas-to-content)
fi
if [[ -n "${CHROME_BIN}" ]]; then
  xtask_args+=(--chrome "${CHROME_BIN}")
fi
if [[ -n "${SHARD}" ]]; then
  xtask_args+=(--shard "${SHARD}")
fi
if [[ -n "${TOLERANCE}" ]]; then
  xtask_args+=(--tolerance "${TOLERANCE}")
fi
if [[ -n "${MAX_DIFF_PERCENT}" ]]; then
  xtask_args+=(--max-diff-percent "${MAX_DIFF_PERCENT}")
fi
if [[ -n "${MAX_PERCEPTUAL_DISTANCE}" ]]; then
  xtask_args+=(--max-perceptual-distance "${MAX_PERCEPTUAL_DISTANCE}")
fi
if [[ "${IGNORE_ALPHA}" -eq 1 ]]; then
  xtask_args+=(--ignore-alpha)
fi
if [[ -n "${SORT_BY}" ]]; then
  xtask_args+=(--sort-by "${SORT_BY}")
fi
if [[ "${FAIL_ON_DIFFERENCES}" -eq 1 ]]; then
  xtask_args+=(--fail-on-differences)
fi
if [[ "${DIFF_ONLY}" -eq 1 ]]; then
  xtask_args+=(--diff-only)
else
  if [[ "${NO_CHROME}" -eq 1 ]]; then
    xtask_args+=(--no-chrome)
  fi
  if [[ "${NO_FASTRENDER}" -eq 1 ]]; then
    xtask_args+=(--no-fastrender)
  fi
fi
if [[ "${NO_BUILD}" -eq 1 ]]; then
  xtask_args+=(--no-build)
fi
if [[ "${HAS_EXPLICIT_FIXTURES}" -eq 1 ]]; then
  xtask_args+=(--fixtures "${EXPLICIT_FIXTURES}")
elif [[ "${#FILTERS[@]}" -gt 0 ]]; then
  if [[ "${DISABLE_POSITIONAL_FIXTURES}" -eq 1 ]]; then
    echo "warning: ignoring positional fixture list because selection flags were provided; use --fixtures to select explicitly." >&2
  else
    mapfile -t RESOLVED_FIXTURES < <(resolve_fixture_patterns "${FILTERS[@]}")
    if [[ "${#RESOLVED_FIXTURES[@]}" -eq 0 ]]; then
      echo "No fixtures matched the provided filters." >&2
      exit 1
    fi
    xtask_args+=(--fixtures "$(IFS=,; echo "${RESOLVED_FIXTURES[*]}")")
  fi
fi
if [[ "${#EXTRA_XTASK_ARGS[@]}" -gt 0 ]]; then
  xtask_args+=("${EXTRA_XTASK_ARGS[@]}")
fi

cmd=(cargo xtask "${xtask_args[@]}")

printf '$'
printf ' %q' "${cmd[@]}"
printf '\n'

set +e
"${cmd[@]}"
status=$?
set -e

echo
echo "Outputs:"
echo "  Output dir:      ${OUT_DIR}/"
echo "  Chrome PNGs:     ${CHROME_OUT_DIR}/"
echo "  FastRender PNGs: ${FASTR_OUT_DIR}/"
echo "  Diff report:     ${REPORT_HTML}"
echo "  Diff JSON:       ${REPORT_JSON}"

exit "${status}"
