#!/usr/bin/env bash
set -euo pipefail

# Some environments inject `RUSTC_WRAPPER=sccache` without a running sccache daemon, which causes
# builds to fail with errors like "Failed to send data to or receive data from server".
# Prefer deterministic builds by default; opt back in with FASTR_CARGO_USE_SCCACHE=1.
if [[ "${FASTR_CARGO_USE_SCCACHE:-0}" != "1" ]]; then
  export RUSTC_WRAPPER=
  export SCCACHE_DISABLE=1
fi

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
#   FASTR_XTASK_LIMIT_AS     Address-space cap for `cargo run -p xtask` (default: 96G)
#   FASTR_CARGO_LOCK_DIR     Lock directory (default: target/.cargo_agent_locks)
#   FASTR_RUST_TEST_THREADS  Default `RUST_TEST_THREADS` for `cargo test` (default: min(nproc, 32))
#
# Notes:
# - libtest defaults `RUST_TEST_THREADS` to the full CPU count. On multi-agent hosts with hundreds
#   of cores, that can create massive runtime contention and flakiness (especially for tests that
#   spin up local TCP servers). This wrapper sets a conservative default for `cargo test` unless
#   you explicitly override it.

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
  FASTR_XTASK_LIMIT_AS     Address-space cap for `cargo run -p xtask` (default: 96G)
  FASTR_CARGO_LOCK_DIR     Lock directory (default: target/.cargo_agent_locks)
  FASTR_RUST_TEST_THREADS  Default RUST_TEST_THREADS for `cargo test` (default: min(nproc, 32))

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

