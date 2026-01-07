#!/usr/bin/env bash
set -euo pipefail

# Run any command under OS-enforced resource limits.
#
# Prefer `prlimit` when available (hard limits). Fall back to `ulimit` otherwise.
#
# Examples:
#   scripts/run_limited.sh --as 12G --cpu 60 -- cargo bench --bench selector_bloom_bench
#   LIMIT_AS=12G scripts/run_limited.sh -- cargo run --release --bin pageset_progress -- run --timeout 5

usage() {
  cat <<'EOF'
usage: scripts/run_limited.sh [--as <size>] [--rss <size>] [--stack <size>] [--cpu <secs>] -- <command...>

Limits:
  --as <size>     Address-space (virtual memory) limit. Example: 12G, 4096M.
  --rss <size>    Resident set size limit (advisory on many kernels).
  --stack <size>  Stack size limit.
  --cpu <secs>    CPU time limit (seconds).

Environment defaults (optional):
  LIMIT_AS, LIMIT_RSS, LIMIT_STACK, LIMIT_CPU

Notes:
  - `--as` is the most reliable “hard memory ceiling” on Linux.
  - If `prlimit` is missing, we fall back to `ulimit`. In that mode, size strings without a
    suffix are interpreted as MiB. With `prlimit`, bare numbers are bytes.
EOF
}

to_kib() {
  local raw="${1:-}"
  raw="${raw//[[:space:]]/}"
  raw="${raw,,}"

  # Accept common suffixes: k, m, g, t (optionally with b/ib).
  raw="${raw%ib}"
  raw="${raw%b}"

  if [[ "${raw}" =~ ^[0-9]+$ ]]; then
    # Fallback: treat as MiB (human-friendly for ulimit -v/-s which expect KiB).
    echo $((raw * 1024))
    return 0
  fi

  if [[ "${raw}" =~ ^([0-9]+)([kmgt])$ ]]; then
    local n="${BASH_REMATCH[1]}"
    local unit="${BASH_REMATCH[2]}"
    case "${unit}" in
      k) echo $((n)) ;;
      m) echo $((n * 1024)) ;;
      g) echo $((n * 1024 * 1024)) ;;
      t) echo $((n * 1024 * 1024 * 1024)) ;;
      *) return 1 ;;
    esac
    return 0
  fi

  return 1
}

# Convert human-friendly sizes to bytes for `prlimit`.
#
# Unlike `ulimit`, `prlimit` expects byte counts, and its CLI treats bare numbers as bytes.
# Normalize suffix inputs (e.g. `12G`) to a byte count so callers can use a consistent size syntax
# across both modes. (Some environments ship a buggy `prlimit` build that segfaults on suffix
# parsing, so we always pass the computed byte count.)
to_bytes() {
  local raw="${1:-}"
  raw="${raw//[[:space:]]/}"
  raw="${raw,,}"

  # Accept common suffixes: k, m, g, t (optionally with b/ib).
  raw="${raw%ib}"
  raw="${raw%b}"

  if [[ "${raw}" =~ ^[0-9]+$ ]]; then
    # In prlimit mode, bare numbers are bytes (matches prlimit CLI semantics).
    echo "${raw}"
    return 0
  fi

  if [[ "${raw}" =~ ^([0-9]+)([kmgt])$ ]]; then
    local n="${BASH_REMATCH[1]}"
    local unit="${BASH_REMATCH[2]}"
    case "${unit}" in
      k) echo $((n * 1024)) ;;
      m) echo $((n * 1024 * 1024)) ;;
      g) echo $((n * 1024 * 1024 * 1024)) ;;
      t) echo $((n * 1024 * 1024 * 1024 * 1024)) ;;
      *) return 1 ;;
    esac
    return 0
  fi

  return 1
}

# NOTE: Rust's toolchain shims (rustup) reserve a fairly large amount of virtual address space.
# A too-low RLIMIT_AS can prevent even `cargo --version` from starting. Keep the default high
# enough to allow basic commands, but still bounded to protect multi-agent hosts.
AS="${LIMIT_AS:-12G}"
RSS="${LIMIT_RSS:-}"
STACK="${LIMIT_STACK:-}"
CPU="${LIMIT_CPU:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --as)
      AS="${2:-}"; shift 2 ;;
    --rss)
      RSS="${2:-}"; shift 2 ;;
    --stack)
      STACK="${2:-}"; shift 2 ;;
    --cpu)
      CPU="${2:-}"; shift 2 ;;
    --no-as)
      AS=""; shift ;;
    --no-rss)
      RSS=""; shift ;;
    --no-stack)
      STACK=""; shift ;;
    --no-cpu)
      CPU=""; shift ;;
    --)
      shift
      break
      ;;
    *)
      # No more wrapper flags; treat rest as the command.
      break
      ;;
  esac
done

if [[ $# -lt 1 ]]; then
  usage
  exit 2
fi

cmd=("$@")

# Note: some environments ship a `prlimit` build that segfaults when setting `--as` (RLIMIT_AS).
# Prefer `ulimit` for address-space limits, and only use `prlimit` when `--as` is disabled.
if command -v prlimit >/dev/null 2>&1 && [[ -z "${AS}" || "${AS}" == "0" ]]; then
  pl=(prlimit)
  if [[ -n "${RSS}" && "${RSS}" != "0" ]]; then
    if [[ "${RSS}" == "unlimited" ]]; then
      pl+=(--rss=unlimited)
    else
      rss_bytes="$(to_bytes "${RSS}")" || {
        echo "invalid --rss size: ${RSS}" >&2
        exit 2
      }
      pl+=(--rss="${rss_bytes}")
    fi
  fi
  if [[ -n "${STACK}" && "${STACK}" != "0" ]]; then
    if [[ "${STACK}" == "unlimited" ]]; then
      pl+=(--stack=unlimited)
    else
      stack_bytes="$(to_bytes "${STACK}")" || {
        echo "invalid --stack size: ${STACK}" >&2
        exit 2
      }
      pl+=(--stack="${stack_bytes}")
    fi
  fi
  if [[ -n "${CPU}" && "${CPU}" != "0" ]]; then
    pl+=(--cpu="${CPU}")
  fi
  exec "${pl[@]}" -- "${cmd[@]}"
fi

# Fallback: ulimit. (Not all resources are enforceable; RSS is typically ignored.)
if [[ -n "${AS}" && "${AS}" != "0" ]]; then
  if [[ "${AS}" == "unlimited" ]]; then
    ulimit -v unlimited
  else
    as_kib="$(to_kib "${AS}")" || {
      echo "invalid --as size: ${AS}" >&2
      exit 2
    }
    ulimit -v "${as_kib}"
  fi
fi
if [[ -n "${STACK}" && "${STACK}" != "0" ]]; then
  if [[ "${STACK}" == "unlimited" ]]; then
    ulimit -s unlimited
  else
    stack_kib="$(to_kib "${STACK}")" || {
      echo "invalid --stack size: ${STACK}" >&2
      exit 2
    }
    ulimit -s "${stack_kib}"
  fi
fi
if [[ -n "${CPU}" && "${CPU}" != "0" ]]; then
  if ! [[ "${CPU}" =~ ^[0-9]+$ ]]; then
    echo "invalid --cpu seconds: ${CPU}" >&2
    exit 2
  fi
  ulimit -t "${CPU}"
fi

exec "${cmd[@]}"
