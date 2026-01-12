#!/usr/bin/env bash
set -euo pipefail

# Regression test: `scripts/cargo_agent.sh xtask ...` must not execute an existing (stale)
# `target/debug/xtask` binary when the `cargo build -p xtask --bin xtask` step fails.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

mkdir -p "${repo_root}/target"
tmp_target_dir="$(mktemp -d "${repo_root}/target/cargo_agent_xtask_stale_test.XXXXXX")"
cleanup() {
  rm -rf "${tmp_target_dir}"
}
trap cleanup EXIT

exe_suffix=""
case "${OSTYPE:-}" in
  msys*|cygwin*|win32*) exe_suffix=".exe" ;;
esac

xtask_bin="${tmp_target_dir}/debug/xtask${exe_suffix}"
mkdir -p "$(dirname "${xtask_bin}")"

marker="__cargo_agent_stale_xtask_executed__"
cat >"${xtask_bin}" <<EOF
#!/usr/bin/env bash
echo "${marker}"
exit 0
EOF
chmod +x "${xtask_bin}"

set +e
output="$(
  CARGO_TARGET_DIR="${tmp_target_dir}" \
  RUSTFLAGS="--this-flag-does-not-exist" \
  bash "${repo_root}/scripts/cargo_agent.sh" xtask --help 2>&1
)"
status=$?
set -e

if [[ "${status}" -eq 0 ]]; then
  echo "error: expected non-zero exit status from xtask wrapper when build fails" >&2
  echo "${output}" >&2
  exit 1
fi

if printf '%s\n' "${output}" | grep -Fq "${marker}"; then
  echo "error: wrapper executed stale xtask binary even though build failed" >&2
  echo "${output}" >&2
  exit 1
fi

echo "ok: wrapper failed fast and did not run stale xtask binary" >&2
