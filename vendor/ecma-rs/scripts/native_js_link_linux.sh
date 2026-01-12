#!/usr/bin/env bash
set -euo pipefail

# Native JS AOT linker (Linux / ELF).
#
# Why this exists:
# - LLVM emits GC stack maps into a `.llvm_stackmaps` section.
# - The section contains absolute code addresses, producing `R_X86_64_64` relocations.
# - Linking into a PIE with lld fails by default if those relocations live in a read-only section.
#
# Our recommended PIE policy is PIE *without* text relocations:
# - Rewrite `.llvm_stackmaps` into `.data.rel.ro.llvm_stackmaps` in the object file so relocations
#   are applied to RW memory and the final bytes can be protected by RELRO.
# - Link with a tiny linker-script fragment that `KEEP`s the section (so `--gc-sections` can't drop
#   it) and defines `__fastr_stackmaps_start` / `__fastr_stackmaps_end` (see
#   `runtime-native/link/stackmaps.ld`, with `runtime-native/stackmaps.ld` as a compat alias).
#
# See: ../docs/gc_statepoints.md

usage() {
  cat >&2 <<'EOF'
Usage:
  vendor/ecma-rs/scripts/native_js_link_linux.sh --out <output> [--no-gc-sections] -- <obj>...

Environment:
  NATIVE_JS_CLANG        (default: clang-18/clang)
  NATIVE_JS_OBJCOPY      (default: llvm-objcopy-18/llvm-objcopy)

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

pick_cmd() {
  for c in "$@"; do
    if command -v "${c}" >/dev/null 2>&1; then
      echo "${c}"
      return 0
    fi
  done
  return 1
}

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

clang="${NATIVE_JS_CLANG:-}"
if [[ -z "${clang}" ]]; then
  if ! clang="$(pick_cmd clang-18 clang)"; then
    echo "error: missing clang (expected clang-18 or clang in PATH)" >&2
    exit 2
  fi
fi
objcopy="${NATIVE_JS_OBJCOPY:-}"
if [[ -z "${objcopy}" ]]; then
  if ! objcopy="$(pick_cmd llvm-objcopy-18 llvm-objcopy)"; then
    echo "error: missing llvm-objcopy (expected llvm-objcopy-18 or llvm-objcopy in PATH)" >&2
    exit 2
  fi
fi

lld_fuse=""
if command -v ld.lld-18 >/dev/null 2>&1; then
  lld_fuse="lld-18"
elif command -v ld.lld >/dev/null 2>&1; then
  lld_fuse="lld"
else
  echo "error: missing lld (expected ld.lld-18 or ld.lld in PATH; install lld-18)" >&2
  exit 2
fi

if ! command -v "${clang}" >/dev/null 2>&1; then
  echo "error: missing ${clang} (expected clang-18 or clang in PATH)" >&2
  exit 2
fi
if ! command -v "${objcopy}" >/dev/null 2>&1; then
  echo "error: missing ${objcopy} (expected llvm-objcopy-18 or llvm-objcopy in PATH)" >&2
  exit 2
fi
if ! command -v readelf >/dev/null 2>&1; then
  echo "error: missing readelf (install binutils)" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ecma_rs_root="$(cd "${script_dir}/.." && pwd)"
stackmaps_ld="${ecma_rs_root}/runtime-native/link/stackmaps.ld"
if [[ ! -f "${stackmaps_ld}" ]]; then
  stackmaps_ld="${ecma_rs_root}/runtime-native/stackmaps.ld"
fi
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

  # If present, relocate `.llvm_stackmaps` into `.data.rel.ro.llvm_stackmaps` (RELRO-friendly).
  # Avoid `grep -q` under `set -o pipefail`: `readelf -S` output can be large
  # for debug objects, and early pipe closure can trigger SIGPIPE in the producer.
  if readelf -W -S "${patched}" 2>/dev/null | grep '\.data\.rel\.ro\.llvm_stackmaps' >/dev/null; then
    : # already rewritten
  elif readelf -W -S "${patched}" 2>/dev/null | grep '\.llvm_stackmaps' >/dev/null; then
    "${objcopy}" --rename-section \
      .llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents \
      "${patched}"
  fi

  # Same for `.llvm_faultmaps` if present (patchpoint metadata).
  if readelf -W -S "${patched}" 2>/dev/null | grep '\.data\.rel\.ro\.llvm_faultmaps' >/dev/null; then
    : # already rewritten
  elif readelf -W -S "${patched}" 2>/dev/null | grep '\.llvm_faultmaps' >/dev/null; then
    "${objcopy}" --rename-section \
      .llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents \
      "${patched}"
  fi

  patched_objs+=("${patched}")
done

link_args=(
  "${clang}"
  -fuse-ld="${lld_fuse}"
  -pie
  -o "${out}"
  "-Wl,--script=${stackmaps_ld}"
)
if [[ "${gc_sections}" -eq 1 ]]; then
  link_args+=("-Wl,--gc-sections")
fi
link_args+=("${patched_objs[@]}")

"${link_args[@]}"
