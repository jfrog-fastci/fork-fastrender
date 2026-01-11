#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ecma_rs_root="$(cd "${script_dir}/.." && pwd)"

target_dir="${CARGO_TARGET_DIR:-${ecma_rs_root}/target}"

echo "Building runtime-native (release)..." >&2
cd "${ecma_rs_root}"
bash scripts/cargo_agent.sh build --release -p runtime-native

lib_path="${target_dir}/release/libruntime_native.a"
if [[ ! -f "${lib_path}" ]]; then
  echo "error: expected staticlib at ${lib_path}" >&2
  exit 1
fi

include_dir="${ecma_rs_root}/runtime-native/include"

cat <<EOF
runtime-native artifacts:
  staticlib: ${lib_path}
  include:   ${include_dir}

Example (C99):
  cc -std=c99 -I "${include_dir}" /path/to/program.c "${lib_path}" -o program
EOF
