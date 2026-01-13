#!/usr/bin/env bash
set -euo pipefail

# sccache (shared compilation cache) can dramatically speed up multi-agent environments
# by caching compiled artifacts. However, some environments inject RUSTC_WRAPPER=sccache
# without a running daemon, causing failures.
#
# Enable sccache explicitly with:
#   FASTR_CARGO_USE_SCCACHE=1 bash scripts/cargo_agent.sh build ...
#
# Note: sccache and incremental compilation don't mix well. When sccache is enabled,
# consider disabling incremental for clean builds:
#   FASTR_CARGO_USE_SCCACHE=1 FASTR_CARGO_INCREMENTAL=0 bash scripts/cargo_agent.sh build ...
if [[ "${FASTR_CARGO_USE_SCCACHE:-0}" != "1" ]]; then
  export RUSTC_WRAPPER=
  export CARGO_BUILD_RUSTC_WRAPPER=
  export SCCACHE_DISABLE=1
fi

# Incremental compilation is ENABLED by default for faster iteration.
#
# Previous rationale for disabling: "one-shot CI builds". However, analysis shows that:
# - Agent iteration on the same clone benefits massively from incremental (~11s vs ~60s)
# - Even "clean" builds with incremental enabled have minimal overhead
# - The mega-crate architecture (1.38M lines) makes incremental essential
#
# Disable only for CI release builds where reproducibility matters:
#   FASTR_CARGO_INCREMENTAL=0 bash scripts/cargo_agent.sh build --release
if [[ "${FASTR_CARGO_INCREMENTAL:-1}" != "0" && -z "${CARGO_INCREMENTAL:-}" ]]; then
  export CARGO_INCREMENTAL=1
fi
if [[ "${FASTR_CARGO_DEBUG_INFO:-0}" != "1" ]]; then
  if [[ -z "${CARGO_PROFILE_DEV_DEBUG:-}" ]]; then
    export CARGO_PROFILE_DEV_DEBUG=0
  fi
  if [[ -z "${CARGO_PROFILE_TEST_DEBUG:-}" ]]; then
    export CARGO_PROFILE_TEST_DEBUG=0
  fi
fi

# libaom (used for AVIF decoding via `avif-decode` → `aom-decode` → `libaom-sys`) requires an
# assembler (yasm/nasm) for optimized builds. Our agent environments may not have these tools
# installed, so default to the portable (non-asm) build.
#
# Allow callers to opt back into optimized builds by exporting these variables in their shell.
if [[ -z "${AOM_TARGET_CPU:-}" ]]; then
  export AOM_TARGET_CPU="generic"
fi
if [[ -z "${CMAKE_ARGS:-}" ]]; then
  export CMAKE_ARGS="-DAOM_TARGET_CPU=generic -DENABLE_NASM=0 -DENABLE_YASM=0 -DENABLE_ASM=0"
else
  if [[ "${CMAKE_ARGS}" != *"AOM_TARGET_CPU"* ]]; then
    CMAKE_ARGS="${CMAKE_ARGS} -DAOM_TARGET_CPU=generic"
  fi
  if [[ "${CMAKE_ARGS}" != *"ENABLE_NASM"* ]]; then
    CMAKE_ARGS="${CMAKE_ARGS} -DENABLE_NASM=0"
  fi
  if [[ "${CMAKE_ARGS}" != *"ENABLE_YASM"* ]]; then
    CMAKE_ARGS="${CMAKE_ARGS} -DENABLE_YASM=0"
  fi
  if [[ "${CMAKE_ARGS}" != *"ENABLE_ASM"* ]]; then
    CMAKE_ARGS="${CMAKE_ARGS} -DENABLE_ASM=0"
  fi
  export CMAKE_ARGS
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
#   scripts/cargo_agent.sh test --test integration -- --exact wpt::wpt_local_suite_passes
#   WPT_FILTER=layout/floats scripts/cargo_agent.sh test --test integration -- --exact wpt::wpt_local_suite_passes
#   scripts/cargo_agent.sh test --test allocation_failure
#
# Tuning knobs (env vars):
#   FASTR_CARGO_SLOTS        Max concurrent cargo commands (default: auto from CPU)
#   FASTR_CARGO_JOBS         cargo build jobs per command (default: auto from CPU/slots)
#   FASTR_CARGO_INCREMENTAL  Enable incremental compilation (default: 1, ENABLED)
#   FASTR_CARGO_DEBUG_INFO   Keep debug info enabled for dev/test builds (default: 0)
#   FASTR_CARGO_LIMIT_AS     Address-space cap forwarded to run_limited (default: 64G)
#   FASTR_XTASK_LIMIT_AS     Address-space cap for `scripts/cargo_agent.sh xtask ...` runs (default: 96G)
#   FASTR_FUZZ_LIMIT_AS      Address-space cap for `scripts/cargo_agent.sh fuzz ...` runs (default: 32T)
#   FASTR_CARGO_LOCK_DIR     Lock directory (default: auto; prefers shared cache dirs when available)
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
  scripts/cargo_agent.sh test --test integration -- --exact wpt::wpt_local_suite_passes
  WPT_FILTER=layout/floats scripts/cargo_agent.sh test --test integration -- --exact wpt::wpt_local_suite_passes
  scripts/cargo_agent.sh test --test allocation_failure

