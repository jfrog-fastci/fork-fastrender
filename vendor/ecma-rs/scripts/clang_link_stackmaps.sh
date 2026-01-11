#!/usr/bin/env bash
set -euo pipefail

# Work around `.llvm_stackmaps` + PIE `DT_TEXTREL` on ELF x86_64.
#
# LLVM stack maps contain absolute function addresses. In a PIE, those need runtime relocations.
# If `.llvm_stackmaps` is placed in a read-only segment (default with GNU ld), the link will emit
# `DT_TEXTREL` and print warnings about relocations in read-only `.llvm_stackmaps`.
#
# Fix: mark `.llvm_stackmaps` as writable in each input `.o` before linking so the linker puts the
# section in a RW segment and the dynamic loader can apply relocations without textrels.
#
# Docs: `vendor/ecma-rs/docs/llvm_stackmaps_linking.md`
#
# Usage:
#   bash vendor/ecma-rs/scripts/clang_link_stackmaps.sh [clang args...]
#
# Environment:
#   CLANG        clang binary to exec (default: clang)
#   LLVM_OBJCOPY llvm-objcopy binary (default: llvm-objcopy; falls back to objcopy)
#
# Note: This modifies input object files in-place. It is intended to be used on temporary build
# artifacts produced by the native codegen pipeline.

clang_bin="${CLANG:-clang}"
objcopy_bin="${LLVM_OBJCOPY:-llvm-objcopy}"

if ! command -v "${objcopy_bin}" >/dev/null 2>&1; then
  objcopy_bin="objcopy"
fi

objs=()
for arg in "$@"; do
  # Only touch existing files; this skips `-o out.o` during compilation.
  if [[ -f "${arg}" ]] && ([[ "${arg}" == *.o ]] || [[ "${arg}" == *.obj ]]); then
    objs+=("${arg}")
  fi
done

if [[ "${#objs[@]}" -ne 0 ]]; then
  if [[ "${objcopy_bin}" == *llvm-objcopy* ]]; then
    for obj in "${objs[@]}"; do
      "${objcopy_bin}" --set-section-flags=.llvm_stackmaps=alloc,contents,load,data "${obj}"
    done
  else
    for obj in "${objs[@]}"; do
      "${objcopy_bin}" --set-section-flags .llvm_stackmaps=alloc,contents,load,data "${obj}"
    done
  fi
fi

exec "${clang_bin}" "$@"

