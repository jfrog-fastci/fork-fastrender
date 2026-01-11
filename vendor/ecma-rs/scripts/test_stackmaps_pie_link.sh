#!/usr/bin/env bash
set -euo pipefail

# Regression test: ensure we can link a PIE binary containing LLVM `.llvm_stackmaps`
# without requiring `-Wl,-z,notext` / DT_TEXTREL.
#
# This exercises the Linux AOT linking policy implemented by:
# - `scripts/native_js_link_linux.sh` (objcopy rewrite + lld PIE link)
# - `runtime-native/stackmaps.ld`      (KEEP + exported stackmap range symbols)

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

extern const unsigned char __fastr_stackmaps_start[];
extern const unsigned char __fastr_stackmaps_end[];

extern void foo(void);

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
readelf -h "${out}" | grep -qE 'Type:[[:space:]]+DYN' || {
  echo "error: expected PIE ET_DYN output" >&2
  readelf -h "${out}" | sed -n '1,40p' >&2 || true
  exit 1
}

# Ensure no DT_TEXTREL.
if readelf -d "${out}" | grep -q TEXTREL; then
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

