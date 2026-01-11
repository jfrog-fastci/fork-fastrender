#!/usr/bin/env bash
set -euo pipefail

# Regression test: ensure we can link a PIE binary containing LLVM `.llvm_stackmaps`
# without requiring `-Wl,-z,notext` / DT_TEXTREL.
#
# This exercises the Linux AOT linking policy implemented by:
# - `scripts/native_js_link_linux.sh` (objcopy rewrite + lld PIE link)
# - `runtime-native/link/stackmaps.ld` (KEEP + exported stackmap range symbols)
#   (`runtime-native/stackmaps.ld` is a compat alias)

if [[ "${OSTYPE:-}" != linux* ]]; then
  echo "skipping: Linux-only (OSTYPE=${OSTYPE:-unknown})" >&2
  exit 0
fi

for tool in clang-18 llvm-objcopy-18 readelf; do
  if ! command -v "${tool}" >/dev/null 2>&1; then
    echo "skipping: missing ${tool} in PATH" >&2
    exit 0
  fi
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmpdir="$(mktemp -d)"
cleanup() { rm -rf "${tmpdir}"; }
trap cleanup EXIT

cat > "${tmpdir}/codegen.ll" <<'EOF'
; ModuleID = 'codegen'
target triple = "x86_64-pc-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @foo() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0)
  ret void
}
EOF

cat > "${tmpdir}/main.c" <<'EOF'
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

extern const unsigned char __fastr_stackmaps_start[];
extern const unsigned char __fastr_stackmaps_end[];

extern void foo(void);

static int contains_u64(const unsigned char* bytes, size_t len, uint64_t needle) {
  unsigned char tmp[8];
  memcpy(tmp, &needle, 8);
  for (size_t i = 0; i + 8 <= len; i++) {
    if (memcmp(bytes + i, tmp, 8) == 0) {
      return 1;
    }
  }
  return 0;
}

int main(void) {
  size_t size = (size_t)(__fastr_stackmaps_end - __fastr_stackmaps_start);
  if (size == 0) {
    fprintf(stderr, "empty .llvm_stackmaps (likely GC'd by the linker)\n");
    return 1;
  }

  unsigned version = (unsigned)__fastr_stackmaps_start[0];
  if (version != 3) {
    fprintf(stderr, "unexpected stackmap version: %u\n", version);
    return 2;
  }

  // PIE correctness: `.llvm_stackmaps` function addresses must be relocated at runtime.
  // Ensure the in-memory stackmaps bytes contain the actual relocated address of `foo`.
  uint64_t foo_addr = (uint64_t)(uintptr_t)(void*)&foo;
  if (!contains_u64(__fastr_stackmaps_start, size, foo_addr)) {
    fprintf(stderr, "stackmaps missing relocated foo address: %p\n", (void*)(uintptr_t)foo_addr);
    return 3;
  }

  foo();
  printf("stackmaps: version=%u size=%zu\n", version, size);
  return 0;
}
EOF

clang-18 -c "${tmpdir}/codegen.ll" -o "${tmpdir}/codegen.o"
clang-18 -c "${tmpdir}/main.c" -o "${tmpdir}/main.o"

out="${tmpdir}/app"
bash "${repo_root}/scripts/native_js_link_linux.sh" --out "${out}" -- "${tmpdir}/main.o" "${tmpdir}/codegen.o"

# Ensure output is PIE (ET_DYN).
readelf -h "${out}" | grep -E 'Type:[[:space:]]+DYN' >/dev/null || {
  echo "error: expected PIE ET_DYN output" >&2
  readelf -h "${out}" | sed -n '1,40p' >&2 || true
  exit 1
}

# Ensure no DT_TEXTREL.
if readelf -d "${out}" | grep TEXTREL >/dev/null; then
  echo "error: unexpected DT_TEXTREL in linked PIE output" >&2
  readelf -d "${out}" >&2 || true
  exit 1
fi

output="$("${out}")"
echo "${output}"
echo "${output}" | grep -q 'stackmaps: version=3' || {
  echo "error: expected stackmaps version output, got: ${output}" >&2
  exit 1
}
