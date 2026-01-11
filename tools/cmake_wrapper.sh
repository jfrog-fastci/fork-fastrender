#!/usr/bin/env bash
set -euo pipefail

# CMake wrapper used for CI/agent builds.
#
# Motivation: `libaom` (used for AVIF decoding via `avif-decode`/`libaom-sys`) enables x86_64
# assembly optimizations by default and fails configuration when no assembler (yasm/nasm) is
# available.
#
# Our execution environments do not guarantee those tools are installed, so we force libaom into a
# portable, deterministic (no-asm) configuration by injecting:
# - `-DAOM_TARGET_CPU=generic`
# - plus additional flags that disable all assembler backends.
#
# We do this in a wrapper because the upstream `libaom-sys` build script doesn't reliably surface a
# knob for these CMake flags.

real_cmake="${FASTR_REAL_CMAKE:-cmake}"

# Resolve the wrapper's location to an absolute path without depending on the caller's cwd.
# - If the wrapper is invoked via PATH (e.g. `cmake_wrapper.sh`), `BASH_SOURCE[0]` contains no
#   directory component, so we use `command -v` to resolve it.
# - If it's invoked via a relative path (e.g. `tools/cmake_wrapper.sh`), we make it absolute.
script_path="${BASH_SOURCE[0]}"
if [[ "${script_path}" != /* ]]; then
  if [[ "${script_path}" != */* ]]; then
    resolved="$(command -v -- "${script_path}" 2>/dev/null || true)"
    if [[ -n "${resolved}" ]]; then
      script_path="${resolved}"
    fi
  fi
  script_path="$(cd -- "$(dirname -- "${script_path}")" && pwd -P)/$(basename -- "${script_path}")"
fi
script_dir="$(cd -P -- "$(dirname -- "${script_path}")" && pwd -P)"

aom_toolchain_file="${script_dir}/cmake/aom_target_cpu_generic.cmake"

is_configure=true
is_libaom=false
has_target_cpu=false
has_toolchain_file=false

for arg in "$@"; do
  case "${arg}" in
    --build|--install|-E|-P)
      is_configure=false
      ;;
    -DAOM_TARGET_CPU=*)
      has_target_cpu=true
      ;;
    -DCMAKE_TOOLCHAIN_FILE=*|-DCMAKE_TOOLCHAIN_FILE:*)
      has_toolchain_file=true
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

  # Only enforce the portable/no-asm configuration when building for the generic target. Power
  # users can opt out (and keep asm enabled) by setting `AOM_TARGET_CPU` to a non-`generic` value or
  # by passing `-DAOM_TARGET_CPU=...` explicitly.
  if [[ "${aom_target_cpu}" == "generic" ]]; then
    extra_args=(
      "-DAOM_TARGET_CPU=generic"
      "-DENABLE_ASM=0"
      "-DENABLE_NASM=0"
      "-DENABLE_YASM=0"
    )

    # Prefer passing a toolchain snippet via an absolute path derived from this wrapper's location
    # (rather than relying on the caller's cwd). If the caller already supplied a toolchain file,
    # don't override it; the explicit ENABLE_* flags above still guarantee a no-asm build.
    if [[ "${has_toolchain_file}" == "false" ]]; then
      extra_args+=("-DCMAKE_TOOLCHAIN_FILE=${aom_toolchain_file}")
    fi

    exec "${real_cmake}" "$@" "${extra_args[@]}"
  fi

  # Non-generic target: forward AOM_TARGET_CPU but do not force-disable asm.
  exec "${real_cmake}" "$@" "-DAOM_TARGET_CPU=${aom_target_cpu}"
fi

exec "${real_cmake}" "$@"