Environment:
  FASTR_CARGO_SLOTS        Max concurrent cargo commands (default: auto)
  FASTR_CARGO_JOBS         cargo build jobs per command (default: auto from CPU/slots)
  FASTR_CARGO_INCREMENTAL  Enable incremental compilation (default: 1, ENABLED)
  FASTR_CARGO_DEBUG_INFO   Keep debug info enabled for dev/test builds (default: 0)
  FASTR_CARGO_LIMIT_AS     Address-space cap (default: 64G)
  FASTR_XTASK_LIMIT_AS     Address-space cap for `scripts/cargo_agent.sh xtask ...` runs (default: 96G)
  FASTR_FUZZ_LIMIT_AS      Address-space cap for `scripts/cargo_agent.sh fuzz ...` runs (default: 32T)
  FASTR_CARGO_LOCK_DIR     Lock directory (default: auto)
  FASTR_RUST_TEST_THREADS  Default RUST_TEST_THREADS for `cargo test` (default: min(nproc, 32))

Notes:
  - This wrapper is intentionally simple:
    - It limits how many *cargo commands* can run concurrently (slots).
    - It enforces a RAM cap via RLIMIT_AS by default (through scripts/run_limited.sh).
  - Set FASTR_CARGO_JOBS to force a fixed -j value (e.g. FASTR_CARGO_JOBS=64). When unset, the
    wrapper sets `-j` automatically based on CPU + slot count to keep multi-agent hosts stable.
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

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Some agent/CI environments copy the repository without preserving executable bits. This is fine
# for most helper scripts (we invoke them via `bash ...`), but `libaom-sys` (AVIF decoding) executes
# the CMake wrapper directly via the `CMAKE` env var from `.cargo/config.toml`.
#
# Ensure the wrapper is runnable so AVIF-enabled builds don't fail with:
#   "failed to execute command: Permission denied (os error 13)"
if [[ -f "${repo_root}/tools/cmake_wrapper.sh" ]]; then
  chmod +x "${repo_root}/tools/cmake_wrapper.sh" 2>/dev/null || true
fi
# See `.cargo/config.toml` for why this exists.
if [[ -f "${repo_root}/tools/clang_wrapper.sh" ]]; then
  chmod +x "${repo_root}/tools/clang_wrapper.sh" 2>/dev/null || true
fi
# Directory to run `cargo` in.
#
# By default, we execute cargo from the monorepo root so it picks up:
# - `<repo_root>/.cargo/config.toml`
# - `<repo_root>/rust-toolchain.toml` (if present)
#
# However, `vendor/ecma-rs` is a nested workspace with its own `.cargo/config.toml`
# (notably `RUSTC_BOOTSTRAP=1` for `cargo-fuzz` + a few runtime crates). When we
# auto-scope a command to the nested workspace, run cargo from `vendor/ecma-rs/`
# so those settings are applied.
#
# Some tools (e.g. `vendor/ecma-rs/scripts/gen_deps_graph.sh`) run generic Cargo
# subcommands like `cargo metadata` without `-p/--package` flags. When invoked
# from inside the nested workspace, default to running Cargo from
# `vendor/ecma-rs/` so it discovers the correct workspace root.
cargo_workdir="${repo_root}"
caller_pwd="$(pwd -P)"
if [[ "${caller_pwd}" == "${repo_root}/vendor/ecma-rs" || "${caller_pwd}" == "${repo_root}/vendor/ecma-rs/"* ]]; then
  cargo_workdir="${repo_root}/vendor/ecma-rs"
fi

