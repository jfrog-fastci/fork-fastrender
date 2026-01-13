#!/usr/bin/env bash
set -euo pipefail

# Trace syscalls used by a renderer workload to help maintain a strict seccomp allowlist.
#
# This script is intended for Linux developer machines. It is not used in CI.
#
# Output:
#   - Raw strace log:    target/seccomp/renderer.strace
#   - Unique syscall set target/seccomp/renderer_syscalls.txt
#   - The syscall list is also printed to stdout (one syscall per line).
#
# Notes:
#   - We build the chosen workload *outside* of strace so the trace only contains syscalls from the
#     renderer binary (not from cargo/rustc).
#   - All executed commands are wrapped in the repo's safety wrappers:
#       - `timeout -k 10 ...`
#       - `scripts/run_limited.sh`

usage() {
  cat <<'EOF'
usage: bash scripts/trace_renderer_syscalls.sh [options] [-- <renderer-args...>]

Options:
  --bin <name>          Cargo binary to trace (default: render_fixtures)
  --profile <profile>   Cargo profile to build/run (default: release)
                        Common values: release, dev
  --no-build            Skip the cargo build step
  --timeout <secs>      Hard timeout for the traced workload (default: 120)
  --build-timeout <s>   Hard timeout for cargo build (default: 600)
  --as <size>           Address-space limit passed to scripts/run_limited.sh (default: 64G)
  --out <path>          Write syscall list to this path (default: target/seccomp/renderer_syscalls.txt)
  --trace-out <path>    Write raw strace output to this path (default: target/seccomp/renderer.strace)
  -h, --help            Show this help

Renderer args:
  Any remaining arguments are forwarded to the traced binary.

Environment:
  STRACE_FLAGS          Extra flags appended to the strace invocation (example: "-ttt -s 0").

Examples:
  # Default workload (render one offline fixture).
  bash scripts/trace_renderer_syscalls.sh

  # Trace a different fixture (render_fixtures args).
  bash scripts/trace_renderer_syscalls.sh -- --fixtures go.dev --jobs 1

  # Add timestamps to the raw trace log for pre/post-sandbox correlation.
  STRACE_FLAGS="-ttt" bash scripts/trace_renderer_syscalls.sh
EOF
}

# Always run relative paths from the repository root, even if the script is invoked from a
# subdirectory.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: syscall tracing is only supported on Linux (requires strace)" >&2
  exit 1
fi

if ! command -v strace >/dev/null 2>&1; then
  echo "error: missing 'strace' (install your distro's strace package)" >&2
  exit 1
fi

BIN="render_fixtures"
PROFILE="release"
NO_BUILD=0
TIMEOUT_SECS=120
BUILD_TIMEOUT_SECS=600
LIMIT_AS="64G"
OUT_PATH="target/seccomp/renderer_syscalls.txt"
TRACE_OUT_PATH="target/seccomp/renderer.strace"

while [[ $# -gt 0 ]]; do
  case "${1:-}" in
    -h|--help)
      usage
      exit 0
      ;;
    --bin)
      BIN="${2:-}"; shift 2 ;;
    --profile)
      PROFILE="${2:-}"; shift 2 ;;
    --no-build)
      NO_BUILD=1; shift ;;
    --timeout)
      TIMEOUT_SECS="${2:-}"; shift 2 ;;
    --build-timeout)
      BUILD_TIMEOUT_SECS="${2:-}"; shift 2 ;;
    --as)
      LIMIT_AS="${2:-}"; shift 2 ;;
    --out)
      OUT_PATH="${2:-}"; shift 2 ;;
    --trace-out)
      TRACE_OUT_PATH="${2:-}"; shift 2 ;;
    --)
      shift
      break
      ;;
    *)
      # Treat unknown flags as renderer args for convenience.
      break
      ;;
  esac
done

mkdir -p "$(dirname "${OUT_PATH}")"
mkdir -p "$(dirname "${TRACE_OUT_PATH}")"

BIN_ARGS=("$@")

if [[ "${#BIN_ARGS[@]}" -eq 0 && "${BIN}" == "render_fixtures" ]]; then
  # Keep the default trace workload small and offline.
  BIN_ARGS=(
    --fixtures example.com
    --jobs 1
    --timeout 10
    --out-dir target/seccomp/fixture_renders
  )
fi

if [[ "${NO_BUILD}" -eq 0 ]]; then
  build_args=(build --profile "${PROFILE}" --bin "${BIN}")
  if [[ "${PROFILE}" == "release" ]]; then
    build_args=(build --release --bin "${BIN}")
  elif [[ "${PROFILE}" == "dev" ]]; then
    build_args=(build --bin "${BIN}")
  fi
  echo "Building ${BIN} (${PROFILE})..." >&2
  timeout -k 10 "${BUILD_TIMEOUT_SECS}" bash scripts/cargo_agent.sh "${build_args[@]}"
fi

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
if [[ "${TARGET_DIR}" != /* ]]; then
  TARGET_DIR="${REPO_ROOT}/${TARGET_DIR}"
fi

case "${PROFILE}" in
  release) BIN_PATH="${TARGET_DIR}/release/${BIN}" ;;
  dev) BIN_PATH="${TARGET_DIR}/debug/${BIN}" ;;
  *) BIN_PATH="${TARGET_DIR}/${PROFILE}/${BIN}" ;;
esac
if [[ -f "${BIN_PATH}.exe" ]]; then
  BIN_PATH="${BIN_PATH}.exe"
fi

if [[ ! -f "${BIN_PATH}" ]]; then
  echo "error: expected binary at ${BIN_PATH} (build may have failed)" >&2
  exit 1
fi

# Optional extra strace flags via env var.
STRACE_EXTRA=()
if [[ -n "${STRACE_FLAGS:-}" ]]; then
  # shellcheck disable=SC2206
  STRACE_EXTRA=(${STRACE_FLAGS})
fi

rm -f "${TRACE_OUT_PATH}"
echo "Tracing syscalls (raw log: ${TRACE_OUT_PATH})..." >&2

timeout -k 10 "${TIMEOUT_SECS}" bash scripts/run_limited.sh --as "${LIMIT_AS}" -- \
  strace -f -qq -o "${TRACE_OUT_PATH}" "${STRACE_EXTRA[@]}" -- \
  "${BIN_PATH}" "${BIN_ARGS[@]}"

# Parse syscall names from strace output.
#
# We handle:
#   - "openat(...)" normal lines
#   - "<... futex resumed> ) = 0" resumed lines
#   - Optional PID prefixes when `-f` is used.
awk '
  {
    line = $0

    # Strip optional PID prefix (strace -f prefixes each line with the PID).
    sub(/^[0-9]+[[:space:]]+/, "", line)

    # Strip optional timestamp if STRACE_FLAGS included -t/-tt/-ttt (after pid removal).
    sub(/^[0-9]+(\.[0-9]+)?[[:space:]]+/, "", line)

    # Handle "<... name resumed>" lines.
    if (match(line, /^<\.\.\. ([A-Za-z0-9_]+) resumed>/, m)) {
      print m[1]
      next
    }

    # Handle "name(" syscall lines.
    if (match(line, /^([A-Za-z0-9_]+)\(/, m)) {
      print m[1]
      next
    }
  }
' "${TRACE_OUT_PATH}" | LC_ALL=C sort -u | tee "${OUT_PATH}"

echo "Wrote syscall allowlist candidates to: ${OUT_PATH}" >&2
