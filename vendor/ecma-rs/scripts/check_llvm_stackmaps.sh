#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Guardrail: this repo standardizes on LLVM's production GC strategy name (`coreclr`).
# LLVM's demo/reference GC strategy ("statepoint-" + "example") is intentionally *not* checked in
# to avoid drift between modules and to keep fixture expectations stable.
#
# Keep this check in the lightweight CI path (it runs before any LLVM work below).
gc_demo_strategy="statepoint-"
gc_demo_strategy="${gc_demo_strategy}example"
if grep -r --line-number \
  --exclude='*.md' \
  --exclude-dir='target' \
  --exclude-dir='test262' \
  --exclude-dir='test262-semantic' \
  --exclude-dir='TypeScript' \
  --exclude-dir='.git' \
  --binary-files=without-match \
  "${gc_demo_strategy}" "${script_dir}/.."; then
  echo "error: found disallowed LLVM GC strategy name \"${gc_demo_strategy}\" in non-markdown files under vendor/ecma-rs" >&2
  echo "note: this repo standardizes on gc \"coreclr\"; see vendor/ecma-rs/native-js/docs/llvm_gc_strategy.md" >&2
  exit 1
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

CLANG="${ECMA_RS_NATIVE_CLANG:-$(pick_cmd clang-18 clang)}"
READELF="$(pick_cmd readelf)"
OBJCOPY="$(pick_cmd objcopy)"
STRIP="$(pick_cmd strip)"
LLVM_STRIP="$(command -v llvm-strip || true)"
LLVM_READOBJ="$(command -v llvm-readobj-18 || command -v llvm-readobj || true)"
LLVM_OBJCOPY="$(command -v llvm-objcopy-18 || command -v llvm-objcopy || true)"

stackmaps_ld_pie="${script_dir}/../runtime-native/link/stackmaps.ld"
stackmaps_ld_nopie="${script_dir}/../runtime-native/link/stackmaps_nopie.ld"
if [[ ! -f "${stackmaps_ld_pie}" ]]; then
  # Compatibility path for older docs/build scripts.
  stackmaps_ld_pie="${script_dir}/../runtime-native/stackmaps.ld"
fi
if [[ ! -f "${stackmaps_ld_pie}" ]]; then
  echo "error: missing PIE stackmaps linker script fragment at ${stackmaps_ld_pie}" >&2
  exit 1
fi
if [[ ! -f "${stackmaps_ld_nopie}" ]]; then
  # Older checkouts only have a single fragment; treat it as the non-PIE fallback.
  stackmaps_ld_nopie="${stackmaps_ld_pie}"
fi

LLD_FUSE=""
if command -v ld.lld-18 >/dev/null 2>&1; then
  LLD_FUSE="lld-18"
elif command -v ld.lld >/dev/null 2>&1; then
  LLD_FUSE="lld"
fi

tmp="$(mktemp -d)"
cleanup() { rm -rf "${tmp}"; }
trap cleanup EXIT

cat >"${tmp}/mod_a.ll" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

declare token @llvm.experimental.gc.statepoint.p0(i64 immarg, i32 immarg, ptr, i32 immarg, i32 immarg, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32 immarg, i32 immarg)

define ptr addrspace(1) @fooA(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 2882400001, i32 0, ptr elementtype(void ()) @callee, i32 0, i32 0, i32 0, i32 0
  ) ["gc-live"(ptr addrspace(1) %obj)]
  %obj.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  ret ptr addrspace(1) %obj.relocated
}
EOF

cat >"${tmp}/mod_b.ll" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

declare token @llvm.experimental.gc.statepoint.p0(i64 immarg, i32 immarg, ptr, i32 immarg, i32 immarg, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32 immarg, i32 immarg)

define ptr addrspace(1) @fooB(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 2882400002, i32 0, ptr elementtype(void ()) @callee, i32 0, i32 0, i32 0, i32 0
  ) ["gc-live"(ptr addrspace(1) %obj)]
  %obj.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  ret ptr addrspace(1) %obj.relocated
}
EOF

cat >"${tmp}/main.ll" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare ptr addrspace(1) @fooA(ptr addrspace(1))
declare ptr addrspace(1) @fooB(ptr addrspace(1))

define i32 @main() {
entry:
  %a = call ptr addrspace(1) @fooA(ptr addrspace(1) null)
  %b = call ptr addrspace(1) @fooB(ptr addrspace(1) %a)
  ret i32 0
}
EOF

