#!/usr/bin/env bash
set -euo pipefail

# Regenerate the `.llvm_stackmaps` binary fixtures in this directory.
#
# Requirements:
# - LLVM 18 toolchain on PATH (Ubuntu package names: opt-18, llc-18, llvm-objcopy-18)
#
# This script intentionally *overwrites* the committed `.stackmaps.bin` files. Use `git diff`
# afterwards to verify that regeneration is reproducible.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
IR_DIR="${SCRIPT_DIR}/ir"
OUT_DIR="${SCRIPT_DIR}"

need_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: missing required tool: $1" >&2
    exit 1
  fi
}

need_tool opt-18
need_tool llc-18
need_tool llvm-objcopy-18

TMP="$(mktemp -d)"
cleanup() { rm -rf "${TMP}"; }
trap cleanup EXIT

TRIPLE="x86_64-unknown-linux-gnu"
CPU="x86-64"

gen_stackmaps() {
  local name="$1"

  opt-18 -mtriple="${TRIPLE}" -passes=rewrite-statepoints-for-gc -S \
    "${IR_DIR}/${name}.ll" \
    -o "${TMP}/${name}.rewritten.ll"

  llc-18 -O0 -filetype=obj \
    -mtriple="${TRIPLE}" -mcpu="${CPU}" \
    "${TMP}/${name}.rewritten.ll" \
    -o "${TMP}/${name}.o"

  llvm-objcopy-18 --dump-section ".llvm_stackmaps=${OUT_DIR}/${name}.stackmaps.bin" \
    "${TMP}/${name}.o"
}

gen_stackmaps deopt_bundle2
gen_stackmaps deopt_var
gen_stackmaps transition_bundle
gen_stackmaps deopt_transition

echo "ok: regenerated stackmap fixtures into ${OUT_DIR}"
