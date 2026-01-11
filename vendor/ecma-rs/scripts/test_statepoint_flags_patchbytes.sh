#!/usr/bin/env bash
set -euo pipefail

# Regression test for LLVM 18 `gc.statepoint`:
# - `flags` (5th argument) is a 2-bit mask (0..3) and is recorded in the stackmap.
# - `patch_bytes` (2nd argument) controls whether LLVM emits an actual call
#   (`patch_bytes=0`) or a patchable NOP region (`patch_bytes>0`) and shifts the
#   stackmap instruction offset accordingly.

die() {
  echo "error: $*" >&2
  exit 1
}

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

LLC="$(find_llvm_tool llc)" || true
LLVM_READOBJ="$(find_llvm_tool llvm-readobj)" || true
LLVM_OBJDUMP="$(find_llvm_tool llvm-objdump)" || true

[[ -n "${LLC}" ]] || die "llc (LLVM 18) not found in PATH"
[[ -n "${LLVM_READOBJ}" ]] || die "llvm-readobj (LLVM 18) not found in PATH"
[[ -n "${LLVM_OBJDUMP}" ]] || die "llvm-objdump (LLVM 18) not found in PATH"

if [[ "$(uname -m)" != "x86_64" ]]; then
  echo "skipping: expected x86_64, got $(uname -m)" >&2
  exit 0
fi

require_llvm18() {
  local tool="$1"
  local out
  out="$("${tool}" --version 2>/dev/null || true)"

  # Some LLVM builds print the version on line 1 ("Ubuntu LLVM version 18.1.x"),
  # others print it on line 2 ("LLVM (http://llvm.org/):" then "LLVM version 18.1.x").
  if ! grep -Eq 'version 18\.' <<<"${out}"; then
    die "expected LLVM 18.x (${tool}), got: $(echo "${out}" | head -n2 | tr '\n' ' ')"
  fi
}

require_llvm18 "${LLC}"
require_llvm18 "${LLVM_READOBJ}"
require_llvm18 "${LLVM_OBJDUMP}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECMA_RS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
FIXTURES_DIR="${ECMA_RS_DIR}/fixtures/llvm_stackmap_abi"

IR_A="${FIXTURES_DIR}/gc_statepoint_patch_bytes_0_flags_0.ll"
IR_B="${FIXTURES_DIR}/gc_statepoint_patch_bytes_16_flags_2.ll"

[[ -f "${IR_A}" ]] || die "missing fixture: ${IR_A}"
[[ -f "${IR_B}" ]] || die "missing fixture: ${IR_B}"

# Keep temp files under target/ so we don't dirty the working tree.
TMP_BASE="${ECMA_RS_DIR}/target"
mkdir -p "${TMP_BASE}"
tmpdir="$(mktemp -d "${TMP_BASE}/statepoint-flags-patchbytes.XXXXXX")"
trap 'rm -rf "${tmpdir}"' EXIT

OBJ_A="${tmpdir}/a.o"
OBJ_B="${tmpdir}/b.o"

run_llc() {
  local in="$1"
  local out="$2"
  local err="${tmpdir}/$(basename "${out}").llc.err"
  if ! "${LLC}" -O0 -filetype=obj "${in}" -o "${out}" 2>"${err}"; then
    echo "llc failed for: ${in}" >&2
    cat "${err}" >&2
    exit 1
  fi
}

run_llc "${IR_A}" "${OBJ_A}"
run_llc "${IR_B}" "${OBJ_B}"

STACKMAP_A="$("${LLVM_READOBJ}" --stackmap "${OBJ_A}")"
STACKMAP_B="$("${LLVM_READOBJ}" --stackmap "${OBJ_B}")"

extract_call_return_offset_from_objdump() {
  local objdump_out="$1"
  local func="$2"
  local call_re="$3"

  local start_hex
  start_hex="$(
    printf '%s\n' "${objdump_out}" |
      awk -v f="${func}" '$0 ~ "<"f">:" {print $1; exit}'
  )"
  [[ -n "${start_hex}" ]] || die "failed to locate function header for ${func} in objdump output"

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
  [[ -n "${next_hex}" ]] || die "failed to locate call + following instruction in ${func} disassembly"

  printf '%d' "$((0x${next_hex} - 0x${start_hex}))"
}

