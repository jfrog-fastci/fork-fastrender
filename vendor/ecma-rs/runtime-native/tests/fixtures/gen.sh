#!/usr/bin/env bash
set -euo pipefail

# Regenerate the `.llvm_stackmaps` binary fixtures under `tests/fixtures/`.
#
# Requirements:
# - LLVM 18 toolchain on PATH (Ubuntu package names: opt-18, llc-18, llvm-objcopy-18)
#
# This script intentionally *overwrites* the committed `.bin` files. Use `git diff`
# afterwards to verify that regeneration is reproducible.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
IR_DIR="${SCRIPT_DIR}/ir"
BIN_DIR="${SCRIPT_DIR}/bin"

need_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: missing required tool: $1" >&2
    exit 1
  fi
}

need_tool opt-18
need_tool llc-18
need_tool llvm-objcopy-18

mkdir -p "${BIN_DIR}"

TMP="$(mktemp -d)"
cleanup() { rm -rf "${TMP}"; }
trap cleanup EXIT

# stackmap_const (x86_64)
llc-18 -O0 -filetype=obj \
  -mtriple=x86_64-unknown-linux-gnu -mcpu=x86-64 \
  "${IR_DIR}/stackmap_const.ll" \
  -o "${TMP}/stackmap_const_x86_64.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${BIN_DIR}/stackmap_const_x86_64.bin" \
  "${TMP}/stackmap_const_x86_64.o"

# stackmap_direct (x86_64)
llc-18 -O0 -filetype=obj \
  -mtriple=x86_64-unknown-linux-gnu -mcpu=x86-64 \
  "${IR_DIR}/stackmap_direct.ll" \
  -o "${TMP}/stackmap_direct_x86_64.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${BIN_DIR}/stackmap_direct_x86_64.bin" \
  "${TMP}/stackmap_direct_x86_64.o"

# stackmap_register (x86_64)
llc-18 -O0 -filetype=obj \
  -mtriple=x86_64-unknown-linux-gnu -mcpu=x86-64 \
  "${IR_DIR}/stackmap_register.ll" \
  -o "${TMP}/stackmap_register_x86_64.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${BIN_DIR}/stackmap_register_x86_64.bin" \
  "${TMP}/stackmap_register_x86_64.o"

# statepoint_gcroot2 (rewrite-statepoints-for-gc + stackmaps)
opt-18 -mtriple=x86_64-unknown-linux-gnu -passes=rewrite-statepoints-for-gc -S \
  "${IR_DIR}/statepoint_gcroot2.ll" \
  -o "${TMP}/statepoint_x86_64_rewritten.ll"

llc-18 -O0 -filetype=obj \
  -mtriple=x86_64-unknown-linux-gnu -mcpu=x86-64 \
  "${TMP}/statepoint_x86_64_rewritten.ll" \
  -o "${TMP}/statepoint_x86_64.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${BIN_DIR}/statepoint_x86_64.bin" \
  "${TMP}/statepoint_x86_64.o"

opt-18 -mtriple=aarch64-unknown-linux-gnu -passes=rewrite-statepoints-for-gc -S \
  "${IR_DIR}/statepoint_gcroot2.ll" \
  -o "${TMP}/statepoint_aarch64_rewritten.ll"

llc-18 -O0 -filetype=obj \
  -mtriple=aarch64-unknown-linux-gnu -mcpu=generic \
  "${TMP}/statepoint_aarch64_rewritten.ll" \
  -o "${TMP}/statepoint_aarch64.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${BIN_DIR}/statepoint_aarch64.bin" \
  "${TMP}/statepoint_aarch64.o"

# statepoint_base_derived (explicit gc.statepoint + gc.relocate)
llc-18 -O0 -filetype=obj \
  -mtriple=x86_64-unknown-linux-gnu -mcpu=x86-64 \
  "${IR_DIR}/statepoint_gcroot_base_derived.ll" \
  -o "${TMP}/statepoint_base_derived_x86_64.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${BIN_DIR}/statepoint_base_derived_x86_64.bin" \
  "${TMP}/statepoint_base_derived_x86_64.o"

llc-18 -O0 -filetype=obj \
  -mtriple=aarch64-unknown-linux-gnu -mcpu=generic \
  "${IR_DIR}/statepoint_gcroot_base_derived.ll" \
  -o "${TMP}/statepoint_base_derived_aarch64.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${BIN_DIR}/statepoint_base_derived_aarch64.bin" \
  "${TMP}/statepoint_base_derived_aarch64.o"

# Additional fixtures (not in `bin/`):
# - `stackmaps_v3.bin`: one function containing multiple stackmap records that exercise all location kinds.
# - `patchpoint_liveouts.bin`: patchpoint stackmap with non-empty live-out list.

# stackmaps_v3 (x86_64)
llc-18 -O2 -filetype=obj \
  -mtriple=x86_64-unknown-linux-gnu -mcpu=x86-64 \
  "${SCRIPT_DIR}/stackmaps_v3.ll" \
  -o "${TMP}/stackmaps_v3.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${SCRIPT_DIR}/stackmaps_v3.bin" \
  "${TMP}/stackmaps_v3.o"

# patchpoint_liveouts (x86_64)
llc-18 -O0 -filetype=obj \
  -mtriple=x86_64-unknown-linux-gnu -mcpu=x86-64 \
  "${SCRIPT_DIR}/patchpoint_liveouts.ll" \
  -o "${TMP}/patchpoint_liveouts.o"

llvm-objcopy-18 --dump-section ".llvm_stackmaps=${SCRIPT_DIR}/patchpoint_liveouts.bin" \
  "${TMP}/patchpoint_liveouts.o"

echo "ok: regenerated stackmap fixtures into:"
echo "  - ${BIN_DIR}"
echo "  - ${SCRIPT_DIR}"
