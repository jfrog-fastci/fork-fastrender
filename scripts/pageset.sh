#!/usr/bin/env bash
set -euo pipefail

# Convenience wrapper around the pageset xtask (`scripts/cargo_agent.sh run -p xtask -- pageset`).
#
# This script exists mainly for backwards-compatible env vars/flags and muscle memory.
# Keep orchestration logic in the Rust `xtask` implementation so the behavior is validated and
# testable.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

usage() {
  cat <<'USAGE'
usage: scripts/pageset.sh [wrapper flags] [--] [pageset_progress flags...]

This is a thin wrapper over:
  scripts/cargo_agent.sh run -p xtask -- pageset [flags] [-- <extra pageset_progress flags...>]

Wrapper-only flags:
  --dry-run    Print the `scripts/cargo_agent.sh run -p xtask -- pageset ...` command that would run and exit 0.
USAGE
}

dry_run=0

# Backwards-compatible env defaults (xtask does not read these env vars directly).
jobs="${JOBS:-}"
fetch_timeout="${FETCH_TIMEOUT:-}"
render_timeout="${RENDER_TIMEOUT:-}"
user_agent="${USER_AGENT:-}"
accept_language="${ACCEPT_LANGUAGE:-}"
viewport="${VIEWPORT:-}"
dpr="${DPR:-}"

disk_cache_audit_clean=0
if [[ -n "${DISK_CACHE_AUDIT_CLEAN:-}" && "${DISK_CACHE_AUDIT_CLEAN}" != "0" ]]; then
  disk_cache_audit_clean=1
fi

cache_dir=""
no_fetch=0
refresh=0
disk_cache_override=""
font_mode=""
pages=""
shard=""
allow_http_error_status=0
allow_collisions=0
timings=0
accuracy=0
accuracy_baseline=""
accuracy_baseline_dir=""
accuracy_tolerance=""
accuracy_max_diff_percent=""
accuracy_diff_dir=""
capture_missing_failure_fixtures=0
capture_missing_failure_fixtures_out_dir=""
capture_missing_failure_fixtures_allow_missing_resources=0
capture_missing_failure_fixtures_overwrite=0
capture_worst_accuracy_fixtures=0
capture_worst_accuracy_fixtures_out_dir=""
capture_worst_accuracy_fixtures_min_diff_percent=""
capture_worst_accuracy_fixtures_top=""
capture_worst_accuracy_fixtures_allow_missing_resources=0
capture_worst_accuracy_fixtures_overwrite=0

extra_args=()

