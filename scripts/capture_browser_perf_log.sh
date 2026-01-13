#!/usr/bin/env bash
set -euo pipefail

# Capture the windowed browser's JSONL perf log (`FASTR_PERF_LOG`) under the repo-mandated
# guardrails.
#
# Guardrails (mandatory):
#   timeout -k 10 600 + scripts/run_limited.sh --as 64G + scripts/cargo_agent.sh
#
# Usage (positional, recommended):
#   bash scripts/capture_browser_perf_log.sh target/browser.perf.jsonl about:test-scroll
#
# Usage (flag form, for scripts):
#   bash scripts/capture_browser_perf_log.sh --out target/browser.perf.jsonl --url about:test-scroll
#
# Add `--summary` to run `browser_perf_log_summary` after capture.

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
cd "${repo_root}"

usage() {
  cat <<'EOF' >&2
usage:
  scripts/capture_browser_perf_log.sh [--summary] <out.jsonl> [url] [browser args...]
  scripts/capture_browser_perf_log.sh --out <out.jsonl> [--url <url>] [--summary] [-- <browser args...>]

Capture the windowed `browser` perf JSONL log (FASTR_PERF_LOG=1) to <out.jsonl>.

Examples:
  # Positional:
  bash scripts/capture_browser_perf_log.sh target/browser.perf.jsonl about:test-scroll

  # Flag form:
  bash scripts/capture_browser_perf_log.sh --out target/browser.perf.jsonl --url about:test-scroll

  # Capture + summarize:
  bash scripts/capture_browser_perf_log.sh --summary target/browser.perf.jsonl about:test-scroll

Notes:
  - The main perf log stream is written to <out.jsonl> via `FASTR_PERF_LOG_OUT`.
  - Some auxiliary perf diagnostics may still be emitted on stdout; this script captures stdout and
    appends any JSON lines that contain an `"event"` field into <out.jsonl> after the browser exits.
  - Script progress messages (including optional summaries) are written to stderr.
EOF
}

require_value() {
  local flag="$1"
  local value="${2:-}"
  if [[ -z "${value}" || "${value}" == --* ]]; then
    echo "capture_browser_perf_log: ${flag} requires a value" >&2
    usage
    exit 2
  fi
}

url=""
out=""
run_summary=0
extra_browser_args=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --summary|--summarize|-s)
      run_summary=1
      shift
      ;;
    --url)
      require_value "$1" "${2:-}"
      url="$2"
      shift 2
      ;;
    --url=*)
      url="${1#--url=}"
      shift
      ;;
    --out)
      require_value "$1" "${2:-}"
      out="$2"
      shift 2
      ;;
    --out=*)
      out="${1#--out=}"
      shift
      ;;
    --)
      shift
      extra_browser_args=("$@")
      set -- # stop parsing
      break
      ;;
    -*)
      # Unknown flag before we hit positional args.
      echo "capture_browser_perf_log: unknown flag: $1" >&2
      usage
      exit 2
      ;;
    *)
      break
      ;;
  esac
done

