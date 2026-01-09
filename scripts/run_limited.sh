#!/usr/bin/env bash
set -euo pipefail

# Run any command under OS-enforced resource limits.
#
# Prefer `prlimit` when available (hard limits). Fall back to `ulimit` otherwise.
#
# Examples:
#   scripts/run_limited.sh --as 64G -- cargo bench --bench selector_bloom_bench
#   LIMIT_AS=64G scripts/run_limited.sh -- cargo run --release --bin pageset_progress -- run --timeout 5

usage() {
  cat <<'EOF'
usage: scripts/run_limited.sh [--as <size>] [--rss <size>] [--stack <size>] [--cpu <secs>] -- <command...>

Limits:
  --as <size>     Address-space (virtual memory) limit. Example: 64G, 4096M.
  --rss <size>    Resident set size limit (advisory on many kernels).
  --stack <size>  Stack size limit.
  --cpu <secs>    CPU time limit (seconds).

Environment defaults (optional):
  LIMIT_AS, LIMIT_RSS, LIMIT_STACK, LIMIT_CPU

Notes:
  - `--as` is the most reliable “hard memory ceiling” on Linux.
  - If `prlimit` is missing, we fall back to `ulimit`.
  - Size strings without a suffix are interpreted as MiB.
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

to_bytes() {
  local kib
  kib="$(to_kib "${1:-}")" || return 1
  echo $((kib * 1024))
}

# NOTE: Rust's toolchain shims (rustup) reserve a fairly large amount of virtual address space.
# A too-low RLIMIT_AS can prevent even `cargo --version` from starting. Keep the default high
# enough to allow basic commands, but still bounded to protect multi-agent hosts.
AS="${LIMIT_AS:-64G}"
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

# Windows (Git Bash / MSYS / Cygwin) note:
#
# This repo primarily relies on Linux/macOS resource limits (RLIMIT_AS via prlimit/ulimit) to keep
# hostile inputs from exhausting RAM. The POSIX `ulimit` knobs are not reliably supported when
# running under Windows shells (Git Bash / MSYS / Cygwin); attempting to set them can fail with
# "invalid argument" and break workflows that shell out through this script (including
# `scripts/cargo_agent.sh`).
#
# When running on Windows, treat this script as a no-op wrapper and run the command without
# applying limits.
uname_s="$(uname -s 2>/dev/null || echo "")"
case "${uname_s}" in
  MINGW*|MSYS*|CYGWIN*)
    exec "${cmd[@]}"
    ;;
esac

any_limit=false
if [[ -n "${AS}" && "${AS}" != "0" && "${AS}" != "unlimited" ]]; then any_limit=true; fi
if [[ -n "${RSS}" && "${RSS}" != "0" ]]; then any_limit=true; fi
if [[ -n "${STACK}" && "${STACK}" != "0" && "${STACK}" != "unlimited" ]]; then any_limit=true; fi
if [[ -n "${CPU}" && "${CPU}" != "0" && "${CPU}" != "unlimited" ]]; then any_limit=true; fi

if [[ "${any_limit}" == "false" ]]; then
  exec "${cmd[@]}"
fi

# Rustup's shim binaries (`cargo`, `rustc`, ...) can reserve a large amount of virtual address
# space. When RLIMIT_AS is set, they may fail before delegating to the real toolchain binary.
# Resolve `cargo` to the actual toolchain executable before applying limits so callers can use
# `scripts/run_limited.sh --as ... -- cargo ...` reliably.
if [[ -n "${AS}" && "${AS}" != "0" && "${AS}" != "unlimited" ]] \
  && [[ "${cmd[0]}" == "cargo" ]] \
  && command -v rustup >/dev/null 2>&1
then
  cargo_shim="$(command -v cargo || true)"
  if [[ -n "${cargo_shim}" ]]; then
    cargo_target="${cargo_shim}"
    if [[ -L "${cargo_shim}" ]]; then
      cargo_target="$(readlink "${cargo_shim}" 2>/dev/null || echo "${cargo_shim}")"
    fi

    if [[ "${cargo_target}" == "rustup" || "${cargo_target}" == */rustup || "${cargo_shim}" == */.cargo/bin/cargo ]]; then
      toolchain=""
      if [[ ${#cmd[@]} -gt 1 && "${cmd[1]}" == +* ]]; then
        toolchain="${cmd[1]#+}"
        cmd=("${cmd[0]}" "${cmd[@]:2}")
      fi

      if [[ -n "${toolchain}" ]]; then
        resolved="$(rustup which --toolchain "${toolchain}" cargo 2>/dev/null || true)"
      else
        resolved="$(rustup which cargo 2>/dev/null || true)"
      fi

      if [[ -n "${resolved}" ]]; then
        cmd[0]="${resolved}"
        toolchain_bin="$(dirname "${resolved}")"
        export PATH="${toolchain_bin}:${PATH}"
      fi
    fi
  fi
fi

prlimit_ok=0
if command -v prlimit >/dev/null 2>&1; then
  # Some CI/container environments ship a broken `prlimit` binary that immediately segfaults. Run a
  # cheap self-test and fall back to `ulimit` when `prlimit` is unusable.
  #
  # Use explicit byte counts to avoid any suffix parsing differences across `prlimit` builds.
  if prlimit --as=67108864 --cpu=1 -- true >/dev/null 2>&1; then
    prlimit_ok=1
  fi
fi

if [[ "${prlimit_ok}" -eq 1 ]]; then
  # Apply limits to the current process (inherited by `exec`). We normalize size inputs to bytes
  # because `prlimit` expects byte counts and some builds mis-handle suffix parsing.
  pl=(prlimit --pid $$)
  if [[ -n "${AS}" && "${AS}" != "0" ]]; then
    if [[ "${AS}" == "unlimited" ]]; then
      pl+=(--as=unlimited)
    else
      as_bytes="$(to_bytes "${AS}")" || {
        echo "invalid --as size: ${AS}" >&2
        exit 2
      }
      pl+=(--as="${as_bytes}")
    fi
  fi
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
    if [[ "${CPU}" == "unlimited" ]]; then
      pl+=(--cpu=unlimited)
    else
      if ! [[ "${CPU}" =~ ^[0-9]+$ ]]; then
        echo "invalid --cpu seconds: ${CPU}" >&2
        exit 2
      fi
      pl+=(--cpu="${CPU}")
    fi
  fi

  if "${pl[@]}" >/dev/null 2>&1; then
    exec "${cmd[@]}"
  fi
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
  if [[ "${CPU}" == "unlimited" ]]; then
    ulimit -t unlimited
  else
    if ! [[ "${CPU}" =~ ^[0-9]+$ ]]; then
      echo "invalid --cpu seconds: ${CPU}" >&2
      exit 2
    fi
    ulimit -t "${CPU}"
  fi
fi

exec "${cmd[@]}"