require_value() {
  local flag="$1"
  if [[ $# -lt 2 || -z "${2:-}" || "${2:-}" == -* ]]; then
    echo "error: ${flag} requires a value" >&2
    exit 2
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    --)
      # Keep parsing wrapper flags even after `--` for backwards compatibility.
      shift
      ;;
    --jobs|-j)
      require_value "$1" "${2:-}"
      jobs="$2"
      shift 2
      ;;
    --jobs=*)
      jobs="${1#--jobs=}"
      shift
      ;;
    -j*)
      jobs="${1#-j}"
      jobs="${jobs#=}"
      if [[ -z "${jobs}" ]]; then
        echo "error: $1 requires a value" >&2
        exit 2
      fi
      shift
      ;;
    --fetch-timeout)
      require_value "$1" "${2:-}"
      fetch_timeout="$2"
      shift 2
      ;;
    --fetch-timeout=*)
      fetch_timeout="${1#--fetch-timeout=}"
      shift
      ;;
    --render-timeout)
      require_value "$1" "${2:-}"
      render_timeout="$2"
      shift 2
      ;;
    --render-timeout=*)
      render_timeout="${1#--render-timeout=}"
      shift
      ;;
    --cache-dir)
      require_value "$1" "${2:-}"
      cache_dir="$2"
      shift 2
      ;;
    --cache-dir=*)
      cache_dir="${1#--cache-dir=}"
      shift
      ;;
    --user-agent)
      require_value "$1" "${2:-}"
      user_agent="$2"
      shift 2
      ;;
    --user-agent=*)
      user_agent="${1#--user-agent=}"
      shift
      ;;
    --accept-language)
      require_value "$1" "${2:-}"
      accept_language="$2"
      shift 2
      ;;
    --accept-language=*)
      accept_language="${1#--accept-language=}"
      shift
      ;;
    --viewport)
      require_value "$1" "${2:-}"
      viewport="$2"
      shift 2
      ;;
    --viewport=*)
      viewport="${1#--viewport=}"
      shift
      ;;
    --dpr)
      require_value "$1" "${2:-}"
      dpr="$2"
      shift 2
      ;;
    --dpr=*)
      dpr="${1#--dpr=}"
      shift
      ;;
    --no-fetch)
      no_fetch=1
      shift
      ;;
    --refresh)
      refresh=1
      shift
      ;;
    --disk-cache)
      disk_cache_override="1"
      shift
      ;;
    --no-disk-cache)
      disk_cache_override="0"
      shift
      ;;
    --disk-cache-audit-clean)
      disk_cache_audit_clean=1
      shift
      ;;
    --bundled-fonts)
      font_mode="bundled"
      shift
      ;;
    --system-fonts|--no-bundled-fonts)
      font_mode="system"
      shift
      ;;
    --pages)
      require_value "$1" "${2:-}"
      pages="$2"
      shift 2
      ;;
    --pages=*)
      pages="${1#--pages=}"
      shift
      ;;
    --shard)
      require_value "$1" "${2:-}"
      shard="$2"
      shift 2
      ;;
    --shard=*)
      shard="${1#--shard=}"
      shift
      ;;
    --allow-http-error-status)
      allow_http_error_status=1
      shift
      ;;
    --allow-collisions)
      allow_collisions=1
      shift
      ;;
    --timings)
      timings=1
      shift
      ;;
    --accuracy)
      accuracy=1
      shift
      ;;
    --accuracy-baseline)
      require_value "$1" "${2:-}"
      accuracy_baseline="$2"
      shift 2
      ;;
    --accuracy-baseline=*)
      accuracy_baseline="${1#--accuracy-baseline=}"
      shift
      ;;
    --accuracy-baseline-dir)
      require_value "$1" "${2:-}"
      accuracy_baseline_dir="$2"
      shift 2
      ;;
    --accuracy-baseline-dir=*)
      accuracy_baseline_dir="${1#--accuracy-baseline-dir=}"
      shift
      ;;
    --accuracy-tolerance)
      require_value "$1" "${2:-}"
      accuracy_tolerance="$2"
      shift 2
      ;;
    --accuracy-tolerance=*)
      accuracy_tolerance="${1#--accuracy-tolerance=}"
      shift
      ;;
    --accuracy-max-diff-percent)
      require_value "$1" "${2:-}"
      accuracy_max_diff_percent="$2"
      shift 2
      ;;
    --accuracy-max-diff-percent=*)
      accuracy_max_diff_percent="${1#--accuracy-max-diff-percent=}"
      shift
      ;;
    --accuracy-diff-dir)
      require_value "$1" "${2:-}"
      accuracy_diff_dir="$2"
      shift 2
      ;;
    --accuracy-diff-dir=*)
      accuracy_diff_dir="${1#--accuracy-diff-dir=}"
      shift
      ;;
    --capture-missing-failure-fixtures)
      capture_missing_failure_fixtures=1
      shift
      ;;
    --capture-missing-failure-fixtures-out-dir)
      require_value "$1" "${2:-}"
      capture_missing_failure_fixtures_out_dir="$2"
      shift 2
      ;;
    --capture-missing-failure-fixtures-out-dir=*)
      capture_missing_failure_fixtures_out_dir="${1#--capture-missing-failure-fixtures-out-dir=}"
      shift
      ;;
    --capture-missing-failure-fixtures-allow-missing-resources)
      capture_missing_failure_fixtures_allow_missing_resources=1
      shift
      ;;
    --capture-missing-failure-fixtures-overwrite)
      capture_missing_failure_fixtures_overwrite=1
      shift
      ;;
    --capture-worst-accuracy-fixtures)
      capture_worst_accuracy_fixtures=1
      shift
      ;;
    --capture-worst-accuracy-fixtures-out-dir)
      require_value "$1" "${2:-}"
      capture_worst_accuracy_fixtures_out_dir="$2"
      shift 2
      ;;
    --capture-worst-accuracy-fixtures-out-dir=*)
      capture_worst_accuracy_fixtures_out_dir="${1#--capture-worst-accuracy-fixtures-out-dir=}"
      shift
      ;;
    --capture-worst-accuracy-fixtures-min-diff-percent)
      require_value "$1" "${2:-}"
      capture_worst_accuracy_fixtures_min_diff_percent="$2"
      shift 2
      ;;
    --capture-worst-accuracy-fixtures-min-diff-percent=*)
      capture_worst_accuracy_fixtures_min_diff_percent="${1#--capture-worst-accuracy-fixtures-min-diff-percent=}"
      shift
      ;;
    --capture-worst-accuracy-fixtures-top)
      require_value "$1" "${2:-}"
      capture_worst_accuracy_fixtures_top="$2"
      shift 2
      ;;
    --capture-worst-accuracy-fixtures-top=*)
      capture_worst_accuracy_fixtures_top="${1#--capture-worst-accuracy-fixtures-top=}"
      shift
      ;;
    --capture-worst-accuracy-fixtures-allow-missing-resources)
      capture_worst_accuracy_fixtures_allow_missing_resources=1
      shift
      ;;
    --capture-worst-accuracy-fixtures-overwrite)
      capture_worst_accuracy_fixtures_overwrite=1
      shift
      ;;
    *)
      extra_args+=("$1")
      shift
      ;;
  esac
