#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  native_strip.sh <binary>

Strips a native binary while preserving runtime stack map sections.

Notes:
  - Do NOT use `llvm-strip --strip-sections`; it removes the section header
    table, which breaks section-name based discovery.
EOF
}

bin="${1:-}"
if [[ -z "${bin}" || "${bin}" == "-h" || "${bin}" == "--help" ]]; then
  usage
  exit 2
fi

if command -v llvm-strip >/dev/null 2>&1; then
  exec llvm-strip \
    --strip-all \
    --keep-section=.llvm_stackmaps \
    --keep-section=.llvm_stackmaps.* \
    --keep-section=.data.rel.ro.llvm_stackmaps \
    --keep-section=.data.rel.ro.llvm_stackmaps.* \
    --keep-section=llvm_stackmaps \
    --keep-section=llvm_stackmaps.* \
    --keep-section=.llvm_faultmaps \
    --keep-section=.llvm_faultmaps.* \
    --keep-section=.data.rel.ro.llvm_faultmaps \
    --keep-section=.data.rel.ro.llvm_faultmaps.* \
    "${bin}"
fi

# GNU strip doesn't support --keep-section, but it also doesn't remove SHF_ALLOC
# sections like `.llvm_stackmaps` under its common modes.
exec strip "${bin}"