if [[ -z "${out}" ]]; then
  if [[ $# -lt 1 ]]; then
    echo "capture_browser_perf_log: missing required output path" >&2
    usage
    exit 2
  fi
  out="$1"
  shift

  # Optional URL (positional).
  if [[ $# -gt 0 ]]; then
    url="$1"
    shift
  fi

  # Any remaining args are forwarded to the browser.
  if [[ $# -gt 0 ]]; then
    extra_browser_args+=("$@")
  fi
else
  # If --out was provided, allow a single positional URL as a convenience.
  if [[ -z "${url}" && $# -gt 0 ]]; then
    url="$1"
    shift
  fi
  if [[ $# -gt 0 ]]; then
    echo "capture_browser_perf_log: unexpected extra arguments: $*" >&2
    usage
    exit 2
  fi
fi

out_path="${out}"
if [[ "${out_path}" != /* ]]; then
  out_path="${repo_root}/${out_path}"
fi

mkdir -p -- "$(dirname "${out_path}")"
# Ensure the output is truncated before running so repeated captures don't accumulate.
: > "${out_path}"

echo "capture_browser_perf_log: output=${out_path}" >&2
if [[ -n "${url}" ]]; then
  echo "capture_browser_perf_log: url=${url}" >&2
fi

exe_suffix=""
case "${OSTYPE:-}" in
  msys*|cygwin*|win32*) exe_suffix=".exe" ;;
esac

browser_cmd=(timeout -k 10 600 bash "${repo_root}/scripts/run_limited.sh" --as 64G --)
if [[ -n "${CARGO_BIN_EXE_browser:-}" && -x "${CARGO_BIN_EXE_browser}" ]]; then
  echo "capture_browser_perf_log: using CARGO_BIN_EXE_browser=${CARGO_BIN_EXE_browser}" >&2
  browser_cmd+=("${CARGO_BIN_EXE_browser}")
else
  browser_cmd+=(
    bash "${repo_root}/scripts/cargo_agent.sh" run --release --features browser_ui --bin browser --
  )
fi
if [[ -n "${url}" ]]; then
  browser_cmd+=("${url}")
fi
if [[ ${#extra_browser_args[@]} -gt 0 ]]; then
  browser_cmd+=("${extra_browser_args[@]}")
fi

echo "capture_browser_perf_log: capturing perf JSONL (FASTR_PERF_LOG_OUT → ${out_path})" >&2

stdout_tmp="$(mktemp -t fastrender-browser-perf-stdout.XXXXXX)"
stdout_filtered_tmp=""
cleanup() {
  rm -f -- "${stdout_tmp}"
  if [[ -n "${stdout_filtered_tmp}" ]]; then
    rm -f -- "${stdout_filtered_tmp}"
  fi
}
trap cleanup EXIT

set +e
FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT="${out_path}" "${browser_cmd[@]}" >"${stdout_tmp}"
browser_status=$?
set -e

status="${browser_status}"

if [[ "${browser_status}" -ne 0 ]]; then
  echo "capture_browser_perf_log: browser exited with status ${browser_status} (continuing)" >&2
fi

if [[ -s "${stdout_tmp}" ]]; then
  stdout_filtered_tmp="$(mktemp -t fastrender-browser-perf-stdout-filtered.XXXXXX)"
  if command -v python3 >/dev/null 2>&1; then
    python3 - "${stdout_tmp}" >"${stdout_filtered_tmp}" <<'PY'
import json
import sys

path = sys.argv[1]
with open(path, "r", errors="replace") as f:
    for raw in f:
        line = raw.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except Exception:
            continue
        if isinstance(obj, dict) and isinstance(obj.get("event"), str):
            # Emit compact JSON to keep the output file stable and JSONL-friendly.
            sys.stdout.write(json.dumps(obj, separators=(",", ":")))
            sys.stdout.write("\n")
PY
  else
    # Best-effort fallback: keep only lines that look like JSON objects with an "event" field.
    awk 'BEGIN{FS=""} /^[[:space:]]*\\{/ && $0 ~ /"event"[[:space:]]*:[[:space:]]*"/ {print}' \
      "${stdout_tmp}" >"${stdout_filtered_tmp}"
  fi

  if [[ -s "${stdout_filtered_tmp}" ]]; then
    cat "${stdout_filtered_tmp}" >> "${out_path}"
  fi
fi

if [[ ! -s "${out_path}" ]]; then
  echo "capture_browser_perf_log: warning: perf log is empty at ${out_path}" >&2
fi

echo "capture_browser_perf_log: hint: summarize with browser_perf_log_summary:" >&2
echo "  timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \\" >&2
echo "    bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary -- --input ${out_path}" >&2

if [[ "${run_summary}" -eq 1 ]]; then
  if [[ ! -s "${out_path}" ]]; then
    echo "capture_browser_perf_log: skipping summary (empty log file)" >&2
  else
    echo "capture_browser_perf_log: running summary (p50/p95/max)..." >&2
    set +e
    # `browser_perf_log_summary` prints a human-readable summary to stderr and JSON to stdout.
    # Suppress stdout so the wrapper stays terminal-friendly.
    timeout -k 10 600 bash "${repo_root}/scripts/run_limited.sh" --as 64G -- \
      bash "${repo_root}/scripts/cargo_agent.sh" run --quiet --release --bin browser_perf_log_summary -- \
      --input "${out_path}" >/dev/null
    summary_status=$?
    set -e
    if [[ "${summary_status}" -ne 0 ]]; then
      echo "capture_browser_perf_log: warning: browser_perf_log_summary exited with ${summary_status}" >&2
    fi
  fi
fi

exit "${status}"