# Compatibility: `vendor/ecma-rs` is a nested workspace (excluded from the
# top-level Cargo workspace). Some workflows still want to run commands like:
#   bash scripts/cargo_agent.sh test -p optimize-js --lib
# from the repo root.
#
# If the requested package name matches a crate under `vendor/ecma-rs/` (usually
# `vendor/ecma-rs/<package-name>/Cargo.toml`, but see the special-cases below)
# and the caller did not explicitly provide `--manifest-path`, automatically
# scope the cargo invocation to `vendor/ecma-rs/Cargo.toml` so the package can
# be resolved.
#
# If the monorepo workspace also contains a package with the same name, prefer
# the monorepo package. To target the vendored workspace explicitly, use:
#   - `bash vendor/ecma-rs/scripts/cargo_agent.sh ...`, or
#   - `bash scripts/cargo_agent.sh ... --manifest-path vendor/ecma-rs/Cargo.toml`.
has_manifest_path=0
argv=("$@")
for ((i = 0; i < ${#argv[@]}; i++)); do
  # Stop once we reach the argument delimiter. Anything after `--` is forwarded
  # to rustc / the test harness / the executed binary, and should not be
  # interpreted as Cargo flags.
  if [[ "${argv[$i]}" == "--" ]]; then
    break
  fi
  if [[ "${argv[$i]}" == "--manifest-path" || "${argv[$i]}" == --manifest-path=* ]]; then
    has_manifest_path=1
    break
  fi
done

if [[ "${has_manifest_path}" -eq 0 ]]; then
  argv=("$@")
  subcmd_pos=0
  if [[ "${argv[0]:-}" == +* ]]; then
    subcmd_pos=1
  fi
  subcmd="${argv[${subcmd_pos}]:-}"

  # `xtask` is a wrapper-managed subcommand (`scripts/cargo_agent.sh xtask ...` builds the xtask
  # binary and then executes it directly). Arguments after `xtask` are xtask CLI flags, so do not
  # interpret `--package` / `-p` occurrences there as Cargo package selectors.
  if [[ "${subcmd}" != "xtask" ]]; then
    pkg=""
    for ((i = 0; i < ${#argv[@]}; i++)); do
      # Stop once we reach the argument delimiter. Anything after `--` is forwarded to rustc / the
      # test harness / the executed binary, and should not be interpreted as Cargo flags.
      if [[ "${argv[$i]}" == "--" ]]; then
        break
      fi
        case "${argv[$i]}" in
          -p|--package)
            pkg="${argv[$((i + 1))]:-}"
            ;;
          --package=*)
            pkg="${argv[$i]#--package=}"
            ;;
        esac

        # Convenience alias: the vendored legacy WebIDL runtime package lives at
        # `vendor/ecma-rs/webidl-runtime/` but its Cargo package name is `webidl-js-runtime`.
        # Accept `-p webidl-runtime` (directory name) as an alias so CI/docs can use either form.
        if [[ "${pkg}" == "webidl-runtime" ]]; then
          case "${argv[$i]}" in
            -p|--package)
              argv[$((i + 1))]="webidl-js-runtime"
              ;;
            --package=*)
              argv[$i]="--package=webidl-js-runtime"
              ;;
          esac
          pkg="webidl-js-runtime"
        fi

        # `cargo-fuzz` generates a standalone workspace under `fuzz/` (package name:
        # `fastrender-fuzz`). Allow selecting it as either:
        # - `-p fuzz` (convenient alias used in docs), or
        # - `-p fastrender-fuzz` (its real Cargo package name),
        #
        # and automatically scope the invocation to `fuzz/Cargo.toml` so it can be built from the
        # monorepo root.
        if [[ "${pkg}" == "fuzz" || "${pkg}" == "fastrender-fuzz" ]]; then
          if [[ "${pkg}" == "fuzz" ]]; then
            case "${argv[$i]}" in
              -p|--package)
                argv[$((i + 1))]="fastrender-fuzz"
                ;;
              --package=*)
                argv[$i]="--package=fastrender-fuzz"
                ;;
            esac
          fi

          insert_pos=$((subcmd_pos + 1))
          argv=(
            "${argv[@]:0:${insert_pos}}"
            --manifest-path "${repo_root}/fuzz/Cargo.toml"
            "${argv[@]:${insert_pos}}"
          )
          set -- "${argv[@]}"
          break
        fi

       if [[ -n "${pkg}" ]]; then
         # Prefer monorepo workspace packages when there is a name collision with
         # the nested `vendor/ecma-rs` workspace.
         #
        # Example: During WebIDL consolidation, `webidl-js-runtime` existed both
        # as a monorepo workspace crate (under `crates/`) and as a vendored
        # package (located at `vendor/ecma-rs/webidl-runtime/`; the Cargo package
        # name intentionally does not match the directory name). In that case
        # `scripts/cargo_agent.sh test -p webidl-js-runtime` should continue to
        # target the monorepo crate until it is removed, and then seamlessly
        # fall through to the vendored workspace.
        if [[ -f "${repo_root}/${pkg}/Cargo.toml" || -f "${repo_root}/crates/${pkg}/Cargo.toml" ]]; then
          continue
        fi
      fi

      vendored_pkg_dir="${pkg}"
      # Some ecma-rs packages intentionally use a different on-disk directory
      # name than their Cargo package name. Keep the mapping local to this
      # vendored-workspace auto-detection so we do not change Cargo's package
      # selection semantics (we still pass `-p <package>` through unchanged).
      case "${pkg}" in
        webidl-js-runtime)
          vendored_pkg_dir="webidl-runtime"
          ;;
      esac

      if [[ -n "${pkg}" && -f "${repo_root}/vendor/ecma-rs/${vendored_pkg_dir}/Cargo.toml" ]]; then
        insert_pos=$((subcmd_pos + 1))
        argv=(
          "${argv[@]:0:${insert_pos}}"
          --manifest-path "${repo_root}/vendor/ecma-rs/Cargo.toml"
          "${argv[@]:${insert_pos}}"
        )
        set -- "${argv[@]}"
        cargo_workdir="${repo_root}/vendor/ecma-rs"
        break
      fi
    done
  fi
