#!/usr/bin/env bash
set -euo pipefail

# CMake wrapper used for CI/agent builds.
#
# Motivation: `libaom` (used for AVIF decoding via `avif-decode`/`libaom-sys`) requires `yasm`/`nasm`
# to build x86_64 assembly optimizations. Our execution environments do not guarantee those tools
# are installed, so we force libaom to use the portable (no-asm) configuration by injecting
# `-DAOM_TARGET_CPU=generic` into the initial CMake configure step.
#
# We use a wrapper because the upstream `libaom-sys` build script does not currently expose a
# reliable environment-variable hook for setting this CMake define.

real_cmake="${FASTR_REAL_CMAKE:-cmake}"

is_configure=true
is_libaom=false
has_target_cpu=false

for arg in "$@"; do
  case "${arg}" in
    --build|--install|-E)
      is_configure=false
      ;;
    -DAOM_TARGET_CPU=*)
      has_target_cpu=true
      ;;
  esac

  # The libaom-sys crate builds from a vendored `libaom` source directory named `vendor`.
  # Match conservatively so other CMake projects are unaffected.
  if [[ "${arg}" == *"libaom-sys"* ]]; then
    is_libaom=true
  fi
done

if [[ "${is_configure}" == "true" && "${is_libaom}" == "true" && "${has_target_cpu}" == "false" ]]; then
  aom_target_cpu="${AOM_TARGET_CPU:-generic}"
  exec "${real_cmake}" "$@" "-DAOM_TARGET_CPU=${aom_target_cpu}"
fi

exec "${real_cmake}" "$@"