cat >"${tmp}/callee.c" <<'EOF'
void callee(void) {}
EOF

"${CLANG}" -c -O0 -o "${tmp}/faultmaps.o" -x assembler - <<'EOF'
.section .llvm_faultmaps,"a",@progbits
  .quad 0xfeedfacefeedface
.section .note.GNU-stack,"",@progbits
EOF

"${CLANG}" -c -O0 -o "${tmp}/mod_a.o" "${tmp}/mod_a.ll" \
  -mllvm --fixup-allow-gcptr-in-csr=false -mllvm --fixup-max-csr-statepoints=0
"${CLANG}" -c -O0 -o "${tmp}/mod_b.o" "${tmp}/mod_b.ll" \
  -mllvm --fixup-allow-gcptr-in-csr=false -mllvm --fixup-max-csr-statepoints=0
"${CLANG}" -c -O0 -o "${tmp}/main.o" "${tmp}/main.ll" \
  -mllvm --fixup-allow-gcptr-in-csr=false -mllvm --fixup-max-csr-statepoints=0
"${CLANG}" -c -O0 -o "${tmp}/callee.o" "${tmp}/callee.c"

objs=("${tmp}/main.o" "${tmp}/mod_a.o" "${tmp}/mod_b.o" "${tmp}/callee.o" "${tmp}/faultmaps.o")

readelf_sections() {
  # Normalize `readelf -S` output to stable `name addr_hex size_hex` triples.
  #
  # `readelf -W -S` prints section indices as either:
  # - `[ 1] .interp ...` (space for single-digit indices; `$1` becomes `[`), or
  # - `[10] .rela.plt ...` (no space; `$1` becomes `[10]`).
  #
  # Keep this parsing logic in one place so the rest of the script doesn't depend
  # on column offsets.
  local bin="$1"
  "${READELF}" -W -S "${bin}" | awk '
    $1 == "[" {
      # Single-digit section index: `[ 1] <name> <type> <addr> <off> <size> ...`
      # Section 0 has an empty name, which shifts columns; skip it.
      if ($3 == "NULL") next
      print $3, $5, $7
      next
    }
    $1 ~ /^[[][0-9]+[]]$/ {
      # Two+ digit section index: `[10] <name> <type> <addr> <off> <size> ...`
      print $2, $4, $6
      next
    }
  '
}