fi

# `runtime-native` contains an FP-based stack walker / GC root enumerator and enforces
# `-C force-frame-pointers=yes` via its build script.
#
# `native-js` and `native-js-cli` depend on `runtime-native`, so building them also requires frame
# pointers.
#
# Note: `runtime-native` also exposes an `allow_omit_frame_pointers` feature as an escape hatch for
# experiments. Respect it: if the caller explicitly enables the feature, do not force-inject frame
# pointers (they can still opt in by setting RUSTFLAGS themselves).
needs_frame_pointers=0
allow_omit_frame_pointers=0
is_frame_pointer_pkg() {
  case "$1" in
    runtime-native|native-js|native-js-cli)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

argv=("$@")
for ((i = 0; i < ${#argv[@]}; i++)); do
  # Stop once we reach the argument delimiter. Anything after `--` is forwarded to rustc / the test
  # harness / the executed binary, and should not be interpreted as Cargo flags.
  if [[ "${argv[$i]}" == "--" ]]; then
    break
  fi

  case "${argv[$i]}" in
    -p|--package)
      if is_frame_pointer_pkg "${argv[$((i + 1))]:-}"; then
        needs_frame_pointers=1
      fi
      ;;
    -p=*)
      if is_frame_pointer_pkg "${argv[$i]#-p=}"; then
        needs_frame_pointers=1
      fi
      ;;
    --package=*)
      if is_frame_pointer_pkg "${argv[$i]#--package=}"; then
        needs_frame_pointers=1
      fi
      ;;
    --features|-F)
      features="${argv[$((i + 1))]:-}"
      features="${features//[[:space:]]/}"
      if [[ -n "${features}" && ",${features}," == *",allow_omit_frame_pointers,"* ]]; then
        allow_omit_frame_pointers=1
      fi
      ;;
    --features=*)
      features="${argv[$i]#--features=}"
      features="${features//[[:space:]]/}"
      if [[ -n "${features}" && ",${features}," == *",allow_omit_frame_pointers,"* ]]; then
        allow_omit_frame_pointers=1
      fi
      ;;
    -F*)
      # Cargo accepts `-F foo` and `-Ffoo` spellings.
      features="${argv[$i]#-F}"
      features="${features//[[:space:]]/}"
      if [[ -n "${features}" && ",${features}," == *",allow_omit_frame_pointers,"* ]]; then
        allow_omit_frame_pointers=1
      fi
      ;;
    --manifest-path)
      case "${argv[$((i + 1))]:-}" in
        *"runtime-native/Cargo.toml"|*"native-js/Cargo.toml"|*"native-js-cli/Cargo.toml")
          needs_frame_pointers=1
          ;;
      esac
      ;;
    --manifest-path=*)
      case "${argv[$i]#--manifest-path=}" in
        *"runtime-native/Cargo.toml"|*"native-js/Cargo.toml"|*"native-js-cli/Cargo.toml")
          needs_frame_pointers=1
          ;;
      esac
      ;;
    --workspace)
      # `vendor/ecma-rs`'s full workspace includes `runtime-native`, whose build script enforces frame
      # pointers.
      if [[ "${cargo_workdir}" == "${repo_root}/vendor/ecma-rs" ]]; then
        needs_frame_pointers=1
      fi
      ;;
  esac
done

if [[ "${needs_frame_pointers}" -eq 1 && "${allow_omit_frame_pointers}" -eq 0 ]]; then
  # rustc uses "last flag wins". If the user/CI environment sets
  # `-C force-frame-pointers=no` (or similar) anywhere in `RUSTFLAGS`, we must
  # append a final `=yes` so `runtime-native`'s FP-chain stack walking contract
  # holds.
  need_fp=0
  if [[ "${RUSTFLAGS:-}" != *"force-frame-pointers=yes"* ]]; then
    need_fp=1
  fi
  if [[ "${RUSTFLAGS:-}" == *"force-frame-pointers=no"* ]]; then
    need_fp=1
  fi
  if [[ "${RUSTFLAGS:-}" == *"force-frame-pointers=false"* ]]; then
    need_fp=1
  fi
  if [[ "${RUSTFLAGS:-}" == *"force-frame-pointers=off"* ]]; then
    need_fp=1
  fi
  if [[ "${RUSTFLAGS:-}" == *"force-frame-pointers=non-leaf"* ]]; then
    need_fp=1
  fi
  if [[ "${need_fp}" -ne 0 ]]; then
    if [[ -z "${RUSTFLAGS:-}" ]]; then
      export RUSTFLAGS="-C force-frame-pointers=yes"
    else
      export RUSTFLAGS="${RUSTFLAGS} -C force-frame-pointers=yes"
    fi
  fi
