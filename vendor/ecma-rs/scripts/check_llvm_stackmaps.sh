#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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

LLD_PATH="$(command -v ld.lld || command -v ld.lld-18 || true)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "${tmp}"; }
trap cleanup EXIT

cat >"${tmp}/mod_a.ll" <<'EOF'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

declare token @llvm.experimental.gc.statepoint.p0(i64 immarg, i32 immarg, ptr, i32 immarg, i32 immarg, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32 immarg, i32 immarg)

define ptr addrspace(1) @fooA(ptr addrspace(1) %obj) gc "statepoint-example" {
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

define ptr addrspace(1) @fooB(ptr addrspace(1) %obj) gc "statepoint-example" {
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
  if ! "${READELF}" -S "${bin}" | grep -qF ".llvm_stackmaps"; then
    echo "expected .llvm_stackmaps in: ${bin}" >&2
    "${READELF}" -S "${bin}" >&2 || true
    exit 1
  fi
}

must_not_have_stackmaps() {
  local bin="$1"
  if "${READELF}" -S "${bin}" | grep -qF ".llvm_stackmaps"; then
    echo "expected no .llvm_stackmaps in: ${bin}" >&2
    "${READELF}" -S "${bin}" >&2 || true
    exit 1
  fi
}

echo "[stackmaps] link: ld (no-pie, no gc-sections)"
"${CLANG}" -no-pie -o "${tmp}/a_ld_nogc" "${objs[@]}"
must_have_stackmaps "${tmp}/a_ld_nogc"

echo "[stackmaps] link: ld (no-pie, --gc-sections) => EXPECTED DROP"
"${CLANG}" -no-pie -Wl,--gc-sections -o "${tmp}/a_ld_gc" "${objs[@]}"
must_not_have_stackmaps "${tmp}/a_ld_gc"

echo "[stackmaps] link: native_link.sh default (no-pie, --gc-sections + KEEP)"
"${script_dir}/native_link.sh" -o "${tmp}/a_policy" "${objs[@]}"
must_have_stackmaps "${tmp}/a_policy"

if [[ -n "${LLD_PATH}" ]]; then
  echo "[stackmaps] link: lld (no-pie, no gc-sections)"
  ln -sf "${LLD_PATH}" "${tmp}/ld.lld"
  PATH="${tmp}:${PATH}" "${CLANG}" -fuse-ld=lld -no-pie -o "${tmp}/a_lld_nogc" "${objs[@]}"
  must_have_stackmaps "${tmp}/a_lld_nogc"

  echo "[stackmaps] link: lld (no-pie, --gc-sections) => EXPECTED DROP"
  PATH="${tmp}:${PATH}" "${CLANG}" -fuse-ld=lld -no-pie -Wl,--gc-sections -o "${tmp}/a_lld_gc" "${objs[@]}"
  must_not_have_stackmaps "${tmp}/a_lld_gc"
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

if [[ -n "${LLVM_STRIP}" ]]; then
  echo "[stackmaps] strip: llvm-strip"
  cp "${tmp}/a_policy" "${tmp}/a_policy.llvm_strip"
  "${LLVM_STRIP}" "${tmp}/a_policy.llvm_strip"
  must_have_stackmaps "${tmp}/a_policy.llvm_strip"
else
  echo "[stackmaps] note: llvm-strip not found; skipping llvm-strip check"
fi

echo "[stackmaps] ok"
