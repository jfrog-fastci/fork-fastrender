#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ecma_rs_root="$(cd "${script_dir}/.." && pwd)"

target_dir="${CARGO_TARGET_DIR:-${ecma_rs_root}/target}"

# The runtime-native crate requires frame pointers for FP-based stack walking / GC root enumeration.
# (Enforced by `runtime-native/build.rs`.) Inject the flag here so the helper script works out of
# the box even when the caller isn't using `scripts/cargo_agent.sh` / `scripts/cargo_llvm.sh`.
if [[ "${RUSTFLAGS:-}" != *"force-frame-pointers=yes"* ]]; then
  if [[ -z "${RUSTFLAGS:-}" ]]; then
    export RUSTFLAGS="-C force-frame-pointers=yes"
  else
    export RUSTFLAGS="${RUSTFLAGS} -C force-frame-pointers=yes"
  fi
fi

echo "Building runtime-native (release)..." >&2
cd "${ecma_rs_root}"
# Use the standard cargo wrapper (slot + RLIMIT_AS caps). `scripts/cargo_agent.sh` also injects
# `-C force-frame-pointers=yes` for runtime-native, so we don't need to require LLVM 18 tooling
# just to build the Rust runtime library.
bash scripts/cargo_agent.sh build --release -p runtime-native

lib_path="${target_dir}/release/libruntime_native.a"
if [[ ! -f "${lib_path}" ]]; then
  echo "error: expected staticlib at ${lib_path}" >&2
  exit 1
fi

include_dir="${ecma_rs_root}/runtime-native/include"
stackmaps_ld="${ecma_rs_root}/runtime-native/link/stackmaps.ld"
if [[ ! -f "${stackmaps_ld}" ]]; then
  stackmaps_ld="${ecma_rs_root}/runtime-native/stackmaps.ld"
fi
linker_script_line=""
linker_script_flag=""
if [[ "$(uname -s)" == "Linux" ]]; then
  linker_script_line="  linker-script: ${stackmaps_ld}"
  linker_script_flag=" -Wl,-T,${stackmaps_ld}"
fi

cat <<EOF
runtime-native artifacts:
  staticlib: ${lib_path}
  include:   ${include_dir}
${linker_script_line}

Example (C99):
  cc -std=c99 -I "${include_dir}" /path/to/program.c "${lib_path}"${linker_script_flag} -o program
EOF