done

cmd=(bash scripts/cargo_agent.sh run -p xtask -- pageset)

if [[ -n "${jobs}" ]]; then
  cmd+=(--jobs "${jobs}")
fi
if [[ -n "${fetch_timeout}" ]]; then
  cmd+=(--fetch-timeout "${fetch_timeout}")
fi
if [[ -n "${render_timeout}" ]]; then
  cmd+=(--render-timeout "${render_timeout}")
fi
if [[ -n "${cache_dir}" ]]; then
  cmd+=(--cache-dir "${cache_dir}")
fi
if [[ -n "${user_agent}" ]]; then
  cmd+=(--user-agent "${user_agent}")
fi
if [[ -n "${accept_language}" ]]; then
  cmd+=(--accept-language "${accept_language}")
fi
if [[ -n "${viewport}" ]]; then
  cmd+=(--viewport "${viewport}")
fi
if [[ -n "${dpr}" ]]; then
  cmd+=(--dpr "${dpr}")
fi
if [[ "${no_fetch}" -eq 1 ]]; then
  cmd+=(--no-fetch)
fi
if [[ "${refresh}" -eq 1 ]]; then
  cmd+=(--refresh)
fi
if [[ -n "${disk_cache_override}" ]]; then
  if [[ "${disk_cache_override}" == "1" ]]; then
    cmd+=(--disk-cache)
  else
    cmd+=(--no-disk-cache)
  fi
fi
if [[ "${disk_cache_audit_clean}" -eq 1 ]]; then
  cmd+=(--disk-cache-audit-clean)
fi
if [[ "${font_mode}" == "bundled" ]]; then
  cmd+=(--bundled-fonts)
elif [[ "${font_mode}" == "system" ]]; then
  cmd+=(--system-fonts)
fi
if [[ -n "${pages}" ]]; then
  cmd+=(--pages "${pages}")