must_have_stackmaps() {
  local bin="$1"
  # Prefer a dedicated stackmaps output section name when present. Fall back to
  # `.data.rel.ro` for link layouts that embed stackmaps into the standard RELRO
  # data output section (e.g. the lld PIE fragment).

  local sec_name=""
  local sec_size_hex=""
  for cand in ".data.rel.ro.llvm_stackmaps" ".llvm_stackmaps" ".data.rel.ro"; do
    sec_size_hex="$(readelf_sections "${bin}" | awk -v n="${cand}" '$1==n { print $3; exit }')"
    if [[ -n "${sec_size_hex}" ]]; then
      sec_name="${cand}"
      break
    fi
  done
  if [[ -z "${sec_size_hex}" ]]; then
    echo "expected stackmaps section (.data.rel.ro.llvm_stackmaps, .llvm_stackmaps, or .data.rel.ro) in: ${bin}" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi

  local sec_size_dec=$((16#${sec_size_hex}))
  if [[ "${sec_size_dec}" -le 0 ]]; then
    echo "expected non-empty ${sec_name} in: ${bin} (size=0x${sec_size_hex})" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi

  # `.data.rel.ro` exists even in many binaries that do *not* contain stackmaps. If we're using it
  # as the container, require a non-empty linker-defined symbol range so this check doesn't
  # accidentally pass when stackmaps were GC'd.
  if [[ "${sec_name}" == ".data.rel.ro" ]]; then
    local start_hex stop_hex
    start_hex="$("${READELF}" -W -s "${bin}" | awk '$8=="__start_llvm_stackmaps" { print $2; exit }')"
    stop_hex="$("${READELF}" -W -s "${bin}" | awk '$8=="__stop_llvm_stackmaps" { print $2; exit }')"
    if [[ -z "${start_hex}" || -z "${stop_hex}" ]]; then
      echo "expected __start_llvm_stackmaps/__stop_llvm_stackmaps when stackmaps are embedded in .data.rel.ro: ${bin}" >&2
      "${READELF}" -W -s "${bin}" >&2 || true
      exit 1
    fi
    local start_dec=$((16#${start_hex}))
    local stop_dec=$((16#${stop_hex}))
    if (( stop_dec <= start_dec )); then
      echo "expected non-empty stackmaps symbol range when stackmaps are embedded in .data.rel.ro: ${bin}" >&2
      echo "  __start_llvm_stackmaps=0x${start_hex} __stop_llvm_stackmaps=0x${stop_hex}" >&2
      exit 1
    fi
  fi
}

must_have_faultmaps() {
  local bin="$1"
  # Prefer a dedicated faultmaps output section name when present. Fall back to
  # `.data.rel.ro` for link layouts that embed faultmaps into the standard RELRO
  # data output section (e.g. the lld PIE fragment).

  local sec_name=""
  local sec_size_hex=""
  for cand in ".data.rel.ro.llvm_faultmaps" ".llvm_faultmaps" ".data.rel.ro"; do
    sec_size_hex="$(readelf_sections "${bin}" | awk -v n="${cand}" '$1==n { print $3; exit }')"
    if [[ -n "${sec_size_hex}" ]]; then
      sec_name="${cand}"
      break
    fi
  done
  if [[ -z "${sec_size_hex}" ]]; then
    echo "expected faultmaps section (.data.rel.ro.llvm_faultmaps, .llvm_faultmaps, or .data.rel.ro) in: ${bin}" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi

  local sec_size_dec=$((16#${sec_size_hex}))
  if [[ "${sec_size_dec}" -le 0 ]]; then
    echo "expected non-empty ${sec_name} in: ${bin} (size=0x${sec_size_hex})" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi

  if [[ "${sec_name}" == ".data.rel.ro" ]]; then
    local start_hex stop_hex
    start_hex="$("${READELF}" -W -s "${bin}" | awk '$8=="__llvm_faultmaps_start" { print $2; exit }')"
    stop_hex="$("${READELF}" -W -s "${bin}" | awk '$8=="__llvm_faultmaps_end" { print $2; exit }')"
    if [[ -z "${start_hex}" || -z "${stop_hex}" ]]; then
      echo "expected __llvm_faultmaps_start/__llvm_faultmaps_end when faultmaps are embedded in .data.rel.ro: ${bin}" >&2
      "${READELF}" -W -s "${bin}" >&2 || true
      exit 1
    fi
    local start_dec=$((16#${start_hex}))
    local stop_dec=$((16#${stop_hex}))
    if (( stop_dec <= start_dec )); then
      echo "expected non-empty faultmaps symbol range when faultmaps are embedded in .data.rel.ro: ${bin}" >&2
      echo "  __llvm_faultmaps_start=0x${start_hex} __llvm_faultmaps_end=0x${stop_hex}" >&2
      exit 1
    fi
  fi
}

must_have_stackmaps_symbols() {
  local bin="$1"

  # The linker script fragment is expected to define stable boundary symbols for
  # in-process discovery (used by runtime-native's fast path).
  local start_hex stop_hex
  start_hex="$(
    "${READELF}" -W -s "${bin}" \
      | awk '$8=="__start_llvm_stackmaps" { if (!found) { print $2; found = 1 } }'
  )"
  stop_hex="$(
    "${READELF}" -W -s "${bin}" \
      | awk '$8=="__stop_llvm_stackmaps" { if (!found) { print $2; found = 1 } }'
  )"
  if [[ -z "${start_hex}" || -z "${stop_hex}" ]]; then
    echo "expected __start_llvm_stackmaps/__stop_llvm_stackmaps in: ${bin}" >&2
    "${READELF}" -W -s "${bin}" >&2 || true
    exit 1
  fi

  local start_dec=$((16#${start_hex}))
  local stop_dec=$((16#${stop_hex}))

  if [[ "${stop_dec}" -le "${start_dec}" ]]; then
    echo "invalid stackmaps symbol range in: ${bin} (start=0x${start_hex} stop=0x${stop_hex})" >&2
    exit 1
  fi

  # StackMap v3 uses 64-bit fields and is 8-byte aligned; the runtime's fast
  # path rejects misaligned symbol ranges.
  local len_dec=$((stop_dec - start_dec))
  if (( start_dec % 8 != 0 || len_dec % 8 != 0 )); then
    echo "invalid stackmaps symbol alignment in: ${bin}" >&2
    echo "  __start_llvm_stackmaps=0x${start_hex} __stop_llvm_stackmaps=0x${stop_hex} (len=0x$(printf '%x' "${len_dec}"))" >&2
    exit 1
  fi

  # Ensure the symbol range is contained in some allocated section (often
  # `.data.rel.ro.llvm_stackmaps`, `.llvm_stackmaps`, or `.data.rel.ro`).
  local container=""
  while read -r sec_name sec_addr_hex sec_size_hex; do
    if [[ -z "${sec_name}" || -z "${sec_addr_hex}" || -z "${sec_size_hex}" ]]; then
      continue
    fi
    # Skip non-addressed sections (e.g. ".comment") and malformed lines.
    if [[ "${sec_addr_hex}" == "0000000000000000" ]]; then
      continue
    fi
    local sec_addr_dec=$((16#${sec_addr_hex}))
    local sec_size_dec=$((16#${sec_size_hex}))
    local sec_end_dec=$((sec_addr_dec + sec_size_dec))
    if (( sec_addr_dec <= start_dec && stop_dec <= sec_end_dec )); then
      container="${sec_name}"
      break
    fi
  done < <(readelf_sections "${bin}")

  if [[ -z "${container}" ]]; then
    echo "stackmaps symbol range not contained in any section in: ${bin}" >&2
    echo "  __start_llvm_stackmaps=0x${start_hex} __stop_llvm_stackmaps=0x${stop_hex}" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    "${READELF}" -W -s "${bin}" >&2 || true
    exit 1
  fi
}

must_have_faultmaps_symbols() {
  local bin="$1"

  local start_hex stop_hex
  start_hex="$(
    "${READELF}" -W -s "${bin}" \
      | awk '$8=="__llvm_faultmaps_start" { if (!found) { print $2; found = 1 } }'
  )"
  stop_hex="$(
    "${READELF}" -W -s "${bin}" \
      | awk '$8=="__llvm_faultmaps_end" { if (!found) { print $2; found = 1 } }'
  )"
  if [[ -z "${start_hex}" || -z "${stop_hex}" ]]; then
    echo "expected __llvm_faultmaps_start/__llvm_faultmaps_end in: ${bin}" >&2
    "${READELF}" -W -s "${bin}" >&2 || true
    exit 1
  fi

  local start_dec=$((16#${start_hex}))
  local stop_dec=$((16#${stop_hex}))

  if [[ "${stop_dec}" -le "${start_dec}" ]]; then
    echo "invalid faultmaps symbol range in: ${bin} (start=0x${start_hex} stop=0x${stop_hex})" >&2
    exit 1
  fi

  # Faultmaps are a sequence of fixed-width 64-bit entries; keep the exported
  # symbol range 8-byte aligned.
  local len_dec=$((stop_dec - start_dec))
  if (( start_dec % 8 != 0 || len_dec % 8 != 0 )); then
    echo "invalid faultmaps symbol alignment in: ${bin}" >&2
    echo "  __llvm_faultmaps_start=0x${start_hex} __llvm_faultmaps_end=0x${stop_hex} (len=0x$(printf '%x' "${len_dec}"))" >&2
    exit 1
  fi

  # Ensure the symbol range is contained in some allocated section (often
  # `.data.rel.ro.llvm_faultmaps`, `.llvm_faultmaps`, or `.data.rel.ro`).
  local container=""
  while read -r sec_name sec_addr_hex sec_size_hex; do
    if [[ -z "${sec_name}" || -z "${sec_addr_hex}" || -z "${sec_size_hex}" ]]; then
      continue
    fi
    if [[ "${sec_addr_hex}" == "0000000000000000" ]]; then
      continue
    fi
    local sec_addr_dec=$((16#${sec_addr_hex}))
    local sec_size_dec=$((16#${sec_size_hex}))
    local sec_end_dec=$((sec_addr_dec + sec_size_dec))
    if (( sec_addr_dec <= start_dec && stop_dec <= sec_end_dec )); then
      container="${sec_name}"
      break
    fi
  done < <(readelf_sections "${bin}")

  if [[ -z "${container}" ]]; then
    echo "faultmaps symbol range not contained in any section in: ${bin}" >&2
    echo "  __llvm_faultmaps_start=0x${start_hex} __llvm_faultmaps_end=0x${stop_hex}" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    "${READELF}" -W -s "${bin}" >&2 || true
    exit 1
  fi
}

must_not_have_stackmaps() {
  local bin="$1"
  if readelf_sections "${bin}" | awk '$1==".data.rel.ro.llvm_stackmaps" || $1==".llvm_stackmaps" {found=1} END {exit !found}'; then
    echo "expected no stackmaps section in: ${bin}" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi
}

must_not_have_faultmaps() {
  local bin="$1"
  if readelf_sections "${bin}" | awk '$1==".data.rel.ro.llvm_faultmaps" || $1==".llvm_faultmaps" {found=1} END {exit !found}'; then
    echo "expected no faultmaps section in: ${bin}" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi
}

must_have_textrel() {
  local bin="$1"
  if ! "${READELF}" -d "${bin}" 2>/dev/null | grep "TEXTREL" >/dev/null; then
    echo "expected DT_TEXTREL in: ${bin}" >&2
    "${READELF}" -d "${bin}" >&2 || true
    exit 1
  fi
}

must_not_have_textrel() {
  local bin="$1"
  if "${READELF}" -d "${bin}" 2>/dev/null | grep "TEXTREL" >/dev/null; then
    echo "expected no DT_TEXTREL in: ${bin}" >&2
    "${READELF}" -d "${bin}" >&2 || true
    exit 1
  fi
}

must_not_have_rwx_segment() {
  local bin="$1"
  if "${READELF}" -l "${bin}" 2>/dev/null | grep "RWE" >/dev/null; then
    echo "expected no RWX LOAD segment in: ${bin}" >&2
    "${READELF}" -l "${bin}" >&2 || true
    exit 1
  fi
}

must_have_stackmaps_in_relro() {
  local bin="$1"

  local start_hex stop_hex
  start_hex="$("${READELF}" -W -s "${bin}" | awk '$8=="__start_llvm_stackmaps" { print $2; exit }')"
  stop_hex="$("${READELF}" -W -s "${bin}" | awk '$8=="__stop_llvm_stackmaps" { print $2; exit }')"
  if [[ -z "${start_hex}" || -z "${stop_hex}" ]]; then
    echo "expected __start_llvm_stackmaps/__stop_llvm_stackmaps for RELRO coverage check in: ${bin}" >&2
    "${READELF}" -W -s "${bin}" >&2 || true
    exit 1
  fi

  local start_dec=$((16#${start_hex}))
  local stop_dec=$((16#${stop_hex}))

  local segments
  segments="$("${READELF}" -W -l "${bin}")"

  local relro_vaddr_hex relro_memsz_hex
  read -r relro_vaddr_hex relro_memsz_hex < <(printf '%s\n' "${segments}" | awk '$1=="GNU_RELRO" { print $3, $6; exit }')
  if [[ -z "${relro_vaddr_hex}" || -z "${relro_memsz_hex}" ]]; then
    echo "expected a GNU_RELRO program header for RELRO coverage check in: ${bin}" >&2
    echo "${segments}" >&2
    exit 1
  fi

  relro_vaddr_hex="${relro_vaddr_hex#0x}"
  relro_memsz_hex="${relro_memsz_hex#0x}"
  local relro_vaddr_dec=$((16#${relro_vaddr_hex}))
  local relro_memsz_dec=$((16#${relro_memsz_hex}))
  local relro_end_dec=$((relro_vaddr_dec + relro_memsz_dec))

  if (( start_dec < relro_vaddr_dec || stop_dec > relro_end_dec )); then
    echo "expected stackmaps range to be covered by PT_GNU_RELRO in: ${bin}" >&2
    echo "  stackmaps: start=0x${start_hex} stop=0x${stop_hex}" >&2
    echo "  relro:     vaddr=0x${relro_vaddr_hex} memsz=0x${relro_memsz_hex}" >&2
    echo "${segments}" >&2
    exit 1
  fi
}

echo "[stackmaps] link: ld (no-pie, no gc-sections)"
"${CLANG}" -no-pie -o "${tmp}/a_ld_nogc" "${objs[@]}"
must_have_stackmaps "${tmp}/a_ld_nogc"
must_have_faultmaps "${tmp}/a_ld_nogc"

echo "[stackmaps] link: ld (pie) => EXPECTED DT_TEXTREL"
if "${CLANG}" -pie -o "${tmp}/a_ld_pie_textrel" "${objs[@]}" 2>"${tmp}/a_ld_pie_textrel.stderr"; then
  must_have_stackmaps "${tmp}/a_ld_pie_textrel"
  must_have_textrel "${tmp}/a_ld_pie_textrel"
else
  echo "[stackmaps] note: ld PIE link failed; skipping DT_TEXTREL check" >&2
  cat "${tmp}/a_ld_pie_textrel.stderr" >&2 || true
fi

echo "[stackmaps] link: ld (pie, patched stackmaps) => EXPECTED NO DT_TEXTREL"
cp "${tmp}/mod_a.o" "${tmp}/mod_a.pie.o"
cp "${tmp}/mod_b.o" "${tmp}/mod_b.pie.o"
"${OBJCOPY}" --set-section-flags .llvm_stackmaps=alloc,load,contents,data "${tmp}/mod_a.pie.o"
"${OBJCOPY}" --set-section-flags .llvm_stackmaps=alloc,load,contents,data "${tmp}/mod_b.pie.o"
if "${CLANG}" -pie -o "${tmp}/a_ld_pie_no_textrel" "${tmp}/main.o" "${tmp}/mod_a.pie.o" "${tmp}/mod_b.pie.o" "${tmp}/callee.o" 2>"${tmp}/a_ld_pie_no_textrel.stderr"; then
  must_have_stackmaps "${tmp}/a_ld_pie_no_textrel"
  must_not_have_textrel "${tmp}/a_ld_pie_no_textrel"
else
  echo "[stackmaps] note: ld PIE link failed; skipping patched PIE check" >&2
  cat "${tmp}/a_ld_pie_no_textrel.stderr" >&2 || true
fi

echo "[stackmaps] link: ld (no-pie, --gc-sections) => EXPECTED DROP"
"${CLANG}" -no-pie -Wl,--gc-sections -o "${tmp}/a_ld_gc" "${objs[@]}"
must_not_have_stackmaps "${tmp}/a_ld_gc"
must_not_have_faultmaps "${tmp}/a_ld_gc"

echo "[stackmaps] link: ld (no-pie, --gc-sections + stackmaps_nopie.ld KEEP)"
"${CLANG}" -no-pie -Wl,--gc-sections -Wl,-T,"${stackmaps_ld_nopie}" \
  -o "${tmp}/a_ld_policy" "${objs[@]}"
must_have_stackmaps "${tmp}/a_ld_policy"
must_have_stackmaps_symbols "${tmp}/a_ld_policy"
must_have_faultmaps "${tmp}/a_ld_policy"
must_have_faultmaps_symbols "${tmp}/a_ld_policy"

echo "[stackmaps] link: native_link.sh (no-pie, --gc-sections + KEEP)"
# Invoke via `bash` instead of executing directly:
# - some vendored scripts are checked in without the executable bit
# - some CI/agent environments mount repositories with `noexec`
bash "${script_dir}/native_link.sh" -o "${tmp}/a_policy" "${objs[@]}"
must_have_stackmaps "${tmp}/a_policy"
must_have_stackmaps_symbols "${tmp}/a_policy"
must_have_faultmaps "${tmp}/a_policy"
must_have_faultmaps_symbols "${tmp}/a_policy"

echo "[stackmaps] link: native_link.sh (ld explicit)"
ECMA_RS_NATIVE_LINKER=ld bash "${script_dir}/native_link.sh" -o "${tmp}/a_policy_ld" "${objs[@]}"
must_have_stackmaps "${tmp}/a_policy_ld"
must_have_stackmaps_symbols "${tmp}/a_policy_ld"
must_have_faultmaps "${tmp}/a_policy_ld"
must_have_faultmaps_symbols "${tmp}/a_policy_ld"

echo "[stackmaps] link: native_link.sh (ld + PIE; stackmaps patched via objcopy)"
ECMA_RS_NATIVE_LINKER=ld ECMA_RS_NATIVE_PIE=1 bash "${script_dir}/native_link.sh" -o "${tmp}/a_policy_ld_pie" "${objs[@]}"
must_have_stackmaps "${tmp}/a_policy_ld_pie"
must_have_stackmaps_symbols "${tmp}/a_policy_ld_pie"
must_have_faultmaps "${tmp}/a_policy_ld_pie"
must_have_faultmaps_symbols "${tmp}/a_policy_ld_pie"
must_not_have_textrel "${tmp}/a_policy_ld_pie"
must_not_have_rwx_segment "${tmp}/a_policy_ld_pie"
must_have_stackmaps_in_relro "${tmp}/a_policy_ld_pie"

if [[ -n "${LLD_FUSE}" ]]; then
  echo "[stackmaps] link: lld (no-pie, no gc-sections)"
  "${CLANG}" -fuse-ld="${LLD_FUSE}" -no-pie -o "${tmp}/a_lld_nogc" "${objs[@]}"
  must_have_stackmaps "${tmp}/a_lld_nogc"
  must_have_faultmaps "${tmp}/a_lld_nogc"

  echo "[stackmaps] link: lld (pie, unpatched) => EXPECTED FAIL"
  if "${CLANG}" -fuse-ld="${LLD_FUSE}" -pie -o "${tmp}/a_lld_pie_unpatched" "${objs[@]}" 2>"${tmp}/a_lld_pie_unpatched.stderr"; then
    echo "[stackmaps] warning: lld PIE link unexpectedly succeeded; ensuring no DT_TEXTREL" >&2
    must_not_have_textrel "${tmp}/a_lld_pie_unpatched"
  else
    echo "[stackmaps] ok: lld rejected PIE without stackmaps patching (expected)"
    if ! grep -q "relocation R_X86_64_64" "${tmp}/a_lld_pie_unpatched.stderr" 2>/dev/null; then
      echo "[stackmaps] note: lld failed for an unexpected reason; stderr follows:" >&2
      cat "${tmp}/a_lld_pie_unpatched.stderr" >&2 || true
    fi
  fi

  echo "[stackmaps] link: lld (no-pie, --gc-sections) => EXPECTED DROP"
  "${CLANG}" -fuse-ld="${LLD_FUSE}" -no-pie -Wl,--gc-sections -o "${tmp}/a_lld_gc" "${objs[@]}"
  must_not_have_stackmaps "${tmp}/a_lld_gc"
  must_not_have_faultmaps "${tmp}/a_lld_gc"

  echo "[stackmaps] link: lld (no-pie, --gc-sections + stackmaps_nopie.ld KEEP)"
  "${CLANG}" -fuse-ld="${LLD_FUSE}" -no-pie -Wl,--gc-sections -Wl,-T,"${stackmaps_ld_nopie}" \
    -o "${tmp}/a_lld_policy" "${objs[@]}"
  must_have_stackmaps "${tmp}/a_lld_policy"
  must_have_stackmaps_symbols "${tmp}/a_lld_policy"
  must_have_faultmaps "${tmp}/a_lld_policy"
  must_have_faultmaps_symbols "${tmp}/a_lld_policy"

  echo "[stackmaps] link: native_link.sh (lld explicit)"
  ECMA_RS_NATIVE_LINKER=lld bash "${script_dir}/native_link.sh" -o "${tmp}/a_policy_lld" "${objs[@]}"
  must_have_stackmaps "${tmp}/a_policy_lld"
  must_have_stackmaps_symbols "${tmp}/a_policy_lld"
  must_have_faultmaps "${tmp}/a_policy_lld"
  must_have_faultmaps_symbols "${tmp}/a_policy_lld"

  if [[ -n "${LLVM_OBJCOPY}" ]]; then
    echo "[stackmaps] link: native_link.sh (lld + PIE; stackmaps patched via llvm-objcopy)"
    ECMA_RS_NATIVE_LINKER=lld ECMA_RS_NATIVE_PIE=1 bash "${script_dir}/native_link.sh" -o "${tmp}/a_policy_lld_pie" "${objs[@]}"
    must_have_stackmaps "${tmp}/a_policy_lld_pie"
    must_have_stackmaps_symbols "${tmp}/a_policy_lld_pie"
    must_have_faultmaps "${tmp}/a_policy_lld_pie"
    must_have_faultmaps_symbols "${tmp}/a_policy_lld_pie"
    must_not_have_textrel "${tmp}/a_policy_lld_pie"
    must_not_have_rwx_segment "${tmp}/a_policy_lld_pie"
    # Note: the lld-oriented PIE linker fragment (`runtime-native/link/stackmaps.ld`) appends the
    # stackmaps/faultmaps payload into the standard `.data.rel.ro` output section (after rewriting
    # the *input* sections to `.data.rel.ro.llvm_*`). It anchors at `.dynamic` so the payload stays
    # covered by `PT_GNU_RELRO` without triggering lld's RELRO contiguity errors.
    must_have_stackmaps_in_relro "${tmp}/a_policy_lld_pie"

    echo "[stackmaps] link: lld (pie, full RELRO) => EXPECTED OK"
    # Rust's default Linux hardening flags include full RELRO (`-z relro -z now`).
    # Keep this in CI so changes to `stackmaps.ld` don't accidentally make lld
    # reject hardened links (e.g. `relro sections not contiguous`).
    cp "${tmp}/mod_a.o" "${tmp}/mod_a.pie.relro_now.o"
    cp "${tmp}/mod_b.o" "${tmp}/mod_b.pie.relro_now.o"
    cp "${tmp}/faultmaps.o" "${tmp}/faultmaps.pie.relro_now.o"
    "${LLVM_OBJCOPY}" --rename-section \
      ".llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents" \
      "${tmp}/mod_a.pie.relro_now.o"
    "${LLVM_OBJCOPY}" --rename-section \
      ".llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents" \
      "${tmp}/mod_b.pie.relro_now.o"
    "${LLVM_OBJCOPY}" --rename-section \
      ".llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents" \
      "${tmp}/faultmaps.pie.relro_now.o"
    "${CLANG}" -fuse-ld="${LLD_FUSE}" -pie -Wl,-z,relro -Wl,-z,now -Wl,--gc-sections -Wl,-T,"${stackmaps_ld_pie}" \
      -o "${tmp}/a_lld_pie_relro_now" \
      "${tmp}/main.o" "${tmp}/mod_a.pie.relro_now.o" "${tmp}/mod_b.pie.relro_now.o" "${tmp}/callee.o" "${tmp}/faultmaps.pie.relro_now.o"
    must_have_stackmaps "${tmp}/a_lld_pie_relro_now"
    must_have_stackmaps_symbols "${tmp}/a_lld_pie_relro_now"
    must_have_faultmaps "${tmp}/a_lld_pie_relro_now"
    must_have_faultmaps_symbols "${tmp}/a_lld_pie_relro_now"
    must_not_have_textrel "${tmp}/a_lld_pie_relro_now"
    must_not_have_rwx_segment "${tmp}/a_lld_pie_relro_now"
    must_have_stackmaps_in_relro "${tmp}/a_lld_pie_relro_now"
  else
    echo "[stackmaps] note: llvm-objcopy not found; skipping PIE+lld policy link"
  fi
else
  echo "[stackmaps] note: ld.lld not found; skipping lld matrix"
fi

echo "[stackmaps] strip: GNU strip"
cp "${tmp}/a_policy" "${tmp}/a_policy.strip"
"${STRIP}" "${tmp}/a_policy.strip"
must_have_stackmaps "${tmp}/a_policy.strip"
must_have_faultmaps "${tmp}/a_policy.strip"

echo "[stackmaps] strip: objcopy --strip-unneeded"
cp "${tmp}/a_policy" "${tmp}/a_policy.objcopy_strip_unneeded"
"${OBJCOPY}" --strip-unneeded "${tmp}/a_policy.objcopy_strip_unneeded"
must_have_stackmaps "${tmp}/a_policy.objcopy_strip_unneeded"
must_have_faultmaps "${tmp}/a_policy.objcopy_strip_unneeded"

echo "[stackmaps] strip: native_strip.sh"
cp "${tmp}/a_policy" "${tmp}/a_policy.native_strip"
bash "${script_dir}/native_strip.sh" "${tmp}/a_policy.native_strip"
must_have_stackmaps "${tmp}/a_policy.native_strip"
must_have_faultmaps "${tmp}/a_policy.native_strip"

if [[ -n "${LLVM_STRIP}" ]]; then
  echo "[stackmaps] strip: llvm-strip"
  cp "${tmp}/a_policy" "${tmp}/a_policy.llvm_strip"
  "${LLVM_STRIP}" "${tmp}/a_policy.llvm_strip"
  must_have_stackmaps "${tmp}/a_policy.llvm_strip"
  must_have_faultmaps "${tmp}/a_policy.llvm_strip"
else
  echo "[stackmaps] note: llvm-strip not found; skipping llvm-strip check"
fi

if [[ -n "${LLVM_READOBJ}" ]]; then
  echo "[stackmaps] inspect: llvm-readobj --symbols"
  # Do NOT use `grep -q` here: under `set -o pipefail`, `grep -q` can close the
  # pipe early once a match is found, causing `llvm-readobj` to see EPIPE and
  # exit non-zero (flaky failure depending on scheduling/buffering).
  "${LLVM_READOBJ}" --symbols "${tmp}/a_policy" \
    | grep -E '__start_llvm_stackmaps|__stop_llvm_stackmaps' >/dev/null
else
  echo "[stackmaps] note: llvm-readobj not found; skipping llvm-readobj check"
fi

echo "[stackmaps] ok"
