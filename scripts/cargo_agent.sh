#!/usr/bin/env bash
set -euo pipefail

# High-throughput cargo wrapper for multi-agent hosts.
#
# Goals:
# - Maximize utilization on big machines (many cores)
# - Avoid cargo/rustc/rust-lld stampedes when many agents run commands concurrently
# - Enforce a per-command RAM ceiling (no CPU limiting)
#
# Usage:
#   scripts/cargo_agent.sh build --release
#   scripts/cargo_agent.sh test --lib
#   scripts/cargo_agent.sh test --test wpt_test -- --exact wpt_local_suite_passes
#
# Tuning knobs (env vars):
#   FASTR_CARGO_SLOTS        Max concurrent cargo commands (default: auto from CPU)
#   FASTR_CARGO_JOBS         cargo build jobs per command (default: cargo's default)
#   FASTR_CARGO_LIMIT_AS     Address-space cap forwarded to run_limited (default: 64G)
#   FASTR_CARGO_LOCK_DIR     Lock directory (default: target/.cargo_agent_locks)
#
# Notes:
# - This wrapper intentionally DOES NOT set RUST_TEST_THREADS / RAYON_NUM_THREADS globally.
#   Leave that to the caller when running tests that spawn threads.

usage() {
  cat <<'EOF'
usage: scripts/cargo_agent.sh <cargo-subcommand> [args...] [-- <test-args...>]

Examples:
  scripts/cargo_agent.sh check --quiet
  scripts/cargo_agent.sh build --release
  scripts/cargo_agent.sh test --lib
  scripts/cargo_agent.sh test --test wpt_test -- --exact wpt_local_suite_passes

Environment:
  FASTR_CARGO_SLOTS        Max concurrent cargo commands (default: auto)
  FASTR_CARGO_JOBS         cargo build jobs per command (default: cargo's default)
  FASTR_CARGO_LIMIT_AS     Address-space cap (default: 64G)
  FASTR_CARGO_LOCK_DIR     Lock directory (default: target/.cargo_agent_locks)

Notes:
  - This wrapper is intentionally simple:
    - It limits how many *cargo commands* can run concurrently (slots).
    - It enforces a RAM cap via RLIMIT_AS by default (through scripts/run_limited.sh).
  - Set FASTR_CARGO_JOBS to force a fixed -j value (e.g. FASTR_CARGO_JOBS=192). When unset, we
    do not pass `-j` and let Cargo choose.
  - Set FASTR_CARGO_LIMIT_AS to override the default RAM cap (or `unlimited` to disable).
EOF
}

if [[ $# -lt 1 ]]; then
  usage
  exit 2
fi

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

nproc="${FASTR_CARGO_NPROC:-}"
if [[ -z "${nproc}" ]]; then
  if command -v nproc >/dev/null 2>&1; then
    nproc="$(nproc)"
  else
    nproc="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"
  fi
fi

# Default slots: keep concurrency low by default since each command uses -j nproc.
slots="${FASTR_CARGO_SLOTS:-}"
if [[ -z "${slots}" ]]; then
  # Avoid cargo stampedes: allow a few concurrent cargo commands, but not "one per agent".
  # Heuristic: ~1 concurrent cargo per 48 hw threads (clamped).
  slots=$(( nproc / 48 ))
  if [[ "${slots}" -lt 1 ]]; then slots=1; fi
  if [[ "${slots}" -gt 8 ]]; then slots=8; fi
fi

jobs="${FASTR_CARGO_JOBS:-}"
if [[ -n "${jobs}" ]]; then
  if ! [[ "${jobs}" =~ ^[0-9]+$ ]] || [[ "${jobs}" -lt 1 ]]; then
    echo "invalid FASTR_CARGO_JOBS: ${jobs}" >&2
    exit 2
  fi
fi

limit_as="${FASTR_CARGO_LIMIT_AS:-${LIMIT_AS:-64G}}"

lock_dir="${FASTR_CARGO_LOCK_DIR:-${repo_root}/target/.cargo_agent_locks}"
mkdir -p "${lock_dir}"

run_cargo() {
  local cargo_cmd=(cargo)

  # Cargo expects `-j/--jobs` to appear after the subcommand (`cargo test -j 1`, not `cargo -j 1 test`).
  # See: https://doc.rust-lang.org/cargo/commands/cargo.html
  #
  # Support optional toolchain syntax (`cargo +nightly test ...`) even though the wrapper primarily
  # targets `scripts/cargo_agent.sh <subcommand> ...`.
  if [[ $# -lt 1 ]]; then
    echo "missing cargo subcommand" >&2
    return 2
  fi

  if [[ "$1" == +* ]]; then
    cargo_cmd+=("$1")
    shift
    if [[ $# -lt 1 ]]; then
      echo "missing cargo subcommand after toolchain spec" >&2
      return 2
    fi
  fi

  cargo_cmd+=("$1")
  shift

  if [[ -n "${jobs}" ]]; then
    cargo_cmd+=(-j "${jobs}")
  fi

  cargo_cmd+=("$@")

  if [[ -z "${limit_as}" || "${limit_as}" == "0" || "${limit_as}" == "off" ]]; then
    "${cargo_cmd[@]}"
    return $?
  fi

  "${repo_root}/scripts/run_limited.sh" --as "${limit_as}" -- "${cargo_cmd[@]}"
  return $?
}

if ! command -v flock >/dev/null 2>&1; then
  echo "warning: flock not available; running cargo without slot throttling" >&2
  run_cargo "$@"
  exit $?
fi

acquire_slot() {
  local i k start lockfile fd
  # Avoid hot-spotting slot 0 (and reduce starvation risk) by picking a rotating start index.
  start=$(( ($$ + RANDOM) % slots ))
  for ((k = 0; k < slots; k++)); do
    i=$(( (start + k) % slots ))
    lockfile="${lock_dir}/slot.${i}.lock"
    exec {fd}>"${lockfile}" || continue
    if flock -n "${fd}"; then
      echo "${fd}:${i}"
      return 0
    fi
    exec {fd}>&-
  done
  return 1
}

slot=""
while [[ -z "${slot}" ]]; do
  if s="$(acquire_slot)"; then
    slot="${s}"
    break
  fi
  sleep 0.1
done

slot_fd="${slot%%:*}"
slot_idx="${slot#*:}"
export FASTR_CARGO_SLOT="${slot_idx}"

jobs_label="${jobs:-auto}"
echo "cargo_agent: slot=${slot_idx}/${slots} jobs=${jobs_label} as=${limit_as}" >&2

set +e
run_cargo "$@"
status=$?
set -e

# Release lock.
exec {slot_fd}>&-
exit "${status}"
