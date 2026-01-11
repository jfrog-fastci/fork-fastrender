#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  native_link.sh -o <output> <obj>...

Environment:
  ECMA_RS_NATIVE_CLANG    Override clang binary (default: clang-18/clang)
  ECMA_RS_NATIVE_LINKER   ld (default) | lld
  ECMA_RS_NATIVE_PIE      0 (default) | 1
  ECMA_RS_NATIVE_GC_SECTIONS
                          1 (default) | 0

Notes:
  - `.llvm_stackmaps` has no inbound references; `--gc-sections` will drop it
    unless using GNU ld with `keep_llvm_stackmaps.ld`.
  - lld currently cannot link PIE binaries containing `.llvm_stackmaps` because
    the section uses absolute relocations (see docs/native_stackmaps.md).
EOF
}

out=""
objs=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    -o)
      out="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      objs+=("$1")
      shift 1
      ;;
  esac
done

if [[ -z "${out}" || ${#objs[@]} -eq 0 ]]; then
  usage
  exit 2
fi

CLANG="${ECMA_RS_NATIVE_CLANG:-}"
if [[ -z "${CLANG}" ]]; then
  if command -v clang-18 >/dev/null 2>&1; then
    CLANG="clang-18"
  else
    CLANG="clang"
  fi
fi

LINKER="${ECMA_RS_NATIVE_LINKER:-ld}"
PIE="${ECMA_RS_NATIVE_PIE:-0}"
GC_SECTIONS="${ECMA_RS_NATIVE_GC_SECTIONS:-1}"

link_args=()

if [[ "${PIE}" == "1" ]]; then
  link_args+=("-pie")
else
  link_args+=("-no-pie")
fi

case "${LINKER}" in
  ld)
    ;;
  lld)
    # `clang -fuse-ld=lld` looks for `ld.lld` in PATH. Some distros only ship a
    # version-suffixed binary (`ld.lld-18`), so callers can provide their own
    # PATH entry if needed.
    link_args+=("-fuse-ld=lld")
    ;;
  *)
    echo "native_link.sh: unsupported ECMA_RS_NATIVE_LINKER=${LINKER} (expected ld|lld)" >&2
    exit 2
    ;;
esac

if [[ "${GC_SECTIONS}" == "1" ]]; then
  link_args+=("-Wl,--gc-sections")

  if [[ "${LINKER}" == "ld" ]]; then
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    link_args+=("-Wl,-T,${script_dir}/keep_llvm_stackmaps.ld")
  else
    echo "native_link.sh: refusing to use --gc-sections with lld; it will drop .llvm_stackmaps." >&2
    echo "Set ECMA_RS_NATIVE_GC_SECTIONS=0 or use ECMA_RS_NATIVE_LINKER=ld." >&2
    exit 2
  fi
fi

exec "${CLANG}" "${link_args[@]}" -o "${out}" "${objs[@]}"
