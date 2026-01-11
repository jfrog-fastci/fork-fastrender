#!/usr/bin/env bash
set -euo pipefail

# Rename LLVM stackmap sections in object files to avoid DT_TEXTREL when linking
# PIE binaries / DSOs.
#
# Why:
# - `.llvm_stackmaps` is typically read-only but needs relocations (function addresses),
#   which can force the linker to emit DT_TEXTREL.
# - Moving the section under `.data.rel.ro.*` allows relocations to be applied in a
#   writable segment, then protected by RELRO.
#
# Usage:
#   bash vendor/ecma-rs/scripts/rename_llvm_stackmaps_section.sh file1.o file2.o ...
#
# Notes:
# - We keep this as a standalone helper so native-js (future) can call it for every
#   LLVM-generated object before the final link.

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <obj.o> [more objs...]" >&2
  exit 2
fi

if ! command -v llvm-objcopy-18 >/dev/null 2>&1; then
  echo "error: llvm-objcopy-18 not found in PATH" >&2
  exit 1
fi

if ! command -v llvm-readobj-18 >/dev/null 2>&1; then
  echo "error: llvm-readobj-18 not found in PATH" >&2
  exit 1
fi

for obj in "$@"; do
  if [[ ! -f "${obj}" ]]; then
    echo "error: object file not found: ${obj}" >&2
    exit 1
  fi

  # Skip if already renamed.
  if llvm-readobj-18 --sections "${obj}" | grep -q -- ".data.rel.ro.llvm_stackmaps"; then
    continue
  fi

  # Only rename if the legacy section exists.
  if llvm-readobj-18 --sections "${obj}" | grep -q -- ".llvm_stackmaps"; then
    # Important: set section flags to "data" (writable) so linkers that don't
    # automatically fold `.data.rel.ro.*` into `.data.rel.ro` still won't emit TEXTREL.
    llvm-objcopy-18 \
      --rename-section .llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents \
      "${obj}"
  fi
done