# Compatibility: allow callers to pass `-p <pkg>` before the subcommand.
#
# Cargo requires `-p/--package` to come *after* the subcommand:
#   cargo test -p my_crate
# not:
#   cargo -p my_crate test
#
# Some internal docs/tools historically used the latter ordering, so we accept it here and
# normalize to Cargo's expected argv shape.
if [[ "${1:-}" == +* && ( "${2:-}" == "-p" || "${2:-}" == "--package" ) ]]; then
  if [[ $# -lt 4 ]]; then
    usage
    exit 2
  fi
  toolchain="$1"
  pkg="$3"
  subcmd="$4"
  shift 4
  set -- "${toolchain}" "${subcmd}" -p "${pkg}" "$@"
elif [[ "${1:-}" == "-p" || "${1:-}" == "--package" ]]; then
  if [[ $# -lt 3 ]]; then
    usage
    exit 2
  fi
  pkg="$2"
  subcmd="$3"
  shift 3
  set -- "${subcmd}" -p "${pkg}" "$@"
fi

# Compatibility: older docs/tests refer to the layout integration test target as `layout`, but the
# aggregator binary is named `layout_tests` (see tests/layout_tests.rs).
#
# Accept `--test layout` and rewrite it to the actual target name so guidance like:
#   scripts/cargo_agent.sh test -p fastrender --test layout <filter>
# keeps working.
argv=("$@")
for ((i = 0; i < ${#argv[@]}; i++)); do
  if [[ "${argv[$i]}" == "--test" && "${argv[$((i + 1))]:-}" == "layout" ]]; then
    argv[$((i + 1))]="layout_tests"
  elif [[ "${argv[$i]}" == "--test=layout" ]]; then
    argv[$i]="--test=layout_tests"
  fi
done
set -- "${argv[@]}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Some CI/agent environments configure `build.rustc-wrapper = "sccache"` in a global Cargo config.
# When the sccache daemon is unhealthy, it can fail *some* compilations mid-run and surface as a
# spurious `could not compile ... process didn't exit successfully: sccache rustc ...` error.
#
# Prefer reliability over caching by default: unless the caller explicitly opted into a wrapper,
# override to a no-op wrapper (`env`) so Cargo executes `env rustc ...` instead of using sccache.
#
# Callers that want sccache can export `RUSTC_WRAPPER=sccache` (or `CARGO_BUILD_RUSTC_WRAPPER`)
# before invoking this script.
if [[ -z "${RUSTC_WRAPPER:-}" && -z "${CARGO_BUILD_RUSTC_WRAPPER:-}" ]]; then
  export RUSTC_WRAPPER="env"
  export CARGO_BUILD_RUSTC_WRAPPER="env"
fi

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

limit_as_defaulted=0
if [[ -z "${FASTR_CARGO_LIMIT_AS:-}" && -z "${LIMIT_AS:-}" ]]; then
  limit_as="64G"
  limit_as_defaulted=1
else
  limit_as="${FASTR_CARGO_LIMIT_AS:-${LIMIT_AS:-64G}}"
fi

# `cargo run -p xtask` is a special case: several xtask commands (page-loop, fixture-chrome-diff,
# chrome-baseline-fixtures) spawn headless Chrome. Recent Chrome builds reserve a very large virtual
# address space up front (~75GiB on Chrome 143), which causes "Oilpan: Out of memory" failures when
# RLIMIT_AS is set to the default 64G.
#
# Keep the default limit (64G) for normal cargo commands, but bump it for xtask runs unless the
# caller explicitly requested a different limit via FASTR_CARGO_LIMIT_AS/LIMIT_AS.
if [[ "${limit_as_defaulted}" -eq 1 ]]; then
  argv=("$@")
  subcmd_pos=0
  if [[ "${argv[0]:-}" == +* ]]; then
    subcmd_pos=1
  fi
  subcmd="${argv[${subcmd_pos}]:-}"
  if [[ "${subcmd}" == "run" ]]; then
    for ((i = subcmd_pos + 1; i < ${#argv[@]}; i++)); do
      if [[ "${argv[$i]}" == "-p" || "${argv[$i]}" == "--package" ]]; then
        if [[ "${argv[$((i + 1))]:-}" == "xtask" ]]; then
          limit_as="${FASTR_XTASK_LIMIT_AS:-96G}"
          break
        fi
      elif [[ "${argv[$i]}" == --package=* ]]; then
        if [[ "${argv[$i]#--package=}" == "xtask" ]]; then
          limit_as="${FASTR_XTASK_LIMIT_AS:-96G}"
          break
        fi
      fi
    done
  fi
fi

lock_dir="${FASTR_CARGO_LOCK_DIR:-${repo_root}/target/.cargo_agent_locks}"
mkdir -p "${lock_dir}"

run_cargo() {
  local cargo_cmd=(cargo)
  local subcommand=""

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

  subcommand="$1"
  cargo_cmd+=("${subcommand}")
  shift

  if [[ "${subcommand}" == "test" && -z "${RUST_TEST_THREADS:-}" ]]; then
    local rust_test_threads="${FASTR_RUST_TEST_THREADS:-}"
    if [[ -z "${rust_test_threads}" ]]; then
      rust_test_threads=$(( nproc < 32 ? nproc : 32 ))
    fi
    export RUST_TEST_THREADS="${rust_test_threads}"
  fi

  if [[ -n "${jobs}" ]]; then
    cargo_cmd+=(-j "${jobs}")
  fi

  cargo_cmd+=("$@")

  if [[ -z "${limit_as}" || "${limit_as}" == "0" || "${limit_as}" == "off" ]]; then
    "${cargo_cmd[@]}"
    return $?
  fi

  # Invoke through `bash`:
  # - Some agent/CI environments mount repos with `noexec`, which prevents executing scripts directly.
  # - Some checkouts (including CI artifact tars) may drop the executable bit on shell scripts.
  bash "${repo_root}/scripts/run_limited.sh" --as "${limit_as}" -- "${cargo_cmd[@]}"
  return $?
}

# Nested invocation support:
#
# Many local workflows run `xtask` via this wrapper:
#   bash scripts/cargo_agent.sh xtask <subcommand>
#
# The xtask binary itself often needs to run additional scoped cargo commands (also via this
# wrapper). If we try to acquire another slot lock while the parent `cargo_agent.sh` invocation is
# still holding the only available slot (common on small machines where the default slot count is
# 1), the nested invocation will deadlock.
#
# Detect when we are already running under a cargo_agent slot and skip slot acquisition in that
# case. We still enforce the per-command RLIMIT_AS cap and optional `-j` throttling.
if [[ -n "${FASTR_CARGO_SLOT:-}" ]]; then
  jobs_label="${jobs:-auto}"
  echo "cargo_agent: nested slot=${FASTR_CARGO_SLOT} jobs=${jobs_label} as=${limit_as}" >&2
  run_cargo "$@"
  exit $?
fi

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
