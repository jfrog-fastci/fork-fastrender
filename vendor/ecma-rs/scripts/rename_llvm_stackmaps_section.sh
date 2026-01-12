#!/usr/bin/env bash
set -euo pipefail

# Rename LLVM stackmap / faultmap sections in object files to avoid DT_TEXTREL
# when linking PIE binaries / DSOs.
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

pick_cmd() {
  for c in "$@"; do
    if command -v "${c}" >/dev/null 2>&1; then
      echo "${c}"
      return 0
    fi
  done
  return 1
}

if ! llvm_objcopy="$(pick_cmd llvm-objcopy-18 llvm-objcopy)"; then
  echo "error: llvm-objcopy not found in PATH (expected llvm-objcopy-18 or llvm-objcopy)" >&2
  exit 1
fi

if ! llvm_readobj="$(pick_cmd llvm-readobj-18 llvm-readobj)"; then
  echo "error: llvm-readobj not found in PATH (expected llvm-readobj-18 or llvm-readobj)" >&2
  exit 1
fi

for obj in "$@"; do
  if [[ ! -f "${obj}" ]]; then
    echo "error: object file not found: ${obj}" >&2
    exit 1
  fi

  # Skip if already renamed.
  #
  # Do NOT use `grep -q` under `set -o pipefail`: `llvm-readobj` can emit enough
  # output that `grep -q` exits early and triggers EPIPE/SIGPIPE in `llvm-readobj`,
  # making the pipeline return non-zero (flaky false negatives).
  if "${llvm_readobj}" --sections "${obj}" | grep -- ".data.rel.ro.llvm_stackmaps" >/dev/null; then
    : # already renamed
  elif "${llvm_readobj}" --sections "${obj}" | grep -- ".llvm_stackmaps" >/dev/null; then
    # Important: set section flags to "data" (writable) so linkers that don't
    # automatically fold `.data.rel.ro.*` into `.data.rel.ro` still won't emit TEXTREL.
    "${llvm_objcopy}" \
      --rename-section .llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents \
      "${obj}"
  fi

  # Same policy for `.llvm_faultmaps` when present.
  if "${llvm_readobj}" --sections "${obj}" | grep -- ".data.rel.ro.llvm_faultmaps" >/dev/null; then
    : # already renamed
  elif "${llvm_readobj}" --sections "${obj}" | grep -- ".llvm_faultmaps" >/dev/null; then
    "${llvm_objcopy}" \
      --rename-section .llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents \
      "${obj}"
  fi
done
