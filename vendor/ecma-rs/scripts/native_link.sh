#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  native_link.sh -o <output> <obj>...

Environment:
  ECMA_RS_NATIVE_CLANG    Override clang binary (default: clang-18/clang)
  ECMA_RS_NATIVE_LINKER   lld (default if available) | ld
  ECMA_RS_NATIVE_PIE      0 (default) | 1
  ECMA_RS_NATIVE_GC_SECTIONS
                          1 (default) | 0

Notes:
  - `.llvm_stackmaps` has no inbound references; `--gc-sections` will drop it
    unless explicitly `KEEP`'d via the linker-script fragment we inject
    (`runtime-native/link/stackmaps_nopie.ld`, `runtime-native/link/stackmaps.ld`,
    or `runtime-native/stackmaps.ld`).
  - On Linux x86_64, PIE binaries require runtime relocations for stackmap
    FunctionAddress entries. If stackmaps/faultmaps are mapped read-only, GNU ld
    emits `DT_TEXTREL` and lld typically rejects the link. We avoid this by
    rewriting input objects so the stackmap sections are writable during
    relocation.
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

if [[ "${OSTYPE:-}" != linux* ]]; then
  echo "native_link.sh: Linux-only (ELF)" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ecma_root="$(cd "${script_dir}/.." && pwd)"
stackmaps_ld_lld="${ecma_root}/runtime-native/link/stackmaps.ld"
stackmaps_ld_nopie="${ecma_root}/runtime-native/link/stackmaps_nopie.ld"
stackmaps_ld_gnuld="${ecma_root}/runtime-native/link/stackmaps_gnuld.ld"

stackmaps_ld="${stackmaps_ld_lld}"
if [[ ! -f "${stackmaps_ld}" ]]; then
  # Compatibility path for older docs/build scripts.
  stackmaps_ld="${ecma_root}/runtime-native/stackmaps.ld"
fi
if [[ ! -f "${stackmaps_ld}" ]]; then
  echo "native_link.sh: missing linker script fragment at ${stackmaps_ld}" >&2
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

default_linker="ld"
if command -v ld.lld-18 >/dev/null 2>&1 || command -v ld.lld >/dev/null 2>&1; then
  default_linker="lld"
fi

LINKER="${ECMA_RS_NATIVE_LINKER:-${default_linker}}"
PIE="${ECMA_RS_NATIVE_PIE:-0}"
GC_SECTIONS="${ECMA_RS_NATIVE_GC_SECTIONS:-1}"

# GNU ld has a known pitfall: inserting a writable stackmaps section immediately
# after `.text` (as in `stackmaps_nopie.ld`) can cause GNU ld to merge it into the
# text PT_LOAD, producing an RWX segment. Prefer the GNU ld fragment whenever the
# selected linker is `ld` so both PIE and non-PIE links avoid this hazard even if
# the input objects already contain writable `.data.rel.ro.llvm_*` sections.
if [[ "${LINKER}" == "ld" && -f "${stackmaps_ld_gnuld}" ]]; then
  stackmaps_ld="${stackmaps_ld_gnuld}"
# Otherwise, prefer the dedicated non-PIE fragment when available. This avoids
# lld's RELRO contiguity constraints and does not require patching stackmap
# section flags.
elif [[ "${PIE}" != "1" && -f "${stackmaps_ld_nopie}" ]]; then
  stackmaps_ld="${stackmaps_ld_nopie}"
fi

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
    # Prefer version-suffixed lld if installed.
    if command -v ld.lld-18 >/dev/null 2>&1; then
      link_args+=("-fuse-ld=lld-18")
    else
      link_args+=("-fuse-ld=lld")
    fi
    ;;
  *)
    echo "native_link.sh: unsupported ECMA_RS_NATIVE_LINKER=${LINKER} (expected ld|lld)" >&2
    exit 2
    ;;
esac

if [[ "${GC_SECTIONS}" == "1" ]]; then
  link_args+=("-Wl,--gc-sections")
fi

# Always inject the script so the binary exports stackmap boundary symbols and
# stackmap sections are never dropped under `--gc-sections`.
link_args+=("-Wl,-T,${stackmaps_ld}")

# Ensure `.llvm_stackmaps` / `.llvm_faultmaps` (and their `.data.rel.ro.*`
# variants) are writable in the *input* objects.
#
# - PIE/DSO: required so runtime relocations don't force DT_TEXTREL / `-z notext`.
# - When using the PIE linker fragment (`runtime-native/link/stackmaps.ld`),
#   we also rename `.llvm_{stackmaps,faultmaps}` into `.data.rel.ro.llvm_*` so
#   the linker script's `KEEP(*(.data.rel.ro.llvm_* ...))` patterns match.
patched_dir=""
cleanup() {
  if [[ -n "${patched_dir}" ]]; then
    rm -rf "${patched_dir}"
  fi
}
trap cleanup EXIT

if [[ "${PIE}" == "1" || ( "${LINKER}" == "lld" && "${stackmaps_ld}" != "${stackmaps_ld_nopie}" ) ]]; then
  objcopy=""
  for cand in llvm-objcopy-18 llvm-objcopy objcopy; do
    if command -v "${cand}" >/dev/null 2>&1; then
      objcopy="${cand}"
      break
    fi
  done
  if [[ -z "${objcopy}" ]]; then
    echo "native_link.sh: PIE or lld requires llvm-objcopy/objcopy to patch .llvm_stackmaps flags" >&2
    exit 2
  fi

  patched_dir="$(mktemp -d)"
  patched_objs=()
  for i in "${!objs[@]}"; do
    src="${objs[$i]}"
    dst="${patched_dir}/obj${i}.o"
    cp "${src}" "${dst}"
    # For PIE/DSO links, prefer to relocate stackmaps/faultmaps into
    # `.data.rel.ro.llvm_*` sections so the linker script can place them into a
    # writable output section without requiring `DT_TEXTREL`.
    if [[ "${PIE}" == "1" || ( "${LINKER}" == "lld" && "${stackmaps_ld}" != "${stackmaps_ld_nopie}" ) ]]; then
      "${objcopy}" --rename-section \
        ".llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents" \
        "${dst}"
      "${objcopy}" --rename-section \
        ".llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents" \
        "${dst}"
    fi
    if [[ "${objcopy}" == *llvm-objcopy* ]]; then
      "${objcopy}" --set-section-flags=.llvm_stackmaps=alloc,load,contents,data "${dst}"
      "${objcopy}" --set-section-flags=.llvm_faultmaps=alloc,load,contents,data "${dst}"
      "${objcopy}" --set-section-flags=.data.rel.ro.llvm_stackmaps=alloc,load,contents,data "${dst}"
      "${objcopy}" --set-section-flags=.data.rel.ro.llvm_faultmaps=alloc,load,contents,data "${dst}"
    else
      "${objcopy}" --set-section-flags .llvm_stackmaps=alloc,load,contents,data "${dst}"
      "${objcopy}" --set-section-flags .llvm_faultmaps=alloc,load,contents,data "${dst}"
      "${objcopy}" --set-section-flags .data.rel.ro.llvm_stackmaps=alloc,load,contents,data "${dst}"
      "${objcopy}" --set-section-flags .data.rel.ro.llvm_faultmaps=alloc,load,contents,data "${dst}"
    fi
    patched_objs+=("${dst}")
  done
  objs=("${patched_objs[@]}")
fi

exec "${CLANG}" "${link_args[@]}" -o "${out}" "${objs[@]}"
