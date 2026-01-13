#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF' >&2
usage: scripts/capture_browser_perf_log.sh --url <url> --out <path.jsonl> [--summary] [-- <extra browser args...>]

Capture the windowed `browser` perf JSONL log stream (stdout) to a file.

Example:
  scripts/capture_browser_perf_log.sh --url about:test-layout-stress --out target/browser_perf.jsonl

Options:
  --url <url>       Initial URL to open (passed to `browser` as the positional URL argument)
  --out <file>      Output JSONL path (parent directories will be created)
  --summary         After the browser exits, run `browser_perf_log_summary` if it is available
  -h, --help        Show this help

Notes:
  - Perf events are emitted on stdout by the `browser` process when FASTR_PERF_LOG=1.
  - Script progress messages go to stderr so the captured JSONL stays clean.
  - Relative --out paths are interpreted relative to the repo root.
EOF
}

url=""
out=""
run_summary=0
extra_browser_args=()

while [[ $# -gt 0 ]]; do
  case "${1}" in
    -h|--help)
      usage
      exit 0
      ;;
    --url)
      url="${2:-}"
      shift 2
      ;;
    --out)
      out="${2:-}"
      shift 2
      ;;
    --summary)
      run_summary=1
      shift
      ;;
    --)
      shift
      extra_browser_args=("$@")
      break
      ;;
    *)
      echo "capture_browser_perf_log: unknown argument: ${1}" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "${url}" ]]; then
  echo "capture_browser_perf_log: missing required --url" >&2
  usage
  exit 2
fi
if [[ -z "${out}" ]]; then
  echo "capture_browser_perf_log: missing required --out" >&2
  usage
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"

out_path="${out}"
if [[ "${out_path}" != /* ]]; then
  out_path="${repo_root}/${out_path}"
fi

out_dir="$(dirname "${out_path}")"
mkdir -p -- "${out_dir}"

echo "capture_browser_perf_log: writing perf JSONL to ${out_path}" >&2
echo "capture_browser_perf_log: running with FASTR_PERF_LOG=1 under scripts/run_limited.sh --as 64G" >&2
echo "capture_browser_perf_log: forcing FASTR_PERF_LOG_OUT to be unset/empty so logs stay on stdout" >&2

browser_cmd=()
if [[ -n "${CARGO_BIN_EXE_browser:-}" && -x "${CARGO_BIN_EXE_browser}" ]]; then
  echo "capture_browser_perf_log: using CARGO_BIN_EXE_browser=${CARGO_BIN_EXE_browser}" >&2
  browser_cmd=("${CARGO_BIN_EXE_browser}" "${url}" "${extra_browser_args[@]}")
else
  browser_cmd=(
    bash "${repo_root}/scripts/cargo_agent.sh"
    run
    --release
    --features
    browser_ui
    --bin
    browser
    --
    "${url}"
    "${extra_browser_args[@]}"
  )
fi

set +e
(
  cd "${repo_root}" || exit 1
  FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT= \
    bash "${repo_root}/scripts/run_limited.sh" --as 64G -- "${browser_cmd[@]}"
) | tee -- "${out_path}"
pipeline_status=$?
browser_status=${PIPESTATUS[0]}
tee_status=${PIPESTATUS[1]}
set -e

echo "capture_browser_perf_log: browser exit status=${browser_status}, tee exit status=${tee_status}" >&2

echo "capture_browser_perf_log: to summarize:" >&2
echo "  bash \"${repo_root}/scripts/run_limited.sh\" --as 64G -- \\" >&2
echo "    bash \"${repo_root}/scripts/cargo_agent.sh\" run --release --bin browser_perf_log_summary -- --input \"${out_path}\"" >&2
echo "  (or re-run with: --summary, if browser_perf_log_summary is already built/available)" >&2

find_summary_bin() {
  if [[ -n "${CARGO_BIN_EXE_browser_perf_log_summary:-}" && -x "${CARGO_BIN_EXE_browser_perf_log_summary}" ]]; then
    echo "${CARGO_BIN_EXE_browser_perf_log_summary}"
    return 0
  fi
  if command -v browser_perf_log_summary >/dev/null 2>&1; then
    command -v browser_perf_log_summary
    return 0
  fi

  target_dir="${CARGO_TARGET_DIR:-target}"
  if [[ "${target_dir}" != /* ]]; then
    target_dir="${repo_root}/${target_dir}"
  fi

  if [[ -x "${target_dir}/release/browser_perf_log_summary" ]]; then
    echo "${target_dir}/release/browser_perf_log_summary"
    return 0
  fi
  if [[ -x "${target_dir}/debug/browser_perf_log_summary" ]]; then
    echo "${target_dir}/debug/browser_perf_log_summary"
    return 0
  fi
  return 1
}

if [[ "${run_summary}" -eq 1 ]]; then
  if summary_bin="$(find_summary_bin)"; then
    echo "capture_browser_perf_log: running ${summary_bin} --input \"${out_path}\"" >&2
    # Keep stdout reserved for the perf JSONL stream. `browser_perf_log_summary` prints a JSON
    # summary on stdout and a human-readable summary on stderr; redirect stdout so everything stays
    # on stderr when `--summary` is used.
    bash "${repo_root}/scripts/run_limited.sh" --as 64G -- "${summary_bin}" --input "${out_path}" 1>&2
  else
    echo "capture_browser_perf_log: --summary requested but browser_perf_log_summary was not found." >&2
    echo "capture_browser_perf_log: build + run it with:" >&2
    echo "  bash \"${repo_root}/scripts/run_limited.sh\" --as 64G -- \\" >&2
    echo "    bash \"${repo_root}/scripts/cargo_agent.sh\" run --release --bin browser_perf_log_summary -- --input \"${out_path}\"" >&2
  fi
fi

exit "${pipeline_status}"
