#!/usr/bin/env bash
set -euo pipefail

# Regenerate the `.llvm_stackmaps` fixtures in this directory.
#
# These fixtures are extracted from a *linked* ELF (so function addresses are resolved to
# non-zero values), which is useful for testing callsite-PC mapping without needing to model
# relocations.
#
# Requirements:
# - LLVM 18 toolchain on PATH (Ubuntu package names: opt-18, llc-18, llvm-objcopy-18)
# - clang-18 + lld-18 for linking the temporary executable
#
# This script intentionally *overwrites* the committed `.stackmaps.bin` files. Use `git diff`
# afterwards to verify that regeneration is reproducible.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

need_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: missing required tool: $1" >&2
    exit 1
  fi
}

need_tool opt-18
need_tool llc-18
need_tool clang-18
need_tool llvm-objcopy-18

TMP="$(mktemp -d)"
cleanup() { rm -rf "${TMP}"; }
trap cleanup EXIT

TRIPLE="x86_64-unknown-linux-gnu"
CPU="x86-64"

# A tiny entrypoint so we can link an executable (resolving stackmap function addresses).
cat >"${TMP}/main.c" <<'EOF'
void callee(void) {}
int main(void) { return 0; }
EOF
clang-18 -O0 -c "${TMP}/main.c" -o "${TMP}/main.o"

gen_linked_stackmaps() {
  local name="$1"

  opt-18 -mtriple="${TRIPLE}" -passes=rewrite-statepoints-for-gc -S \
    "${SCRIPT_DIR}/${name}.ll" \
    -o "${TMP}/${name}.rewritten.ll"

  llc-18 -O0 -filetype=obj \
    --fixup-allow-gcptr-in-csr=false --fixup-max-csr-statepoints=0 \
    -mtriple="${TRIPLE}" -mcpu="${CPU}" \
    "${TMP}/${name}.rewritten.ll" \
    -o "${TMP}/${name}.o"

  # `-no-pie` keeps the linked addresses stable (ET_EXEC) on distros that default to PIE.
  clang-18 -O0 -fuse-ld=lld-18 -no-pie \
    "${TMP}/main.o" \
    "${TMP}/${name}.o" \
    -o "${TMP}/${name}.elf"

  llvm-objcopy-18 --dump-section ".llvm_stackmaps=${SCRIPT_DIR}/${name}.stackmaps.bin" \
    "${TMP}/${name}.elf"
}

gen_linked_stackmaps two_statepoints
gen_linked_stackmaps two_funcs

echo "ok: regenerated linked stackmap fixtures into ${SCRIPT_DIR}"