fi
if [[ -n "${shard}" ]]; then
  cmd+=(--shard "${shard}")
fi
if [[ "${allow_http_error_status}" -eq 1 ]]; then
  cmd+=(--allow-http-error-status)
fi
if [[ "${allow_collisions}" -eq 1 ]]; then
  cmd+=(--allow-collisions)
fi
if [[ "${timings}" -eq 1 ]]; then
  cmd+=(--timings)
fi
if [[ "${accuracy}" -eq 1 ]]; then
  cmd+=(--accuracy)
fi
if [[ -n "${accuracy_baseline}" ]]; then
  cmd+=(--accuracy-baseline "${accuracy_baseline}")
fi
if [[ -n "${accuracy_baseline_dir}" ]]; then
  cmd+=(--accuracy-baseline-dir "${accuracy_baseline_dir}")
fi
if [[ -n "${accuracy_tolerance}" ]]; then
  cmd+=(--accuracy-tolerance "${accuracy_tolerance}")
fi
if [[ -n "${accuracy_max_diff_percent}" ]]; then
  cmd+=(--accuracy-max-diff-percent "${accuracy_max_diff_percent}")
fi
if [[ -n "${accuracy_diff_dir}" ]]; then
  cmd+=(--accuracy-diff-dir "${accuracy_diff_dir}")
fi
if [[ "${capture_missing_failure_fixtures}" -eq 1 ]]; then
  cmd+=(--capture-missing-failure-fixtures)
fi
if [[ -n "${capture_missing_failure_fixtures_out_dir}" ]]; then
  cmd+=(
    --capture-missing-failure-fixtures-out-dir
    "${capture_missing_failure_fixtures_out_dir}"
  )
fi
if [[ "${capture_missing_failure_fixtures_allow_missing_resources}" -eq 1 ]]; then
  cmd+=(--capture-missing-failure-fixtures-allow-missing-resources)
fi
if [[ "${capture_missing_failure_fixtures_overwrite}" -eq 1 ]]; then
  cmd+=(--capture-missing-failure-fixtures-overwrite)
fi
if [[ "${capture_worst_accuracy_fixtures}" -eq 1 ]]; then
  cmd+=(--capture-worst-accuracy-fixtures)
fi
if [[ -n "${capture_worst_accuracy_fixtures_out_dir}" ]]; then
  cmd+=(--capture-worst-accuracy-fixtures-out-dir "${capture_worst_accuracy_fixtures_out_dir}")
fi
if [[ -n "${capture_worst_accuracy_fixtures_min_diff_percent}" ]]; then
  cmd+=(
    --capture-worst-accuracy-fixtures-min-diff-percent
    "${capture_worst_accuracy_fixtures_min_diff_percent}"
  )
fi
if [[ -n "${capture_worst_accuracy_fixtures_top}" ]]; then
  cmd+=(--capture-worst-accuracy-fixtures-top "${capture_worst_accuracy_fixtures_top}")
fi
if [[ "${capture_worst_accuracy_fixtures_allow_missing_resources}" -eq 1 ]]; then
  cmd+=(--capture-worst-accuracy-fixtures-allow-missing-resources)
fi
if [[ "${capture_worst_accuracy_fixtures_overwrite}" -eq 1 ]]; then
  cmd+=(--capture-worst-accuracy-fixtures-overwrite)
fi

if [[ ${#extra_args[@]} -gt 0 ]]; then
  cmd+=(-- "${extra_args[@]}")
fi

print_cmd() {
  local first=1
  for arg in "$@"; do
    if [[ "${first}" -eq 1 ]]; then
      printf '%q' "${arg}"
      first=0
    else
      printf ' %q' "${arg}"
    fi
  done
  printf '\n'
}

if [[ "${dry_run}" -eq 1 ]]; then
  print_cmd "${cmd[@]}"
  exit 0
fi

exec "${cmd[@]}"