extract_nop_region_offsets_ending_at() {
  local objdump_out="$1"
  local func="$2"
  local want_end_off="$3"

  local fn_start_hex
  fn_start_hex="$(
    printf '%s\n' "${objdump_out}" |
      awk -v f="${func}" '$0 ~ "<"f">:" {print $1; exit}'
  )"
  [[ -n "${fn_start_hex}" ]] || die "failed to locate function header for ${func} in objdump output"

  local runs
  runs="$(
    printf '%s\n' "${objdump_out}" |
      awk -v f="${func}" '
        $0 ~ "<"f">:" {in_func=1; next}
        in_func && /^[0-9a-fA-F]+ <.*>:/ {exit}
        in_func && $1 ~ /^[0-9a-fA-F]+:$/ {
          addr=$1
          sub(":", "", addr)
          inst=$2

          if (!in_nops && inst ~ /^nop/) {
            nop_start=addr
            in_nops=1
            next
          }
          if (in_nops && inst !~ /^nop/) {
            print nop_start " " addr
            in_nops=0
          }
        }
      '
  )"

  local start_hex end_hex start_off end_off
  while read -r start_hex end_hex; do
    [[ -n "${start_hex}" && -n "${end_hex}" ]] || continue
    start_off="$((0x${start_hex} - 0x${fn_start_hex}))"
    end_off="$((0x${end_hex} - 0x${fn_start_hex}))"
    if (( end_off == want_end_off )); then
      printf '%d %d' "${start_off}" "${end_off}"
      return 0
    fi
  done <<<"${runs}"

  echo "=== disassembly ===" >&2
  echo "${objdump_out}" >&2
  die "failed to find a NOP region in ${func} disassembly ending at offset ${want_end_off}"
}

extract_instruction_offset() {
  local stackmap="$1"
  local off
  off="$(printf '%s\n' "${stackmap}" | awk -F'instruction offset: ' '/instruction offset:/ {print $2; exit}')"
  off="$(printf '%s' "${off}" | tr -d ' ,')"
  [[ "${off}" =~ ^[0-9]+$ ]] || die "failed to parse instruction offset from llvm-readobj output"
  printf '%s' "${off}"
}

extract_location2_constant() {
  local stackmap="$1"
  local val
  val="$(
    printf '%s\n' "${stackmap}" | awk '
      /#2: Constant / {
        v=$3
        sub(/,/, "", v)
        print v
        exit
      }
      /#2: (ConstIndex|ConstantIndex) / {
        if (match($0, /\(([0-9-]+)\)/, m)) {
          print m[1]
          exit
        }
      }
    '
  )"
  [[ "${val}" =~ ^-?[0-9]+$ ]] || die "failed to parse '#2' constant value from llvm-readobj output"
  printf '%s' "${val}"
}

extract_location1_constant() {
  local stackmap="$1"
  local val
  val="$(
    printf '%s\n' "${stackmap}" | awk '
      /#1: Constant / {
        v=$3
        sub(/,/, "", v)
        print v
        exit
      }
      /#1: (ConstIndex|ConstantIndex) / {
        if (match($0, /\(([0-9-]+)\)/, m)) {
          print m[1]
          exit
        }
      }
    '
  )"
  [[ "${val}" =~ ^-?[0-9]+$ ]] || die "failed to parse '#1' constant value from llvm-readobj output"
  printf '%s' "${val}"
}

OFF_A="$(extract_instruction_offset "${STACKMAP_A}")"
OFF_B="$(extract_instruction_offset "${STACKMAP_B}")"

PATCH_BYTES_B=16
FLAGS_B_EXPECTED=2
FLAGS_A_EXPECTED=0
FLAGS_A_GOT="$(extract_location2_constant "${STACKMAP_A}")"
if [[ "${FLAGS_A_GOT}" != "${FLAGS_A_EXPECTED}" ]]; then
  echo "${STACKMAP_A}" >&2
  die "stackmap constant #2 (flags) mismatch for fixture A: expected ${FLAGS_A_EXPECTED}, got ${FLAGS_A_GOT}"
