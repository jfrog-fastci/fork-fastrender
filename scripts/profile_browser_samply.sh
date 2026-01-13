#!/usr/bin/env bash
set -euo pipefail

# Profile the windowed `browser` UI (feature = browser_ui) under `samply`.
#
# This helper is meant for interactive debugging: run it, reproduce resize/scroll jank, close the
# browser window, then open the saved profile later with `samply load ...`.

# Always run relative paths from the repository root, even if the script is invoked from a
# subdirectory.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

usage() {
  cat <<'EOF'
usage: scripts/profile_browser_samply.sh [--url <url>] [-- <browser args...>]

Builds a symbolized release `browser` binary with frame pointers, runs it under:
  scripts/run_limited.sh --as 64G -- samply record --save-only --no-open -o <out> -- <browser...>

The default URL is offline-friendly (`about:newtab`, or `about:test-layout-stress` when supported).

Examples:
  bash scripts/profile_browser_samply.sh
  bash scripts/profile_browser_samply.sh --url about:newtab
  bash scripts/profile_browser_samply.sh -- --no-restore
  bash scripts/profile_browser_samply.sh --url https://example.com -- --no-restore

Environment:
  PROFILE_LABEL           Overrides the filename label (default: browser).
  OUT_DIR                 Overrides output directory (default: target/browser/profiles).
  PROFILE_SAVE_BINARY=0   Skip saving the profiled binary next to the profile.
  PROFILE_COPY_BINARY=1   Copy the binary instead of hardlinking when hardlink fails.
EOF
}

URL=""
browser_args=()

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

if ! command -v samply >/dev/null 2>&1; then
  echo "missing 'samply' (install with: scripts/cargo_agent.sh install --locked samply)" >&2
  exit 1
fi

DEFAULT_URL="about:newtab"
# Best-effort: prefer the stress test about-page when it exists in this checkout.
if [[ -f src/ui/about_pages.rs ]] && grep -q 'about:test-layout-stress' src/ui/about_pages.rs; then
  DEFAULT_URL="about:test-layout-stress"
fi

if [[ -z "${URL}" ]]; then
  URL="${DEFAULT_URL}"
fi

# Terminal-only friendly: write profiles to disk and don't auto-open the samply UI.
OUT_DIR="${OUT_DIR:-target/browser/profiles}"
mkdir -p "${OUT_DIR}"
LABEL="${PROFILE_LABEL:-browser}"
OUT_FILE="${OUT_DIR}/${LABEL}-$(date +%Y%m%d-%H%M%S).profile.json.gz"

# Build a symbolized release binary suitable for profiling.
export CARGO_PROFILE_RELEASE_DEBUG=1
export CARGO_PROFILE_RELEASE_STRIP=none
if [[ -z "${RUSTFLAGS:-}" ]]; then
  export RUSTFLAGS="-C force-frame-pointers=yes"
elif [[ "${RUSTFLAGS}" != *force-frame-pointers* ]]; then
  export RUSTFLAGS="${RUSTFLAGS} -C force-frame-pointers=yes"
fi

bash scripts/cargo_agent.sh build --release --features browser_ui --bin browser

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
if [[ "${TARGET_DIR}" != /* ]]; then
  TARGET_DIR="${REPO_ROOT}/${TARGET_DIR}"
fi
BIN_PATH="${TARGET_DIR}/release/browser"
if [[ -f "${BIN_PATH}.exe" ]]; then
  BIN_PATH="${BIN_PATH}.exe"
fi

# For reliable post-hoc symbolication, keep the exact binary that produced the sampled addresses.
# Prefer hardlinking (cheap), but optionally fall back to copying.
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

echo
echo "Launching browser under samply. Interact with the window, then close it to finish recording."
echo "Start URL: ${URL}"
echo

set +e
bash scripts/run_limited.sh --as 64G -- samply record --save-only --no-open -o "${OUT_FILE}" -- \
  "${BIN_PATH}" "${URL}" "${browser_args[@]}"
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

