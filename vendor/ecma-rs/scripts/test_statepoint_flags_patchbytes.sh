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
LLVM_AS="$(find_llvm_tool llvm-as)" || true
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
if [[ -n "${LLVM_AS}" ]]; then
  require_llvm18 "${LLVM_AS}"
fi
require_llvm18 "${LLVM_READOBJ}"
require_llvm18 "${LLVM_OBJDUMP}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECMA_RS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
FIXTURES_DIR="${ECMA_RS_DIR}/fixtures/llvm_stackmap_abi"

IR_A="${FIXTURES_DIR}/gc_statepoint_patch_bytes_0_flags_0.ll"
IR_B="${FIXTURES_DIR}/gc_statepoint_patch_bytes_16_flags_3.ll"

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
  if ! LLC_BIN="${LLC}" bash "${SCRIPT_DIR}/llc_fp.sh" -O0 -filetype=obj "${in}" -o "${out}" 2>"${err}"; then
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

extract_nop_region_offsets_containing() {
  local objdump_out="$1"
  local func="$2"
  local want_off="$3"

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
    # `end_off` is the first non-NOP instruction offset. Allow `want_off` to
    # equal `end_off` (meaning the NOP run ends exactly at the stackmap key),
    # or to fall inside the run (LLVM may emit extra NOPs beyond the reserved
    # patch region).
    if (( start_off <= want_off && want_off <= end_off )); then
      printf '%d %d' "${start_off}" "${end_off}"
      return 0
    fi
  done <<<"${runs}"

  echo "=== disassembly ===" >&2
  echo "${objdump_out}" >&2
  die "failed to find a NOP region in ${func} disassembly containing offset ${want_off}"
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

extract_location_size() {
  local stackmap="$1"
  local idx="$2"
  local size
  size="$(
    printf '%s\n' "${stackmap}" | awk -v idx="${idx}" '
      $1 == "#"idx":" {
        for (i = 1; i <= NF; i++) {
          if ($i == "size:") {
            v = $(i + 1)
            sub(/,/, "", v)
            print v
            exit
          }
        }
      }
    '
  )"
  [[ "${size}" =~ ^[0-9]+$ ]] || die "failed to parse location #${idx} size from llvm-readobj output"
  printf '%s' "${size}"
}

extract_location3_constant() {
  local stackmap="$1"
  local val
  val="$(
    printf '%s\n' "${stackmap}" | awk '
      /#3: Constant / {
        v=$3
        sub(/,/, "", v)
        print v
        exit
      }
      /#3: (ConstIndex|ConstantIndex) / {
        if (match($0, /\(([0-9-]+)\)/, m)) {
          print m[1]
          exit
        }
      }
    '
  )"
  [[ "${val}" =~ ^-?[0-9]+$ ]] || die "failed to parse '#3' constant value from llvm-readobj output"
  printf '%s' "${val}"
}

OFF_A="$(extract_instruction_offset "${STACKMAP_A}")"
OFF_B="$(extract_instruction_offset "${STACKMAP_B}")"

PATCH_BYTES_B=16
FLAGS_B_EXPECTED=3
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

DEOPT_A_GOT="$(extract_location3_constant "${STACKMAP_A}")"
DEOPT_B_GOT="$(extract_location3_constant "${STACKMAP_B}")"
if [[ "${DEOPT_A_GOT}" != "0" || "${DEOPT_B_GOT}" != "0" ]]; then
  echo "=== stackmap A ===" >&2
  echo "${STACKMAP_A}" >&2
  echo "=== stackmap B ===" >&2
  echo "${STACKMAP_B}" >&2
  die "expected statepoint header deopt_count location (#3) to be 0; got A.deopt_count=${DEOPT_A_GOT}, B.deopt_count=${DEOPT_B_GOT}"
fi