fi

FLAGS_B_GOT="$(extract_location2_constant "${STACKMAP_B}")"
if [[ "${FLAGS_B_GOT}" != "${FLAGS_B_EXPECTED}" ]]; then
  echo "${STACKMAP_B}" >&2
  die "stackmap constant #2 (flags) mismatch for fixture B: expected ${FLAGS_B_EXPECTED}, got ${FLAGS_B_GOT}"
fi

if (( OFF_B <= OFF_A )); then
  echo "=== stackmap A ===" >&2
  echo "${STACKMAP_A}" >&2
  echo "=== stackmap B ===" >&2
  echo "${STACKMAP_B}" >&2
  die "expected fixture B instruction offset > fixture A (patch_bytes should increase return-address offset); got A=${OFF_A}, B=${OFF_B}"
fi

DELTA=$((OFF_B - OFF_A))
MIN_DELTA=8 # patch_bytes=16, baseline call encoding may differ slightly across toolchains
if (( DELTA < MIN_DELTA )); then
  echo "=== stackmap A ===" >&2
  echo "${STACKMAP_A}" >&2
  echo "=== stackmap B ===" >&2
  echo "${STACKMAP_B}" >&2
  die "expected fixture B instruction offset to increase by at least ${MIN_DELTA}; got delta=${DELTA} (A=${OFF_A}, B=${OFF_B})"
fi

DIS_A="$("${LLVM_OBJDUMP}" -d --no-show-raw-insn "${OBJ_A}")"
DIS_B="$("${LLVM_OBJDUMP}" -d --no-show-raw-insn "${OBJ_B}")"

extract_function_body() {
  local objdump_out="$1"
  local func="$2"
  printf '%s\n' "${objdump_out}" |
    awk -v f="${func}" '
      $0 ~ "<"f">:" {in_func=1; print; next}
      in_func && /^[0-9a-fA-F]+ <.*>:/ {exit}
      in_func {print}
    '
}

DIS_A_TEST="$(extract_function_body "${DIS_A}" "test")"
DIS_B_TEST="$(extract_function_body "${DIS_B}" "test")"

if ! grep -Eq '[[:space:]]call' <<<"${DIS_A_TEST}"; then
  echo "${DIS_A}" >&2
  die "expected fixture A to contain a direct call at the statepoint site (patch_bytes=0)"
fi

if grep -Eq '[[:space:]]call' <<<"${DIS_B_TEST}"; then
  echo "${DIS_B}" >&2
  die "expected fixture B to contain no direct call at the statepoint site (patch_bytes>0 should emit a NOP sled on x86_64)"
fi

if ! grep -Eq '\bnop' <<<"${DIS_B_TEST}"; then
  echo "${DIS_B}" >&2
  die "expected fixture B to contain NOP padding at the statepoint site"
fi

# Stronger ABI checks:
# - Baseline instruction offset should match the call return address.
# - Patch-bytes instruction offset should match the end of the reserved NOP region, and the reserved
#   region should be at least PATCH_BYTES_B bytes.
RET_A="$(extract_call_return_offset_from_objdump "${DIS_A}" "test" '[[:space:]]call')"
if (( RET_A != OFF_A )); then
  echo "=== disassembly A ===" >&2
  echo "${DIS_A}" >&2
  echo "=== stackmap A ===" >&2
  echo "${STACKMAP_A}" >&2
  die "fixture A: expected stackmap instruction offset to equal call return address; got stackmap=${OFF_A}, disasm=${RET_A}"
fi

NOP_REGION="$(extract_nop_region_offsets_ending_at "${DIS_B}" "test" "${OFF_B}")"
read -r NOP_START_OFF NOP_END_OFF <<<"${NOP_REGION}"
NOP_LEN="$((NOP_END_OFF - NOP_START_OFF))"
if (( NOP_LEN < PATCH_BYTES_B )); then
  echo "=== disassembly B ===" >&2
  echo "${DIS_B}" >&2
  die "fixture B: expected contiguous NOP region length >= ${PATCH_BYTES_B}, got ${NOP_LEN} (nop_start_off=${NOP_START_OFF}, nop_end_off=${NOP_END_OFF})"
