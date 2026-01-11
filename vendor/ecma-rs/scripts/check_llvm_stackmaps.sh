#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Guardrail: this repo standardizes on LLVM's production GC strategy name (`coreclr`).
# LLVM's demo/reference GC strategy ("statepoint-" + "example") is intentionally *not* used here;
# allowing it to creep back into non-doc fixtures makes it easy to accidentally generate
# inconsistent IR.
#
# Keep this check in the lightweight CI path (it runs before any LLVM work below).
gc_demo_strategy="statepoint-"
gc_demo_strategy="${gc_demo_strategy}example"
ecma_root="$(cd "${script_dir}/.." && pwd)"

# IMPORTANT: Do NOT scan the entire `vendor/ecma-rs/` tree.
# Developers sometimes init the heavyweight nested corpora submodules
# (`parse-js/tests/TypeScript`, `test262*/data`, ...) and `grep -R` over the whole
# workspace can take minutes.
guard_paths=()
for p in \
  "${ecma_root}/native-js" \
  "${ecma_root}/runtime-native" \
  "${ecma_root}/runtime-native-abi" \
  "${ecma_root}/llvm-stackmaps" \
  "${ecma_root}/stackmap" \
  "${ecma_root}/stackmap-context" \
  "${ecma_root}/scripts"; do
  if [[ -d "${p}" ]]; then
    guard_paths+=("${p}")
  fi
done

