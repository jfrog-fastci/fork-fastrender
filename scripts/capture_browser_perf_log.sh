#!/usr/bin/env bash
set -euo pipefail

# Capture the windowed browser's JSONL perf log (`browser --perf-log`) under the repo-mandated
# guardrails.
#
# Guardrails (mandatory):
#   timeout -k 10 600 + scripts/run_limited.sh --as 64G + scripts/cargo_agent.sh
#
# Usage (positional, recommended):
#   bash scripts/capture_browser_perf_log.sh target/browser.perf.jsonl about:test-layout-stress
#
# Usage (flag form, for scripts):
#   bash scripts/capture_browser_perf_log.sh --out target/browser.perf.jsonl --url about:test-layout-stress
#
# Optional:
#   --trace-out <path> passes `browser --trace-out <path>` so you can capture a Perfetto/Chrome trace
#   alongside the stdout JSONL perf log.
#
# Add `--summary` to run `browser_perf_log_summary` after capture.

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
cd "${repo_root}"

usage() {
  cat <<'EOF' >&2
 usage:
  scripts/capture_browser_perf_log.sh [--summary] <out.jsonl> [url] [browser args...]
  scripts/capture_browser_perf_log.sh --out <out.jsonl> [--url <url>] [--trace-out <trace.json>] [--summary] [-- <browser args...>]

Capture the windowed `browser` perf JSONL log (`browser --perf-log`) to <out.jsonl>.

Examples:
  # Positional:
  bash scripts/capture_browser_perf_log.sh target/browser.perf.jsonl about:test-layout-stress

  # Flag form:
  bash scripts/capture_browser_perf_log.sh --out target/browser.perf.jsonl --url about:test-layout-stress

  # Capture perf JSONL + a browser UI trace:
  bash scripts/capture_browser_perf_log.sh --out target/browser.perf.jsonl --url about:test-layout-stress \
    --trace-out target/browser_trace.json

  # Capture + summarize:
  bash scripts/capture_browser_perf_log.sh --summary target/browser.perf.jsonl about:test-layout-stress

Notes:
  - Perf logs are emitted on stdout (via `browser --perf-log`) and also written to <out.jsonl> via `tee`.
  - If --trace-out is set, the browser writes a Perfetto/Chrome trace JSON file when it exits.
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
trace_out=""
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
    --trace-out)
      require_value "$1" "${2:-}"
      trace_out="$2"
      shift 2
      ;;
    --trace-out=*)
      trace_out="${1#--trace-out=}"
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

