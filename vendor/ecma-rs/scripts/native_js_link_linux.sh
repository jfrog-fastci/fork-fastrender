#!/usr/bin/env bash
set -euo pipefail

# Native JS AOT linker (Linux / ELF).
#
# Why this exists:
# - LLVM emits GC stack maps into a `.llvm_stackmaps` section.
# - The section contains absolute code addresses, producing `R_X86_64_64` relocations.
# - Linking into a PIE with lld fails by default if those relocations live in a read-only section.
#
# Our default policy is PIE *without* text relocations:
# - Rewrite `.llvm_stackmaps` to be writable in the object file (so relocations are applied to RW
#   memory), using `llvm-objcopy-18`.
# - Link with a tiny linker-script fragment that `KEEP`s the section (so `--gc-sections` can't drop
#   it) and defines `__llvm_stackmaps_start` / `__llvm_stackmaps_end` (see
#   `runtime-native/stackmaps.ld`).
#
# See: ../docs/gc_statepoints.md

usage() {
  cat >&2 <<'EOF'
Usage:
  vendor/ecma-rs/scripts/native_js_link_linux.sh --out <output> [--no-gc-sections] -- <obj>...

Environment:
  NATIVE_JS_CLANG        (default: clang-18)
  NATIVE_JS_OBJCOPY      (default: llvm-objcopy-18)

Notes:
  - This script is Linux-only.
  - The output is a PIE executable (passes -pie explicitly).
EOF
}

if [[ "${OSTYPE:-}" != linux* ]]; then
  echo "error: native_js_link_linux.sh is Linux-only (OSTYPE=${OSTYPE:-unknown})" >&2
  exit 2
fi

out=""
gc_sections=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    -o|--out)
      out="${2:-}"
      shift 2
      ;;
    --out=*)
      out="${1#--out=}"
      shift
      ;;
    --no-gc-sections)
      gc_sections=0
      shift
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "error: unknown option '$1'" >&2
      usage
      exit 2
      ;;
    *)
      break
      ;;
  esac
done

if [[ -z "${out}" || $# -eq 0 ]]; then
  usage
  exit 2
fi

clang="${NATIVE_JS_CLANG:-clang-18}"
objcopy="${NATIVE_JS_OBJCOPY:-llvm-objcopy-18}"

if ! command -v "${clang}" >/dev/null 2>&1; then
  echo "error: missing ${clang} (install clang-18)" >&2
  exit 2
fi
if ! command -v "${objcopy}" >/dev/null 2>&1; then
  echo "error: missing ${objcopy} (install llvm-18)" >&2
  exit 2
fi
if ! command -v readelf >/dev/null 2>&1; then
  echo "error: missing readelf (install binutils)" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ecma_rs_root="$(cd "${script_dir}/.." && pwd)"
stackmaps_ld="${ecma_rs_root}/runtime-native/stackmaps.ld"
if [[ ! -f "${stackmaps_ld}" ]]; then
  echo "error: missing linker script ${stackmaps_ld}" >&2
  exit 2
fi

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "${tmpdir}"
}
trap cleanup EXIT

patched_objs=()
i=0
for obj in "$@"; do
  if [[ ! -f "${obj}" ]]; then
    echo "error: input object not found: ${obj}" >&2
    exit 2
  fi

  i=$((i + 1))
  patched="${tmpdir}/${i}-$(basename "${obj}")"
  cp "${obj}" "${patched}"

  # If present, make `.llvm_stackmaps` writable to avoid PIE text relocations with lld.
  if readelf -S "${patched}" 2>/dev/null | grep -q '\.llvm_stackmaps'; then
    "${objcopy}" --set-section-flags \
      .llvm_stackmaps=alloc,load,contents,data \
      "${patched}"
  fi

  patched_objs+=("${patched}")
done

link_args=(
  "${clang}"
  -fuse-ld=lld
  -pie
  -o "${out}"
  "-Wl,--script=${stackmaps_ld}"
)
if [[ "${gc_sections}" -eq 1 ]]; then
  link_args+=("-Wl,--gc-sections")
fi
link_args+=("${patched_objs[@]}")

"${link_args[@]}"