if ((${#guard_paths[@]} > 0)) && grep -R --line-number --binary-files=without-match --exclude='*.md' \
  --include='*.rs' --include='*.ll' --include='*.c' --include='*.h' --include='*.S' --include='*.sh' \
  "${gc_demo_strategy}" "${guard_paths[@]}"; then
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

"${CLANG}" -c -O0 -o "${tmp}/mod_a.o" "${tmp}/mod_a.ll"
"${CLANG}" -c -O0 -o "${tmp}/mod_b.o" "${tmp}/mod_b.ll"
"${CLANG}" -c -O0 -o "${tmp}/main.o" "${tmp}/main.ll"
"${CLANG}" -c -O0 -o "${tmp}/callee.o" "${tmp}/callee.c"

objs=("${tmp}/main.o" "${tmp}/mod_a.o" "${tmp}/mod_b.o" "${tmp}/callee.o")

must_have_stackmaps() {
  local bin="$1"
  local line
  line="$(
    "${READELF}" -W -S "${bin}" \
      | awk '$2==".data.rel.ro.llvm_stackmaps" || $2==".llvm_stackmaps" {print $0}' \
      | head -n 1
  )"
  if [[ -z "${line}" ]]; then
    echo "expected stackmaps section (.data.rel.ro.llvm_stackmaps or .llvm_stackmaps) in: ${bin}" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi

  # readelf columns: [Nr] Name Type Address Off Size ES Flags Link Info Align
  local sec_name sec_size_hex
  sec_name="$(awk '{print $2}' <<<"${line}")"
  sec_size_hex="$(awk '{print $6}' <<<"${line}")"
  local sec_size_dec=$((16#${sec_size_hex}))
  if [[ "${sec_size_dec}" -le 0 ]]; then
    echo "expected non-empty ${sec_name} in: ${bin} (size=0x${sec_size_hex})" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi
}

must_not_have_stackmaps() {
  local bin="$1"
  if "${READELF}" -W -S "${bin}" | awk '$2==".data.rel.ro.llvm_stackmaps" || $2==".llvm_stackmaps" {found=1} END {exit !found}'; then
    echo "expected no stackmaps section in: ${bin}" >&2
    "${READELF}" -W -S "${bin}" >&2 || true
    exit 1
  fi
}

must_have_textrel() {
  local bin="$1"
  if ! "${READELF}" -d "${bin}" 2>/dev/null | grep -q "TEXTREL"; then
    echo "expected DT_TEXTREL in: ${bin}" >&2
    "${READELF}" -d "${bin}" >&2 || true
    exit 1
  fi
}

must_not_have_textrel() {
  local bin="$1"
  if "${READELF}" -d "${bin}" 2>/dev/null | grep -q "TEXTREL"; then
    echo "expected no DT_TEXTREL in: ${bin}" >&2
    "${READELF}" -d "${bin}" >&2 || true
    exit 1
  fi
}

echo "[stackmaps] link: ld (no-pie, no gc-sections)"
"${CLANG}" -no-pie -o "${tmp}/a_ld_nogc" "${objs[@]}"
must_have_stackmaps "${tmp}/a_ld_nogc"

echo "[stackmaps] link: ld (pie) => EXPECTED DT_TEXTREL"
if "${CLANG}" -pie -o "${tmp}/a_ld_pie_textrel" "${objs[@]}"; then
  must_have_stackmaps "${tmp}/a_ld_pie_textrel"
  must_have_textrel "${tmp}/a_ld_pie_textrel"
else
  echo "[stackmaps] note: ld PIE link failed; skipping DT_TEXTREL check" >&2
fi

echo "[stackmaps] link: ld (pie, patched stackmaps) => EXPECTED NO DT_TEXTREL"
cp "${tmp}/mod_a.o" "${tmp}/mod_a.pie.o"
cp "${tmp}/mod_b.o" "${tmp}/mod_b.pie.o"
"${OBJCOPY}" --set-section-flags .llvm_stackmaps=alloc,load,contents,data "${tmp}/mod_a.pie.o"
"${OBJCOPY}" --set-section-flags .llvm_stackmaps=alloc,load,contents,data "${tmp}/mod_b.pie.o"
if "${CLANG}" -pie -o "${tmp}/a_ld_pie_no_textrel" "${tmp}/main.o" "${tmp}/mod_a.pie.o" "${tmp}/mod_b.pie.o" "${tmp}/callee.o"; then
  must_have_stackmaps "${tmp}/a_ld_pie_no_textrel"
  must_not_have_textrel "${tmp}/a_ld_pie_no_textrel"
else
  echo "[stackmaps] note: ld PIE link failed; skipping patched PIE check" >&2
fi

echo "[stackmaps] link: ld (no-pie, --gc-sections) => EXPECTED DROP"
"${CLANG}" -no-pie -Wl,--gc-sections -o "${tmp}/a_ld_gc" "${objs[@]}"
must_not_have_stackmaps "${tmp}/a_ld_gc"

echo "[stackmaps] link: ld (no-pie, --gc-sections + stackmaps.ld KEEP)"
"${CLANG}" -no-pie -Wl,--gc-sections -Wl,-T,"${script_dir}/../runtime-native/stackmaps.ld" \
  -o "${tmp}/a_ld_policy" "${objs[@]}"
must_have_stackmaps "${tmp}/a_ld_policy"

echo "[stackmaps] link: native_link.sh (no-pie, --gc-sections + KEEP)"
"${script_dir}/native_link.sh" -o "${tmp}/a_policy" "${objs[@]}"
must_have_stackmaps "${tmp}/a_policy"

echo "[stackmaps] link: native_link.sh (ld explicit)"
ECMA_RS_NATIVE_LINKER=ld "${script_dir}/native_link.sh" -o "${tmp}/a_policy_ld" "${objs[@]}"
must_have_stackmaps "${tmp}/a_policy_ld"

if [[ -n "${LLD_FUSE}" ]]; then
  echo "[stackmaps] link: lld (no-pie, no gc-sections)"
  "${CLANG}" -fuse-ld="${LLD_FUSE}" -no-pie -o "${tmp}/a_lld_nogc" "${objs[@]}"
  must_have_stackmaps "${tmp}/a_lld_nogc"

  echo "[stackmaps] link: lld (pie, unpatched) => EXPECTED FAIL"
  if "${CLANG}" -fuse-ld="${LLD_FUSE}" -pie -o "${tmp}/a_lld_pie_unpatched" "${objs[@]}"; then
    echo "[stackmaps] warning: lld PIE link unexpectedly succeeded; ensuring no DT_TEXTREL" >&2
    must_not_have_textrel "${tmp}/a_lld_pie_unpatched"
  else
    echo "[stackmaps] ok: lld rejected PIE without stackmaps patching (expected)"
  fi

  echo "[stackmaps] link: lld (no-pie, --gc-sections) => EXPECTED DROP"
  "${CLANG}" -fuse-ld="${LLD_FUSE}" -no-pie -Wl,--gc-sections -o "${tmp}/a_lld_gc" "${objs[@]}"
  must_not_have_stackmaps "${tmp}/a_lld_gc"

  echo "[stackmaps] link: lld (no-pie, --gc-sections + stackmaps.ld KEEP)"
  "${CLANG}" -fuse-ld="${LLD_FUSE}" -no-pie -Wl,--gc-sections -Wl,-T,"${script_dir}/../runtime-native/stackmaps.ld" \
    -o "${tmp}/a_lld_policy" "${objs[@]}"
  must_have_stackmaps "${tmp}/a_lld_policy"

  echo "[stackmaps] link: native_link.sh (lld explicit)"
  ECMA_RS_NATIVE_LINKER=lld "${script_dir}/native_link.sh" -o "${tmp}/a_policy_lld" "${objs[@]}"
  must_have_stackmaps "${tmp}/a_policy_lld"

  if [[ -n "${LLVM_OBJCOPY}" ]]; then
    echo "[stackmaps] link: native_link.sh (lld + PIE; stackmaps patched via llvm-objcopy)"
    ECMA_RS_NATIVE_LINKER=lld ECMA_RS_NATIVE_PIE=1 "${script_dir}/native_link.sh" -o "${tmp}/a_policy_lld_pie" "${objs[@]}"
    must_have_stackmaps "${tmp}/a_policy_lld_pie"
    must_not_have_textrel "${tmp}/a_policy_lld_pie"
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

echo "[stackmaps] strip: objcopy --strip-unneeded"
cp "${tmp}/a_policy" "${tmp}/a_policy.objcopy_strip_unneeded"
"${OBJCOPY}" --strip-unneeded "${tmp}/a_policy.objcopy_strip_unneeded"
must_have_stackmaps "${tmp}/a_policy.objcopy_strip_unneeded"

echo "[stackmaps] strip: native_strip.sh"
cp "${tmp}/a_policy" "${tmp}/a_policy.native_strip"
"${script_dir}/native_strip.sh" "${tmp}/a_policy.native_strip"
must_have_stackmaps "${tmp}/a_policy.native_strip"

if [[ -n "${LLVM_STRIP}" ]]; then
  echo "[stackmaps] strip: llvm-strip"
  cp "${tmp}/a_policy" "${tmp}/a_policy.llvm_strip"
  "${LLVM_STRIP}" "${tmp}/a_policy.llvm_strip"
  must_have_stackmaps "${tmp}/a_policy.llvm_strip"
else
  echo "[stackmaps] note: llvm-strip not found; skipping llvm-strip check"
fi

if [[ -n "${LLVM_READOBJ}" ]]; then
  echo "[stackmaps] inspect: llvm-readobj --sections"
  "${LLVM_READOBJ}" --sections "${tmp}/a_policy" | grep -Eq 'Name: \.data\.rel\.ro\.llvm_stackmaps|Name: \.llvm_stackmaps'
else
  echo "[stackmaps] note: llvm-readobj not found; skipping llvm-readobj check"
fi

echo "[stackmaps] ok"
