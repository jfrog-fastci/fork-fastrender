#!/usr/bin/env bash
set -euo pipefail

# Sanity-check the LLVM 18 statepoint fixture referenced by docs:
#   docs/llvm_statepoints_llvm18.md
#
# Verifies:
#   - fixture path is correct
#   - `llvm-as` can assemble it (verifier-correct IR)
#   - `llc -filetype=obj` produces an object containing `.llvm_stackmaps`

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

DOC_MD="${REPO_ROOT}/docs/llvm_statepoints_llvm18.md"
FIXTURE_REL="fixtures/llvm_stackmap_abi/statepoint.ll"
FIXTURE_LL="${REPO_ROOT}/${FIXTURE_REL}"

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

LLVM_AS="$(find_llvm_tool llvm-as)" || true
LLC="$(find_llvm_tool llc)" || true
LLVM_READOBJ="$(find_llvm_tool llvm-readobj)" || true

[[ -n "${LLVM_AS}" ]] || fail "llvm-as (LLVM 18) not found in PATH"
[[ -n "${LLC}" ]] || fail "llc (LLVM 18) not found in PATH"
[[ -n "${LLVM_READOBJ}" ]] || fail "llvm-readobj (LLVM 18) not found in PATH"

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

require_llvm18 "${LLVM_AS}"
require_llvm18 "${LLC}"
require_llvm18 "${LLVM_READOBJ}"

[[ -f "${FIXTURE_LL}" ]] || fail "fixture not found: ${FIXTURE_LL}"

# Keep temp files under target/ so we don't dirty the working tree.
TMP_BASE="${REPO_ROOT}/target"
mkdir -p "${TMP_BASE}"
TMP_DIR="$(mktemp -d "${TMP_BASE}/llvm-statepoint-fixture.XXXXXX")"
cleanup() {
  rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

BC="${TMP_DIR}/statepoint.bc"
OBJ="${TMP_DIR}/statepoint.o"

"${LLVM_AS}" "${FIXTURE_LL}" -o "${BC}"
LLC_BIN="${LLC}" bash "${SCRIPT_DIR}/llc_fp.sh" -filetype=obj "${BC}" -o "${OBJ}"

# Do not use `grep -q` under `set -o pipefail`: early pipe closure can cause
# `llvm-readobj` to hit EPIPE/SIGPIPE and return non-zero.
if ! "${LLVM_READOBJ}" --sections "${OBJ}" | grep -F ".llvm_stackmaps" >/dev/null; then
  "${LLVM_READOBJ}" --sections "${OBJ}" >&2 || true
  fail "expected .llvm_stackmaps section in output object: ${OBJ}"
fi

if [[ -f "${DOC_MD}" ]] && ! grep -qF "${FIXTURE_REL}" "${DOC_MD}"; then
  fail "doc ${DOC_MD} does not reference expected fixture path: ${FIXTURE_REL}"
fi

echo "ok: LLVM statepoint fixture assembles and produces .llvm_stackmaps"