fi

# Ensure `flags=3` (both currently-valid bits) is accepted and recorded.
FLAGS3_IR="${tmpdir}/flags3.ll"
cat >"${FLAGS3_IR}" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)

define void @test(ptr %obj) gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 0, i32 0,
    ptr elementtype(void ()) @callee,
    i32 0, i32 3,
    i32 0, i32 0) [ "gc-live"(ptr %obj) ]
  ret void
}
EOF

FLAGS3_OBJ="${tmpdir}/flags3.o"
run_llc "${FLAGS3_IR}" "${FLAGS3_OBJ}"
FLAGS3_STACKMAP="$("${LLVM_READOBJ}" --stackmap "${FLAGS3_OBJ}")"
FLAGS3_GOT="$(extract_location2_constant "${FLAGS3_STACKMAP}")"
if [[ "${FLAGS3_GOT}" != "3" ]]; then
  echo "${FLAGS3_STACKMAP}" >&2
  die "flags=3 fixture: expected stackmap constant #2 (flags) to be 3, got ${FLAGS3_GOT}"
fi

# Verify stackmap constant #1 encodes the callsite calling convention (fastcc = 8).
CALLCONV_IR="${tmpdir}/callconv_fastcc.ll"
cat >"${CALLCONV_IR}" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)

define void @test(ptr %obj) gc "coreclr" {
entry:
  %tok = call fastcc token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 0, i32 0,
    ptr elementtype(void ()) @callee,
    i32 0, i32 0,
    i32 0, i32 0) [ "gc-live"(ptr %obj) ]
  ret void
}
EOF

CALLCONV_OBJ="${tmpdir}/callconv_fastcc.o"
run_llc "${CALLCONV_IR}" "${CALLCONV_OBJ}"
CALLCONV_STACKMAP="$("${LLVM_READOBJ}" --stackmap "${CALLCONV_OBJ}")"
CALLCONV_GOT="$(extract_location1_constant "${CALLCONV_STACKMAP}")"
if [[ "${CALLCONV_GOT}" != "8" ]]; then
  echo "${CALLCONV_STACKMAP}" >&2
  die "fastcc fixture: expected stackmap constant #1 (callconv) to be 8, got ${CALLCONV_GOT}"
fi

# Guard LLVM 18 verifier behaviour: only bits 0 and 1 are accepted (flags 0..3).
INVALID_FLAGS_IR="${tmpdir}/invalid_flags.ll"
cat >"${INVALID_FLAGS_IR}" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)

define void @test(ptr %obj) gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 0, i32 0,
    ptr elementtype(void ()) @callee,
    i32 0, i32 4,
    i32 0, i32 0) [ "gc-live"(ptr %obj) ]
  ret void
}
EOF

INVALID_OBJ="${tmpdir}/invalid_flags.o"
INVALID_ERR="${tmpdir}/invalid_flags.llc.err"
if "${LLC}" -O0 -filetype=obj "${INVALID_FLAGS_IR}" -o "${INVALID_OBJ}" 2>"${INVALID_ERR}"; then
  echo "unexpectedly accepted flags=4; expected LLVM 18 verifier to reject unknown bits" >&2
  "${LLVM_READOBJ}" --stackmap "${INVALID_OBJ}" >&2 || true
  die "flags validation changed (expected flags >= 4 to be rejected on LLVM 18)"
fi
if grep -Eq 'unknown flag used' "${INVALID_ERR}"; then
  :
elif grep -Eq 'gc\.statepoint|flag' "${INVALID_ERR}"; then
  echo "note: llc rejected flags=4 (as expected) but the verifier message changed:" >&2
  cat "${INVALID_ERR}" >&2
else
  echo "=== llc stderr for invalid flags snippet ===" >&2
  cat "${INVALID_ERR}" >&2
  die "llc failed for an unexpected reason while compiling invalid flags snippet"
fi

echo "ok: gc.statepoint flags/patch_bytes behaviour matches LLVM 18 expectations (A_off=${OFF_A}, B_off=${OFF_B}, delta=${DELTA})"