trace_out_path="${trace_out}"
if [[ -n "${trace_out_path}" && "${trace_out_path}" != /* ]]; then
  trace_out_path="${repo_root}/${trace_out_path}"
fi

mkdir -p -- "$(dirname "${out_path}")"
# Ensure the output is truncated before running so repeated captures don't accumulate.
: > "${out_path}"

echo "capture_browser_perf_log: output=${out_path}" >&2
if [[ -n "${url}" ]]; then
  echo "capture_browser_perf_log: url=${url}" >&2
fi
if [[ -n "${trace_out_path}" ]]; then
  echo "capture_browser_perf_log: trace_out=${trace_out_path}" >&2
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

# This script expects perf events on stdout so it can tee to the output file. Disallow passing
# `--perf-log-out` (which would redirect output away from stdout).
for arg in "${extra_browser_args[@]}"; do
  case "${arg}" in
    --perf-log-out|--perf-log-out=*)
      echo "capture_browser_perf_log: error: do not pass ${arg}; this script captures stdout JSONL via tee." >&2
      echo "hint: run the browser directly with: browser --perf-log-out <path> ..." >&2
      exit 2
      ;;
    --trace-out|--trace-out=*)
      if [[ -n "${trace_out_path}" ]]; then
        echo "capture_browser_perf_log: error: do not pass ${arg} when using --trace-out; this script will forward the trace flag for you." >&2
        exit 2
      fi
      ;;
  esac
done

# Prefer the CLI flag so callers do not need to set env vars. Allow overriding if the caller passed
# `--perf-log` explicitly in forwarded args.
if ! printf '%s\n' "${extra_browser_args[@]}" | grep -q -- '^--perf-log$'; then
  browser_cmd+=("--perf-log")
fi
if [[ -n "${trace_out_path}" ]]; then
  browser_cmd+=("--trace-out" "${trace_out_path}")
fi

if [[ -n "${url}" ]]; then
  browser_cmd+=("${url}")
fi
if [[ ${#extra_browser_args[@]} -gt 0 ]]; then
  browser_cmd+=("${extra_browser_args[@]}")
fi

echo "capture_browser_perf_log: capturing perf JSONL (stdout → tee → ${out_path})" >&2

set +e
"${browser_cmd[@]}" | tee -- "${out_path}"
# NOTE: `PIPESTATUS` is updated after *every* command (including simple assignments). Capture both
# pipeline statuses in a single assignment statement so `set -u` doesn't explode mid-script.
browser_status=${PIPESTATUS[0]:-0} tee_status=${PIPESTATUS[1]:-0}
set -e

status="${browser_status}"
if [[ "${status}" -eq 0 && "${tee_status}" -ne 0 ]]; then
  status="${tee_status}"
fi

if [[ "${browser_status}" -ne 0 ]]; then
  echo "capture_browser_perf_log: browser exited with status ${browser_status} (continuing)" >&2
fi
if [[ "${tee_status}" -ne 0 ]]; then
  echo "capture_browser_perf_log: tee exited with status ${tee_status}" >&2
fi

if [[ ! -s "${out_path}" ]]; then
  echo "capture_browser_perf_log: warning: perf log is empty at ${out_path}" >&2
fi

echo "capture_browser_perf_log: hint: summarize with browser_perf_log_summary:" >&2
echo "  timeout -k 10 600 bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary -- --input ${out_path}" >&2

if [[ "${run_summary}" -eq 1 ]]; then
  if [[ ! -s "${out_path}" ]]; then
    echo "capture_browser_perf_log: skipping summary (empty log file)" >&2
  else
    summary_bin=""
    if [[ -n "${CARGO_BIN_EXE_browser_perf_log_summary:-}" && -x "${CARGO_BIN_EXE_browser_perf_log_summary}" ]]; then
      summary_bin="${CARGO_BIN_EXE_browser_perf_log_summary}"
    elif command -v browser_perf_log_summary >/dev/null 2>&1; then
      summary_bin="$(command -v browser_perf_log_summary)"
    else
      target_dir="${CARGO_TARGET_DIR:-}"
      if [[ -z "${target_dir}" ]]; then
        target_dir="${repo_root}/target"
      elif [[ "${target_dir}" != /* ]]; then
        target_dir="${repo_root}/${target_dir}"
      fi
      for profile in release debug; do
        candidate="${target_dir}/${profile}/browser_perf_log_summary${exe_suffix}"
        if [[ -x "${candidate}" ]]; then
          summary_bin="${candidate}"
          break
        fi
      done
    fi

    if [[ -z "${summary_bin}" ]]; then
      echo "capture_browser_perf_log: browser_perf_log_summary not found; build it with:" >&2
      echo "  timeout -k 10 600 bash scripts/cargo_agent.sh build --release --bin browser_perf_log_summary" >&2
    else
      echo "capture_browser_perf_log: running summary (p50/p95/max)..." >&2
      set +e
      # `browser_perf_log_summary` prints a human-readable summary to stderr and JSON to stdout.
      # Suppress stdout so the wrapper stays terminal-friendly (and stdout remains JSONL-only).
      timeout -k 10 600 bash "${repo_root}/scripts/run_limited.sh" --as 64G -- \
        "${summary_bin}" --input "${out_path}" >/dev/null
      summary_status=$?
      set -e
      if [[ "${summary_status}" -ne 0 ]]; then
        echo "capture_browser_perf_log: warning: browser_perf_log_summary exited with ${summary_status}" >&2
      fi
    fi
  fi
fi

exit "${status}"
