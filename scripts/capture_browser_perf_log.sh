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
  - Output is written to <out.jsonl> via FASTR_PERF_LOG_OUT.
  - Script progress messages are written to stderr.
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

browser_cmd=(
  timeout -k 10 600
  bash "${repo_root}/scripts/run_limited.sh" --as 64G -- \
    bash "${repo_root}/scripts/cargo_agent.sh" run --release --features browser_ui --bin browser --
)
if [[ -n "${url}" ]]; then
  browser_cmd+=("${url}")
fi
if [[ ${#extra_browser_args[@]} -gt 0 ]]; then
  browser_cmd+=("${extra_browser_args[@]}")
fi

# Some auxiliary browser perf logs (e.g. `idle_summary`, `worker_wake_summary`) are still emitted on
# stdout even when FASTR_PERF_LOG_OUT is set (the main structured events go to FASTR_PERF_LOG_OUT).
# Capture stdout to a temp file and append any JSON lines back into the output so the user gets a
# complete JSONL stream in one file.
stdout_tmp=""
if command -v mktemp >/dev/null 2>&1; then
  stdout_tmp="$(mktemp "${out_path}.stdout.XXXXXX" 2>/dev/null || true)"
fi
if [[ -z "${stdout_tmp}" ]]; then
  stdout_tmp="${out_path}.stdout.$$"
  : > "${stdout_tmp}"
fi

set +e
FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT="${out_path}" "${browser_cmd[@]}" >"${stdout_tmp}"
browser_status=$?
set -e

if [[ -s "${stdout_tmp}" ]]; then
  if command -v python3 >/dev/null 2>&1; then
    # Filter to valid JSON object lines so accidental non-JSON stdout output doesn't corrupt the
    # captured JSONL file.
    python3 - "${stdout_tmp}" >>"${out_path}" <<'PY'
import json, sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8", errors="replace") as f:
    for raw in f:
        line = raw.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except Exception:
            continue
        # Only keep perf-log-like records. `browser_perf_log_summary` expects an object containing at
        # least an `event` field; dropping other JSON avoids corrupting the captured stream.
        if isinstance(obj, dict) and isinstance(obj.get("event"), str):
            sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
PY
  else
    # Best-effort filter without Python: keep only lines that look like JSON objects and contain an
    # `"event": ...` field. This avoids corrupting the JSONL stream if stdout includes plain-text
    # output.
    if command -v awk >/dev/null 2>&1; then
      awk 'BEGIN { OFS="" }
        {
          line=$0
          sub(/^[[:space:]]+/, "", line)
          sub(/[[:space:]]+$/, "", line)
          if (line ~ /^\{/ && line ~ /"event"[[:space:]]*:/) {
            print line
          }
        }' "${stdout_tmp}" >>"${out_path}"
    else
      cat "${stdout_tmp}" >>"${out_path}"
    fi
  fi
fi
rm -f "${stdout_tmp}" 2>/dev/null || true

if [[ "${browser_status}" -ne 0 ]]; then
  echo "capture_browser_perf_log: browser exited with status ${browser_status} (continuing)" >&2
fi

if [[ ! -s "${out_path}" ]]; then
  echo "capture_browser_perf_log: warning: perf log is empty at ${out_path}" >&2
fi

if [[ "${run_summary}" -eq 1 ]]; then
  if [[ ! -s "${out_path}" ]]; then
    echo "capture_browser_perf_log: skipping summary (empty log file)" >&2
  else
    echo "capture_browser_perf_log: running browser_perf_log_summary..." >&2
    set +e
    timeout -k 10 600 bash "${repo_root}/scripts/run_limited.sh" --as 64G -- \
      bash "${repo_root}/scripts/cargo_agent.sh" run --release --bin browser_perf_log_summary -- \
      --input "${out_path}" \
      >/dev/null
    summary_status=$?
    set -e
    if [[ "${summary_status}" -ne 0 ]]; then
      echo "capture_browser_perf_log: warning: browser_perf_log_summary exited with ${summary_status}" >&2
    fi
  fi
fi

exit "${browser_status}"
