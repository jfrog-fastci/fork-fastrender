#!/usr/bin/env bash
set -euo pipefail

# Regression test for the (undocumented) stackmap ABI assumptions this project
# relies on for precise GC root scanning.
#
# Verifies on LLVM 18:
#   - StackMap "instruction offset" is the RETURN ADDRESS (next instruction after call)
#   - Spilled roots are based off caller-frame SP:
#       x86_64: DWARF reg 7  (RSP)
#       AArch64: DWARF reg 31 (SP)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
FIXTURE_LL="${REPO_ROOT}/fixtures/llvm_stackmap_abi/statepoint.ll"

find_llvm_tool() {
  local name="$1"

  # Prefer versioned binaries when present (Ubuntu packages often provide both).
  if command -v "${name}-18" >/dev/null 2>&1; then
    echo "${name}-18"
    return 0
  fi
  if command -v "${name}" >/dev/null 2>&1; then
    echo "${name}"
    return 0
  fi

  echo ""
  return 1
}

fail() {
  echo "error: $*" >&2
  exit 1
}

LLC="$(find_llvm_tool llc)" || true
LLVM_READOBJ="$(find_llvm_tool llvm-readobj)" || true
LLVM_OBJDUMP="$(find_llvm_tool llvm-objdump)" || true

[[ -n "${LLC}" ]] || fail "llc (LLVM 18) not found in PATH"
[[ -n "${LLVM_READOBJ}" ]] || fail "llvm-readobj (LLVM 18) not found in PATH"
[[ -n "${LLVM_OBJDUMP}" ]] || fail "llvm-objdump (LLVM 18) not found in PATH"

require_llvm18() {
  local tool="$1"
  local out
  out="$("${tool}" --version 2>/dev/null || true)"

  # Some LLVM builds print the version on line 1 ("Ubuntu LLVM version 18.1.x"),
  # others print it on line 2 ("LLVM (http://llvm.org/):" then "LLVM version 18.1.x").
  if ! grep -Eq 'version 18\.' <<<"${out}"; then
    fail "expected LLVM 18.x (${tool}), got: $(echo "${out}" | head -n2 | tr '\n' ' ')"
  fi
}

require_llvm18 "${LLC}"
require_llvm18 "${LLVM_READOBJ}"
require_llvm18 "${LLVM_OBJDUMP}"

[[ -f "${FIXTURE_LL}" ]] || fail "fixture not found: ${FIXTURE_LL}"

# Keep temp files under target/ so we don't dirty the working tree.
TMP_BASE="${REPO_ROOT}/target"
mkdir -p "${TMP_BASE}"
TMP_DIR="$(mktemp -d "${TMP_BASE}/stackmap-abi.XXXXXX")"
cleanup() {
  rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

extract_stackmap_instruction_offset() {
  # Expected line (LLVM 18.1.3):
  #   Record ID: 0, instruction offset: 10
  local stackmap_out="$1"
  local off
  off="$(
    printf '%s\n' "${stackmap_out}" |
      awk -F 'instruction offset: ' '/Record ID:/ {print $2; exit}' |
      awk '{print $1}'
  )"

  [[ -n "${off}" ]] || return 1
  printf '%s\n' "${off}"
}

extract_call_return_offset_from_objdump() {
  local objdump_out="$1"
  local func="$2"
  local call_re="$3"

  local start_hex
  start_hex="$(
    printf '%s\n' "${objdump_out}" |
      awk -v f="${func}" '$0 ~ "<"f">:" {print $1; exit}'
  )"
  [[ -n "${start_hex}" ]] || return 1

  local next_hex
  next_hex="$(
    printf '%s\n' "${objdump_out}" |
      awk -v f="${func}" -v call_re="${call_re}" '
        $0 ~ "<"f">:" {in_func=1; next}
        in_func && /^[0-9a-fA-F]+ <.*>:/ {exit}
        in_func && $1 ~ /^[0-9a-fA-F]+:$/ {
          addr=$1
          sub(":", "", addr)
          if (want_next) {print addr; exit}
          if ($0 ~ call_re) {want_next=1}
        }
      '
  )"
  [[ -n "${next_hex}" ]] || return 1

  printf '%d\n' "$((0x${next_hex} - 0x${start_hex}))"
}

run_case() {
  local triple="$1"
  local case_name="$2"
  local call_re="$3"
  local expected_indirect_re="$4"

  local obj="${TMP_DIR}/${case_name}.o"
  LLC_BIN="${LLC}" bash "${SCRIPT_DIR}/llc_fp.sh" -O0 -filetype=obj \
    --fixup-allow-gcptr-in-csr=false --fixup-max-csr-statepoints=0 \
    -mtriple="${triple}" "${FIXTURE_LL}" -o "${obj}"

  local stackmap_out objdump_out
  stackmap_out="$("${LLVM_READOBJ}" --stackmap "${obj}")"
  objdump_out="$("${LLVM_OBJDUMP}" -d "${obj}")"

  local instr_off_str
  if ! instr_off_str="$(extract_stackmap_instruction_offset "${stackmap_out}")"; then
    echo "${stackmap_out}" >&2
    fail "[${case_name}] unable to parse stackmap instruction offset from llvm-readobj output"
  fi

  local instr_off
  instr_off="$((instr_off_str))"

  local return_off
  if ! return_off="$(extract_call_return_offset_from_objdump "${objdump_out}" "stackmap_abi_test" "${call_re}")"; then
    echo "${objdump_out}" >&2
    fail "[${case_name}] unable to find call instruction + following instruction in disassembly for stackmap_abi_test"
  fi

  if [[ "${instr_off}" -ne "${return_off}" ]]; then
    echo "=== [${case_name}] llvm-readobj --stackmap ===" >&2
    echo "${stackmap_out}" >&2
    echo "" >&2
    echo "=== [${case_name}] llvm-objdump -d (snippet) ===" >&2
    printf '%s\n' "${objdump_out}" | awk '
      /<stackmap_abi_test>:/ {in_func=1; c=0}
      in_func && c < 20 {print; c++}
      in_func && c >= 20 {exit}
    ' >&2
    echo "" >&2
    fail "[${case_name}] stackmap instruction offset mismatch: expected return address offset ${return_off}, got ${instr_off}"
  fi

  if ! printf '%s\n' "${stackmap_out}" | grep -Eq "${expected_indirect_re}"; then
    echo "=== [${case_name}] llvm-readobj --stackmap ===" >&2
    echo "${stackmap_out}" >&2
    echo "" >&2
    fail "[${case_name}] expected at least one spilled root location matching /${expected_indirect_re}/"
  fi
}

run_case \
  "x86_64-unknown-linux-gnu" \
  "x86_64" \
  '[[:space:]]call' \
  'Indirect \[(R#7|RSP) \+'

run_case \
  "aarch64-unknown-linux-gnu" \
  "aarch64" \
  '[[:space:]]bl[[:space:]]' \
  'Indirect \[(R#31|SP) \+'

echo "ok: LLVM statepoint stackmap ABI (x86_64 + aarch64)"