CALLCONV_A_SIZE="$(extract_location_size "${STACKMAP_A}" 1)"
CALLCONV_B_SIZE="$(extract_location_size "${STACKMAP_B}" 1)"
FLAGS_A_SIZE="$(extract_location_size "${STACKMAP_A}" 2)"
FLAGS_B_SIZE="$(extract_location_size "${STACKMAP_B}" 2)"
DEOPT_A_SIZE="$(extract_location_size "${STACKMAP_A}" 3)"
DEOPT_B_SIZE="$(extract_location_size "${STACKMAP_B}" 3)"
if [[ "${CALLCONV_A_SIZE}" != "8" ||
  "${CALLCONV_B_SIZE}" != "8" ||
  "${FLAGS_A_SIZE}" != "8" ||
  "${FLAGS_B_SIZE}" != "8" ||
  "${DEOPT_A_SIZE}" != "8" ||
  "${DEOPT_B_SIZE}" != "8" ]]; then
  echo "=== stackmap A ===" >&2
  echo "${STACKMAP_A}" >&2
  echo "=== stackmap B ===" >&2
  echo "${STACKMAP_B}" >&2
  die "expected statepoint header constant locations (#1/#2/#3) to report size: 8 on x86_64; got A(callconv=${CALLCONV_A_SIZE}, flags=${FLAGS_A_SIZE}, deopt_count=${DEOPT_A_SIZE}) B(callconv=${CALLCONV_B_SIZE}, flags=${FLAGS_B_SIZE}, deopt_count=${DEOPT_B_SIZE})"
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

NOP_REGION="$(extract_nop_region_offsets_containing "${DIS_B}" "test" "${OFF_B}")"
read -r NOP_START_OFF NOP_END_OFF <<<"${NOP_REGION}"
NOP_BYTES_BEFORE_OFF="$((OFF_B - NOP_START_OFF))"
if (( NOP_BYTES_BEFORE_OFF < PATCH_BYTES_B )); then
  echo "=== disassembly B ===" >&2
  echo "${DIS_B}" >&2
  die "fixture B: expected at least ${PATCH_BYTES_B} bytes of contiguous NOP padding immediately before stackmap instruction offset ${OFF_B}, got ${NOP_BYTES_BEFORE_OFF} (nop_start_off=${NOP_START_OFF}, nop_run_end_off=${NOP_END_OFF})"
fi

# Guard LLVM 18 verifier behaviour: only bits 0 and 1 are accepted (flags 0..3).
INVALID_FLAGS_IR="${tmpdir}/invalid_flags.ll"
cat >"${INVALID_FLAGS_IR}" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)

define void @test(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 0, i32 0,
    ptr elementtype(void ()) @callee,
    i32 0, i32 4,
    i32 0, i32 0) [ "gc-live"(ptr addrspace(1) %obj) ]
  ret void
}
EOF

INVALID_ERR="${tmpdir}/invalid_flags.err"
if [[ -n "${LLVM_AS}" ]]; then
  # Prefer `llvm-as` for this guard: it runs the IR verifier but avoids
  # full code generation, keeping the test fast.
  INVALID_BC="${tmpdir}/invalid_flags.bc"
  if "${LLVM_AS}" "${INVALID_FLAGS_IR}" -o "${INVALID_BC}" 2>"${INVALID_ERR}"; then
    die "flags validation changed (expected flags >= 4 to be rejected on LLVM 18)"
  fi
else
  INVALID_OBJ="${tmpdir}/invalid_flags.o"
  if "${LLC}" -O0 -filetype=obj "${INVALID_FLAGS_IR}" -o "${INVALID_OBJ}" 2>"${INVALID_ERR}"; then
    echo "unexpectedly accepted flags=4; expected LLVM 18 verifier to reject unknown bits" >&2
    "${LLVM_READOBJ}" --stackmap "${INVALID_OBJ}" >&2 || true
    die "flags validation changed (expected flags >= 4 to be rejected on LLVM 18)"
  fi
fi

if grep -Eq 'unknown flag used' "${INVALID_ERR}"; then
  :
elif grep -Eq 'gc\.statepoint|flag' "${INVALID_ERR}"; then
  echo "note: verifier rejected flags=4 (as expected) but the error message changed:" >&2
  cat "${INVALID_ERR}" >&2
else
  echo "=== verifier stderr for invalid flags snippet ===" >&2
  cat "${INVALID_ERR}" >&2
  die "verifier failed for an unexpected reason while checking invalid flags snippet"
fi

echo "ok: gc.statepoint flags/patch_bytes behaviour matches LLVM 18 expectations (A_off=${OFF_A}, B_off=${OFF_B}, delta=${DELTA})"
