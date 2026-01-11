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

need_cmd() {
  local cmd="$1"
  command -v "$cmd" >/dev/null 2>&1 || die "missing required command: $cmd"
}

need_cmd llc
need_cmd llvm-readobj
need_cmd llvm-objdump

if [[ "$(uname -m)" != "x86_64" ]]; then
  echo "skipping: expected x86_64, got $(uname -m)" >&2
  exit 0
fi

llc_version_line="$(llc --version | head -n1 || true)"
if [[ "${llc_version_line}" != *"version 18."* ]]; then
  die "expected LLVM 18.x (llc), got: ${llc_version_line}"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECMA_RS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
FIXTURES_DIR="${ECMA_RS_DIR}/fixtures/llvm_stackmap_abi"

IR_A="${FIXTURES_DIR}/gc_statepoint_patch_bytes_0_flags_0.ll"
IR_B="${FIXTURES_DIR}/gc_statepoint_patch_bytes_16_flags_2.ll"

[[ -f "${IR_A}" ]] || die "missing fixture: ${IR_A}"
[[ -f "${IR_B}" ]] || die "missing fixture: ${IR_B}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "${tmpdir}"' EXIT

OBJ_A="${tmpdir}/a.o"
OBJ_B="${tmpdir}/b.o"

run_llc() {
  local in="$1"
  local out="$2"
  local err="${tmpdir}/$(basename "${out}").llc.err"
  if ! llc -O0 -filetype=obj "${in}" -o "${out}" 2>"${err}"; then
    echo "llc failed for: ${in}" >&2
    cat "${err}" >&2
    exit 1
  fi
}

run_llc "${IR_A}" "${OBJ_A}"
run_llc "${IR_B}" "${OBJ_B}"

STACKMAP_A="$(llvm-readobj --stackmap "${OBJ_A}")"
STACKMAP_B="$(llvm-readobj --stackmap "${OBJ_B}")"

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
  val="$(printf '%s\n' "${stackmap}" | awk '/#2: Constant/ {print $3; exit}')"
  val="${val%,}"
  [[ "${val}" =~ ^[0-9]+$ ]] || die "failed to parse '#2: Constant' from llvm-readobj output"
  printf '%s' "${val}"
}

OFF_A="$(extract_instruction_offset "${STACKMAP_A}")"
OFF_B="$(extract_instruction_offset "${STACKMAP_B}")"

FLAGS_B_EXPECTED=2
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

DIS_A="$(llvm-objdump -d "${OBJ_A}")"
DIS_B="$(llvm-objdump -d "${OBJ_B}")"

if ! grep -Eq '\bcallq\b' <<<"${DIS_A}"; then
  echo "${DIS_A}" >&2
  die "expected fixture A to contain a direct call at the statepoint site (patch_bytes=0)"
fi

if grep -Eq '\bcallq\b' <<<"${DIS_B}"; then
  echo "${DIS_B}" >&2
  die "expected fixture B to contain no direct call at the statepoint site (patch_bytes>0 should emit a NOP sled on x86_64)"
fi

if ! grep -Eq '\bnop' <<<"${DIS_B}"; then
  echo "${DIS_B}" >&2
  die "expected fixture B to contain NOP padding at the statepoint site"
fi

# Guard LLVM 18 verifier behaviour: only bits 0 and 1 are accepted (flags 0..3).
INVALID_FLAGS_IR="${tmpdir}/invalid_flags.ll"
cat >"${INVALID_FLAGS_IR}" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)

define void @test(ptr %obj) gc "statepoint-example" {
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
if llc -O0 -filetype=obj "${INVALID_FLAGS_IR}" -o "${INVALID_OBJ}" 2>"${INVALID_ERR}"; then
  echo "unexpectedly accepted flags=4; expected LLVM 18 verifier to reject unknown bits" >&2
  llvm-readobj --stackmap "${INVALID_OBJ}" >&2 || true
  die "flags validation changed (expected flags >= 4 to be rejected on LLVM 18)"
fi
if ! grep -Eq 'unknown flag used' "${INVALID_ERR}"; then
  echo "llc rejected flags=4, but error did not match expected text:" >&2
  cat "${INVALID_ERR}" >&2
  die "unexpected verifier error message for invalid flags"
fi

echo "ok: gc.statepoint flags/patch_bytes behaviour matches LLVM 18 expectations (A_off=${OFF_A}, B_off=${OFF_B}, delta=${DELTA})"
