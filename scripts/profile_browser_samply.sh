#!/usr/bin/env bash
set -euo pipefail

# Record an interactive Samply CPU profile of the windowed `browser` UI.
#
# Intended workflow:
#   1) Run this script.
#   2) Reproduce jank by resizing/scrolling/typing in the browser window.
#   3) Close the window to stop recording.
#   4) Open the saved profile later with `samply load ...`.

# Always run relative paths from the repository root, even if the script is invoked from a
# subdirectory.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

usage() {
  cat <<'EOF'
usage: scripts/profile_browser_samply.sh [url] [browser args...]
   or: scripts/profile_browser_samply.sh --url <url> [-- <browser args...>]

Examples:
  bash scripts/profile_browser_samply.sh about:test-layout-stress
  bash scripts/profile_browser_samply.sh https://example.org/ --no-restore
  bash scripts/profile_browser_samply.sh --url about:newtab -- --no-restore

Output (by default):
  target/browser/profiles/<label>-<timestamp>.profile.json.gz
  target/browser/profiles/<label>-<timestamp>.browser   (binary snapshot for symbolication)

Environment:
  BROWSER_FEATURES         Cargo features for building the browser (default: browser_ui)
  PROFILE_LABEL            Override the filename label (default: derived from URL)
  OUT_DIR                  Override output directory (default: target/browser/profiles)
  PROFILE_SAVE_BINARY=0    Skip saving the profiled binary next to the profile
  PROFILE_COPY_BINARY=1    Copy the binary instead of hardlinking when hardlink fails
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if ! command -v samply >/dev/null 2>&1; then
  echo "missing 'samply' (install with: scripts/cargo_agent.sh install --locked samply)" >&2
  exit 1
fi

URL=""
browser_args=()

# Allow `--url ...` (optional) + `--` separator, but also support the simple positional form:
#   scripts/profile_browser_samply.sh about:test-layout-stress --no-restore
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --url)
      if [[ $# -lt 2 || -z "${2:-}" ]]; then
        echo "error: --url requires a value" >&2
        exit 2
      fi
      URL="${2}"
      shift 2
      ;;
    --url=*)
      URL="${1#--url=}"
      shift
      ;;
    --)
      shift
      browser_args+=("$@")
      break
      ;;
    *)
      browser_args+=("$1")
      shift
      ;;
  esac
done

# If no explicit --url was provided, treat the first non-flag arg as the URL.
if [[ -z "${URL}" && ${#browser_args[@]} -gt 0 && "${browser_args[0]}" != -* ]]; then
  URL="${browser_args[0]}"
  browser_args=("${browser_args[@]:1}")
fi

# Build a symbolized release binary suitable for profiling.
export CARGO_PROFILE_RELEASE_DEBUG=1
export CARGO_PROFILE_RELEASE_STRIP=none
if [[ -z "${RUSTFLAGS:-}" ]]; then
  export RUSTFLAGS="-C force-frame-pointers=yes"
elif [[ "${RUSTFLAGS}" != *force-frame-pointers* ]]; then
  export RUSTFLAGS="${RUSTFLAGS} -C force-frame-pointers=yes"
fi

FEATURES="${BROWSER_FEATURES:-browser_ui}"
bash scripts/cargo_agent.sh build --release --features "${FEATURES}" --bin browser

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
if [[ "${TARGET_DIR}" != /* ]]; then
  TARGET_DIR="${REPO_ROOT}/${TARGET_DIR}"
fi
BIN_PATH="${TARGET_DIR}/release/browser"
if [[ -f "${BIN_PATH}.exe" ]]; then
  BIN_PATH="${BIN_PATH}.exe"
fi

# Terminal-friendly: write profiles to disk and don't auto-open a browser.
OUT_DIR="${OUT_DIR:-${TARGET_DIR}/browser/profiles}"
mkdir -p "${OUT_DIR}"

PROFILE_LABEL_DERIVED="browser"
if [[ -n "${URL}" ]]; then
  PROFILE_LABEL_DERIVED="${URL}"
fi
PROFILE_LABEL="${PROFILE_LABEL:-${PROFILE_LABEL_DERIVED}}"

# Sanitize the label into something filesystem-friendly (keep alnum + a few common separators).
PROFILE_LABEL_SAFE="$(
  printf '%s' "${PROFILE_LABEL}" \
    | tr -cs '[:alnum:]._+-' '_' \
    | sed -e 's/^_*//' -e 's/_*$//'
)"
if [[ -z "${PROFILE_LABEL_SAFE}" ]]; then
  PROFILE_LABEL_SAFE="browser"
fi

OUT_FILE="${OUT_DIR}/${PROFILE_LABEL_SAFE}-$(date +%Y%m%d-%H%M%S).profile.json.gz"

# For reliable post-hoc symbolication, keep the exact binary that produced the sampled addresses.
# We prefer a hardlink (cheap; preserves the old inode if `cargo build` replaces the binary later).
# If hardlinking fails (e.g. cross-filesystem), set `PROFILE_COPY_BINARY=1` to force a full copy.
BIN_SNAPSHOT="${OUT_FILE%.profile.json.gz}.browser"
if [[ "${PROFILE_SAVE_BINARY:-1}" != "0" ]]; then
  if ln "${BIN_PATH}" "${BIN_SNAPSHOT}" 2>/dev/null; then
    echo "Saved profiled binary (hardlink): ${BIN_SNAPSHOT}"
  elif [[ "${PROFILE_COPY_BINARY:-0}" == "1" ]]; then
    cp "${BIN_PATH}" "${BIN_SNAPSHOT}"
    echo "Saved profiled binary (copy): ${BIN_SNAPSHOT}"
  else
    echo "Note: could not hardlink ${BIN_PATH} -> ${BIN_SNAPSHOT} (set PROFILE_COPY_BINARY=1 to copy)"
  fi
fi

echo "Recording to: ${OUT_FILE}"
if [[ -n "${URL}" ]]; then
  echo "Start URL: ${URL}"
fi
echo "Close the browser window to finish recording."

run_args=()
if [[ -n "${URL}" ]]; then
  run_args+=("${URL}")
fi
run_args+=("${browser_args[@]}")

set +e
samply record --save-only --no-open -o "${OUT_FILE}" -- \
  bash scripts/run_limited.sh --as 64G -- \
  "${BIN_PATH}" "${run_args[@]}"
SAMP_STATUS=$?
set -e

if [[ ! -s "${OUT_FILE}" ]]; then
  echo "samply failed (exit ${SAMP_STATUS}) and did not produce a profile: ${OUT_FILE}" >&2
  exit "${SAMP_STATUS}"
fi

if [[ "${SAMP_STATUS}" -ne 0 ]]; then
  echo "Note: samply / browser exited with status ${SAMP_STATUS} (profile still written)." >&2
fi

echo "Wrote: ${OUT_FILE}"
echo "To view later: samply load ${OUT_FILE}"
if [[ -f "${BIN_SNAPSHOT}" ]]; then
  echo "Binary snapshot: ${BIN_SNAPSHOT}"
fi

if command -v python3 >/dev/null 2>&1; then
  echo
  echo "Summary (terminal-friendly):"
  ADDR2LINE_BIN="${BIN_PATH}"
  if [[ -f "${BIN_SNAPSHOT}" ]]; then
    ADDR2LINE_BIN="${BIN_SNAPSHOT}"
  fi
  python3 scripts/samply_summary.py "${OUT_FILE}" --top 25 --addr2line-binary "${ADDR2LINE_BIN}" || true
fi
