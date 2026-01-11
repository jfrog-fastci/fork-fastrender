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

  // Linkers may insert alignment padding before the first blob, so don't assume the
  // first byte in the range is the StackMap header.
  unsigned version = 0;
  for (size_t i = 0; i < size; i++) {
    unsigned b = (unsigned)__fastr_stackmaps_start[i];
    if (b != 0) {
      version = b;
      break;
    }
  }
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

# Ensure no RWX LOAD segment.
if readelf -l "${out}" | grep "RWE" >/dev/null; then
  echo "error: unexpected RWX LOAD segment in linked PIE output" >&2
  readelf -l "${out}" >&2 || true
  exit 1
fi

# Ensure stackmaps are not placed into a standalone RW PT_LOAD segment: locate the output section
# that contains `__start_llvm_stackmaps` and ensure its PT_LOAD segment also contains `.dynamic`.
segments="$(readelf -W -l "${out}")"
start_hex="$(readelf -W -s "${out}" | awk '$8=="__start_llvm_stackmaps" { print $2; exit }')"
stop_hex="$(readelf -W -s "${out}" | awk '$8=="__stop_llvm_stackmaps" { print $2; exit }')"
if [[ -z "${start_hex}" ]]; then
  echo "error: missing __start_llvm_stackmaps in PIE output" >&2
  readelf -W -s "${out}" >&2 || true
  exit 1
fi
if [[ -z "${stop_hex}" ]]; then
  echo "error: missing __stop_llvm_stackmaps in PIE output" >&2
  readelf -W -s "${out}" >&2 || true
  exit 1
fi
start_dec=$((16#${start_hex}))
stop_dec=$((16#${stop_hex}))

stackmaps_section=""
while read -r name addr_hex size_hex; do
  addr_dec=$((16#${addr_hex}))
  size_dec=$((16#${size_hex}))
  if (( size_dec == 0 )); then
    continue
  fi
  end_dec=$((addr_dec + size_dec))
  if (( addr_dec <= start_dec && start_dec < end_dec )); then
    stackmaps_section="${name}"
    break
  fi
done < <(
  readelf -W -S "${out}" | awk '
    $1 == "[" { print $3, $5, $7; next }
    $1 ~ /^[[][0-9]+[]]$/ { print $2, $4, $6; next }
  '
)

if [[ -z "${stackmaps_section}" ]]; then
  echo "error: failed to find a section containing __start_llvm_stackmaps (0x${start_hex})" >&2
  readelf -W -S "${out}" >&2 || true
  exit 1
fi

stackmaps_seg_sections="$(
  printf '%s\n' "${segments}" | awk -v target="${stackmaps_section}" '
    $0 ~ /Section to Segment mapping:/ { in_map=1; next }
    !in_map { next }
    $1 == "Segment" { next }
    $1 ~ /^[0-9]+$/ {
      if (seg != "") {
        if (found) { print acc; exit }
      }
      seg = $1
      acc = ""
      found = 0
      for (i = 2; i <= NF; i++) {
        acc = acc $i " "
        if ($i == target) found = 1
      }
      next
    }
    {
      if (seg == "") next
      for (i = 1; i <= NF; i++) {
        acc = acc $i " "
        if ($i == target) found = 1
      }
    }
    END { if (found) print acc }'
)"
if [[ -z "${stackmaps_seg_sections}" ]]; then
  echo "error: failed to locate stackmaps section (${stackmaps_section}) in readelf segment mapping" >&2
  echo "${segments}" >&2
  exit 1
fi
if [[ " ${stackmaps_seg_sections} " != *" .dynamic "* ]]; then
  echo "error: expected stackmaps section (${stackmaps_section}) PT_LOAD segment to also contain .dynamic" >&2
  echo "  segment sections: ${stackmaps_seg_sections}" >&2
  echo "${segments}" >&2
  exit 1
fi

# Ensure the stackmaps range is protected by RELRO (PT_GNU_RELRO), so the bytes become read-only
# after dynamic relocations are applied.
relro_ok=0
while read -r relro_vaddr_hex relro_memsz_hex; do
  relro_vaddr_hex="${relro_vaddr_hex#0x}"
  relro_memsz_hex="${relro_memsz_hex#0x}"
  relro_vaddr_dec=$((16#${relro_vaddr_hex}))
  relro_memsz_dec=$((16#${relro_memsz_hex}))
  relro_end_dec=$((relro_vaddr_dec + relro_memsz_dec))
  if (( relro_vaddr_dec <= start_dec && stop_dec <= relro_end_dec )); then
    relro_ok=1
    break
  fi
done < <(printf '%s\n' "${segments}" | awk '$1=="GNU_RELRO" { print $3, $6 }')

if [[ "${relro_ok}" -ne 1 ]]; then
  echo "error: expected stackmaps range to be covered by PT_GNU_RELRO" >&2
  echo "  stackmaps: start=0x${start_hex} stop=0x${stop_hex}" >&2
  echo "${segments}" >&2
  exit 1
fi

output="$("${out}")"
echo "${output}"
echo "${output}" | grep 'stackmaps: version=3' >/dev/null || {
  echo "error: expected stackmaps version output, got: ${output}" >&2
  exit 1
}