fi

# `libaom-sys` (used for AVIF decoding) requires an assembler (yasm/nasm) for
# optimized x86_64 builds. Our CI/agent environments don't necessarily have
# those tools installed, which would make *any* `cargo build/test` fail.
#
# Use a tiny CMake toolchain file to force the portable (non-asm) libaom build
# by default. Allow callers to override by setting `CMAKE_TOOLCHAIN_FILE`.
if [[ -z "${CMAKE_TOOLCHAIN_FILE:-}" ]]; then
  export CMAKE_TOOLCHAIN_FILE="${repo_root}/.cargo/aom_generic_toolchain.cmake"
fi

# `vendor/ecma-rs` is a nested workspace with its own `.cargo/config.toml`.
# When invoking Cargo from the repo root with `--manifest-path vendor/ecma-rs/Cargo.toml`,
# Cargo does *not* load that nested config, so `env` overrides like `RUSTC_BOOTSTRAP=1`
# would otherwise be lost.
#
# `runtime-native` relies on this (uses `#![feature(thread_local)]`), so propagate the
# nested workspace's bootstrap opt-in when we detect a `vendor/ecma-rs/Cargo.toml`
# manifest path.
needs_rustc_bootstrap=0
argv=("$@")
for ((i = 0; i < ${#argv[@]}; i++)); do
  if [[ "${argv[$i]}" == "--" ]]; then
    break
  fi
  manifest_path=""
  if [[ "${argv[$i]}" == "--manifest-path" ]]; then
    manifest_path="${argv[$((i + 1))]:-}"
  elif [[ "${argv[$i]}" == --manifest-path=* ]]; then
    manifest_path="${argv[$i]#--manifest-path=}"
  fi

  if [[ -n "${manifest_path}" ]]; then
    case "${manifest_path}" in
      "${repo_root}/vendor/ecma-rs/Cargo.toml"|"vendor/ecma-rs/Cargo.toml"|*/vendor/ecma-rs/Cargo.toml)
        needs_rustc_bootstrap=1
        ;;
    esac
  fi
done

if [[ "${needs_rustc_bootstrap}" == "1" && -z "${RUSTC_BOOTSTRAP:-}" ]]; then
  export RUSTC_BOOTSTRAP=1
fi

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
if [[ -n "${nproc}" ]]; then
  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    echo "invalid FASTR_CARGO_NPROC: ${nproc}" >&2
    exit 2
  fi
else
  if command -v nproc >/dev/null 2>&1; then
    nproc="$(nproc 2>/dev/null || true)"
  fi

  # Fall back to `getconf` when `nproc` is missing (macOS, some CI images). Be defensive:
  # some `getconf` builds print "undefined" while still exiting 0.
  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    nproc="$(getconf _NPROCESSORS_ONLN 2>/dev/null || true)"
  fi

  # macOS fallback (when `_NPROCESSORS_ONLN` is unavailable).
  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    if command -v sysctl >/dev/null 2>&1; then
      nproc="$(sysctl -n hw.logicalcpu 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || true)"
    fi
  fi

  # Windows runners commonly expose processor count via NUMBER_OF_PROCESSORS.
  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    nproc="${NUMBER_OF_PROCESSORS:-1}"
  fi

  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    nproc=1
  fi
fi

# Default slots: keep concurrency low by default for multi-agent hosts.
slots="${FASTR_CARGO_SLOTS:-}"
if [[ "${slots}" == "auto" ]]; then
  slots=""
fi
if [[ -n "${slots}" ]]; then
  if ! [[ "${slots}" =~ ^[0-9]+$ ]] || [[ "${slots}" -lt 1 ]]; then
    echo "invalid FASTR_CARGO_SLOTS: ${slots}" >&2
    exit 2
  fi
else
  # Avoid cargo stampedes: allow a few concurrent cargo commands, but not "one per agent".
  # Heuristic: ~1 concurrent cargo per 48 hw threads (clamped).
  slots=$(( nproc / 48 ))
  if [[ "${slots}" -lt 1 ]]; then slots=1; fi
  if [[ "${slots}" -gt 8 ]]; then slots=8; fi
fi

jobs="${FASTR_CARGO_JOBS:-}"
jobs_source="explicit"
if [[ -n "${jobs}" ]]; then
  if ! [[ "${jobs}" =~ ^[0-9]+$ ]] || [[ "${jobs}" -lt 1 ]]; then
    echo "invalid FASTR_CARGO_JOBS: ${jobs}" >&2
    exit 2
  fi
else
  # Default jobs: distribute compilation across the configured slot count so the *aggregate*
  # rustc parallelism across many agents stays close to the host's CPU count.
  #
  # Example: on a 192-thread host with the default 4 slots, each cargo command gets `-j 48`.
  jobs_source="auto"
  jobs=$(( nproc / slots ))
  if [[ "${jobs}" -lt 1 ]]; then jobs=1; fi
fi
jobs_label="${jobs}"
if [[ "${jobs_source}" == "auto" ]]; then
  jobs_label="${jobs} (auto)"
fi

limit_as_defaulted=0
if [[ -z "${FASTR_CARGO_LIMIT_AS:-}" && -z "${LIMIT_AS:-}" ]]; then
  limit_as="64G"
  limit_as_defaulted=1
else
  limit_as="${FASTR_CARGO_LIMIT_AS:-${LIMIT_AS:-64G}}"
fi

# Running the workspace `xtask` package is a special case: several xtask commands (page-loop, fixture-chrome-diff,
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
  # The `xtask` subcommand is a Cargo alias that runs the workspace `xtask` package (see
  # `.cargo/config.toml`). Treat both spellings (`xtask` and `run -p xtask`) as "xtask runs" for
  # address-space purposes.
  if [[ "${subcmd}" == "xtask" ]]; then
    limit_as="${FASTR_XTASK_LIMIT_AS:-96G}"
  elif [[ "${subcmd}" == "fuzz" ]]; then
    # `cargo-fuzz` defaults to AddressSanitizer, which reserves a very large amount of virtual
    # address space for shadow memory (~15TiB on x86_64). The default RLIMIT_AS (64G) prevents the
    # fuzzer from starting, so bump the default cap for fuzz runs unless the caller overrides it.
    limit_as="${FASTR_FUZZ_LIMIT_AS:-32T}"
  elif [[ "${subcmd}" == "run" ]]; then
    for ((i = subcmd_pos + 1; i < ${#argv[@]}; i++)); do
      if [[ "${argv[$i]}" == "--" ]]; then
        break
      fi
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

# Prefer a shared lock dir when running on multi-agent hosts (e.g. Grind swarm) so independent
# containers coordinate and we avoid host-wide cargo stampedes. Fall back to a per-user cache dir
# for normal local development.
default_lock_dir=""
if [[ -d "/state/root" && -w "/state/root" ]]; then
  default_lock_dir="/state/root/.cache/fastrender/cargo_agent_locks"
elif [[ -n "${XDG_CACHE_HOME:-}" ]]; then
  default_lock_dir="${XDG_CACHE_HOME}/fastrender/cargo_agent_locks"
elif [[ -n "${HOME:-}" ]]; then
  default_lock_dir="${HOME}/.cache/fastrender/cargo_agent_locks"
else
  default_lock_dir="${repo_root}/target/.cargo_agent_locks"
fi

lock_dir="${FASTR_CARGO_LOCK_DIR:-${default_lock_dir}}"
if ! mkdir -p "${lock_dir}" 2>/dev/null; then
  # Be defensive: if our default choice isn't writable, fall back to a repo-local lock dir so the
  # wrapper still works instead of failing before Cargo runs.
  lock_dir="${repo_root}/target/.cargo_agent_locks"
  mkdir -p "${lock_dir}"
fi

run_cargo() {
  local cargo_cmd=(cargo)
  local toolchain_arg=""
  local subcommand=""
  local workdir="${cargo_workdir}"
  local manifest_path=""

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
    toolchain_arg="$1"
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

  # IMPORTANT: `cargo xtask ...` is a cargo alias for `cargo run -p xtask -- ...` (see
  # `.cargo/config.toml`). `cargo run` holds the target-dir lock for the entire duration of the
  # process, including while the xtask binary is running.
  #
  # Several xtask subcommands (notably `page-loop`) spawn additional cargo commands via this same
  # wrapper. If we run xtask via `cargo run`, those nested cargo commands will deadlock waiting on
  # Cargo's target-dir lock.
  #
  # To avoid that, build the xtask binary first and then execute it directly (still under the same
  # RLIMIT_AS cap). This keeps the wrapper compatible with `scripts/cargo_agent.sh xtask ...` while
  # allowing xtask to run nested cargo commands.
  if [[ "${subcommand}" == "xtask" ]]; then
    workdir="${repo_root}"
    local build_cmd=(cargo)
    if [[ -n "${toolchain_arg}" ]]; then
      build_cmd+=("${toolchain_arg}")
    fi
    build_cmd+=(build -p xtask --bin xtask)
    if [[ -n "${jobs}" ]]; then
      build_cmd+=(-j "${jobs}")
    fi
    local build_status=0
    if [[ -z "${limit_as}" || "${limit_as}" == "0" || "${limit_as}" == "off" ]]; then
      (cd "${workdir}" && "${build_cmd[@]}") || build_status=$?
    else
      (cd "${workdir}" && bash "${repo_root}/scripts/run_limited.sh" --as "${limit_as}" -- "${build_cmd[@]}") || build_status=$?
    fi
    if [[ "${build_status}" -ne 0 ]]; then
      return "${build_status}"
    fi

    local target_dir="${CARGO_TARGET_DIR:-}"
    if [[ -z "${target_dir}" ]]; then
      target_dir="${repo_root}/target"
    elif [[ "${target_dir}" != /* ]]; then
      target_dir="${repo_root}/${target_dir}"
    fi
    local exe_suffix=""
    case "${OSTYPE:-}" in
      msys*|cygwin*|win32*) exe_suffix=".exe" ;;
    esac
    local xtask_bin="${target_dir}/debug/xtask${exe_suffix}"
    if [[ ! -f "${xtask_bin}" ]]; then
      echo "xtask binary not found at ${xtask_bin}" >&2
      return 1
    fi

    if [[ -z "${limit_as}" || "${limit_as}" == "0" || "${limit_as}" == "off" ]]; then
      (cd "${workdir}" && "${xtask_bin}" "$@")
    else
      (cd "${workdir}" && bash "${repo_root}/scripts/run_limited.sh" --as "${limit_as}" -- "${xtask_bin}" "$@")
    fi
    return $?
  fi

  if [[ "${subcommand}" == "test" && -z "${RUST_TEST_THREADS:-}" ]]; then
    local rust_test_threads="${FASTR_RUST_TEST_THREADS:-}"
    if [[ -z "${rust_test_threads}" ]]; then
      rust_test_threads=$(( nproc < 32 ? nproc : 32 ))
    else
      if ! [[ "${rust_test_threads}" =~ ^[0-9]+$ ]] || [[ "${rust_test_threads}" -lt 1 ]]; then
        echo "invalid FASTR_RUST_TEST_THREADS: ${rust_test_threads}" >&2
        return 2
      fi
    fi
    export RUST_TEST_THREADS="${rust_test_threads}"
  fi

  # Only inject `-j` for subcommands that actually accept compilation job settings.
  #
  # Examples of cargo commands that *do not* accept `-j`:
  # - `cargo metadata`
  # - `cargo --version`
  #
  # Keep this list conservative; passing `-j` to an unsupported subcommand fails fast.
  subcommand_supports_jobs() {
    case "$1" in
      build|check|clippy|doc|run|rustc|test|bench|install)
        return 0
        ;;
      *)
        return 1
        ;;
    esac
  }

  if [[ -n "${jobs}" ]] && subcommand_supports_jobs "${subcommand}"; then
    cargo_cmd+=(-j "${jobs}")
  fi

  cargo_cmd+=("$@")

  # `vendor/ecma-rs` is a nested workspace that relies on `RUSTC_BOOTSTRAP=1` to
  # compile a small amount of runtime code that still uses unstable feature
  # gates (see `vendor/ecma-rs/.cargo/config.toml`). Since this wrapper supports
  # invoking that workspace from the repo root via `--manifest-path`, ensure the
  # env var is set even when Cargo doesn't discover the nested `.cargo` config.
  for ((i = 0; i < ${#cargo_cmd[@]}; i++)); do
    if [[ "${cargo_cmd[$i]}" == "--" ]]; then
      break
    fi
    if [[ "${cargo_cmd[$i]}" == "--manifest-path" ]]; then
      manifest_path="${cargo_cmd[$((i + 1))]:-}"
      break
    elif [[ "${cargo_cmd[$i]}" == --manifest-path=* ]]; then
      manifest_path="${cargo_cmd[$i]#--manifest-path=}"
      break
    fi
  done
  if [[ -n "${manifest_path}" && ( "${manifest_path}" == "vendor/ecma-rs/Cargo.toml" || "${manifest_path}" == */vendor/ecma-rs/Cargo.toml ) ]]; then
    export RUSTC_BOOTSTRAP="1"
    # `runtime-native` (and any code it links against) relies on frame pointers
    # for precise stack walking / GC root enumeration. The nested workspace
    # provides an LLVM wrapper (`vendor/ecma-rs/scripts/cargo_llvm.sh`) that sets
    # this flag automatically, but when we invoke the workspace from the repo
    # root via `--manifest-path`, we need to inject it here.
    #
    # Respect the escape hatch: if the caller explicitly enables
    # `runtime-native`'s `allow_omit_frame_pointers` feature, do not force-inject
    # frame pointers here.
    if [[ "${allow_omit_frame_pointers:-0}" -eq 0 ]]; then
      local need_fp=0
      if [[ "${RUSTFLAGS:-}" != *"force-frame-pointers=yes"* ]]; then
        need_fp=1
      fi
      if [[ "${RUSTFLAGS:-}" == *"force-frame-pointers=no"* ]]; then
        need_fp=1
      fi
      if [[ "${RUSTFLAGS:-}" == *"force-frame-pointers=false"* ]]; then
        need_fp=1
      fi
      if [[ "${RUSTFLAGS:-}" == *"force-frame-pointers=off"* ]]; then
        need_fp=1
      fi
      if [[ "${RUSTFLAGS:-}" == *"force-frame-pointers=non-leaf"* ]]; then
        need_fp=1
      fi
      if [[ "${need_fp}" -ne 0 ]]; then
        if [[ -z "${RUSTFLAGS:-}" ]]; then
          export RUSTFLAGS="-C force-frame-pointers=yes"
        else
          export RUSTFLAGS="${RUSTFLAGS} -C force-frame-pointers=yes"
        fi
      fi
    fi
  fi

  if [[ -z "${limit_as}" || "${limit_as}" == "0" || "${limit_as}" == "off" ]]; then
    (cd "${workdir}" && "${cargo_cmd[@]}")
    return $?
  fi

  # Invoke through `bash`:
  # - Some agent/CI environments mount repos with `noexec`, which prevents executing scripts directly.
  # - Some environments check out scripts without the executable bit (e.g. CI artifact tars).
  (cd "${workdir}" && bash "${repo_root}/scripts/run_limited.sh" --as "${limit_as}" -- "${cargo_cmd[@]}")
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
  nested_as_label="${limit_as}"
  # Nested invocations inherit the parent's RLIMIT_AS already (e.g. when `scripts/cargo_agent.sh xtask`
  # runs the xtask binary under `scripts/run_limited.sh`). Re-applying the wrapper's default `--as`
  # limit here would *narrow* that inherited ceiling (e.g. 96G → 64G) and can cause unexpected OOMs.
  #
  # If the caller explicitly requested a limit via `FASTR_CARGO_LIMIT_AS`/`LIMIT_AS`, honor it even in
  # nested mode. Otherwise, preserve any existing RLIMIT_AS.
  if [[ "${limit_as_defaulted}" -eq 1 ]]; then
    current_as_kib="$(ulimit -v 2>/dev/null || echo "")"
    if [[ -n "${current_as_kib}" && "${current_as_kib}" != "unlimited" && "${current_as_kib}" != "0" ]]; then
      # Preserve the inherited RLIMIT_AS instead of resetting to our default. Use the `K` suffix to
      # express the limit in KiB because `scripts/run_limited.sh` treats bare numbers as MiB.
      limit_as="${current_as_kib}K"
      nested_as_label="inherit"
    fi
  fi
  echo "cargo_agent: nested slot=${FASTR_CARGO_SLOT} jobs=${jobs_label} as=${nested_as_label}" >&2
  run_cargo "$@"
  exit $?
fi

# Git Bash / MSYS / Cygwin note:
#
# The slot-throttling implementation relies on `flock` + inherited file descriptors. That works well
# on Linux, but is unreliable across Windows shell environments. Prefer correctness over
# parallelism: on Windows, disable slot throttling and run cargo directly.
uname_s="$(uname -s 2>/dev/null || echo "")"
case "${uname_s}" in
  MINGW*|MSYS*|CYGWIN*)
    echo "warning: Windows shell detected; running cargo without slot throttling" >&2
    run_cargo "$@"
    exit $?
    ;;
esac

if ! command -v flock >/dev/null 2>&1; then
  echo "warning: flock not available; running cargo without slot throttling" >&2
  run_cargo "$@"
  exit $?
fi

# Some non-Linux environments ship a `flock` binary that doesn't support locking inherited file
# descriptors. Probe it once so we don't deadlock in the retry loop below.
#
# IMPORTANT: `exec` redirections persist for the rest of the shell process. Do **not** attach
# `2>/dev/null` directly to `exec` here, or we'd permanently silence stderr for the remainder of this
# script (including cargo diagnostics).
exec 198>&2
exec 2>/dev/null
if ! exec 199>"${lock_dir}/.flock_probe.lock"; then
  exec 2>&198
  exec 198>&-
  echo "warning: unable to open flock probe lock; running cargo without slot throttling" >&2
  run_cargo "$@"
  exit $?
fi
exec 2>&198
exec 198>&-
if ! flock -n 199 >/dev/null 2>&1; then
  echo "warning: flock appears unusable; running cargo without slot throttling" >&2
  exec 199>&- || true
  run_cargo "$@"
  exit $?
fi
exec 199>&-

acquire_slot() {
  local i k start lockfile fd
  # Avoid hot-spotting slot 0 (and reduce starvation risk) by picking a rotating start index.
  start=$(( ($$ + RANDOM) % slots ))
  for ((k = 0; k < slots; k++)); do
    i=$(( (start + k) % slots ))
    lockfile="${lock_dir}/slot.${i}.lock"
    # Use a fixed fd number per slot (Bash 3.x compatible; avoids `exec {var}>...`).
    fd=$((200 + i))
    eval "exec ${fd}>\"${lockfile}\"" || continue
    if flock -n "${fd}"; then
      echo "${fd}:${i}"
      return 0
    fi
    eval "exec ${fd}>&-" || true
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

echo "cargo_agent: slot=${slot_idx}/${slots} jobs=${jobs_label} as=${limit_as}" >&2

set +e
run_cargo "$@"
status=$?
set -e

# Release lock.
eval "exec ${slot_fd}>&-" || true
exit "${status}"
