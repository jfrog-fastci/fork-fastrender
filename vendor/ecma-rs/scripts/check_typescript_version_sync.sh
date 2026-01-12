#!/usr/bin/env bash
set -euo pipefail

# Keep the pinned TypeScript version in sync across:
# - Rust bundled libs (`typecheck-ts/build.rs`)
# - Node harness oracle (`typecheck-ts-harness/package.json` + `package-lock.json`)
# - Vendored `.d.ts` lib files (`typecheck-ts/fixtures/typescript-libs/<ver>/`)
#
# This is enforced in CI to prevent “drift” where baselines/libs are generated
# with one TypeScript version but the Rust build points at another.

if ! command -v rg >/dev/null 2>&1; then
  echo "error: rg (ripgrep) is required for TypeScript version sync checks" >&2
  exit 1
fi

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

build_rs="typecheck-ts/build.rs"
harness_dir="typecheck-ts-harness"
package_json="${harness_dir}/package.json"
package_lock="${harness_dir}/package-lock.json"

if [[ ! -f "$build_rs" ]]; then
  echo "error: missing ${build_rs} (expected to run from the ecma-rs repo root)" >&2
  exit 1
fi

if [[ ! -f "$package_json" ]]; then
  echo "error: missing ${package_json}" >&2
  exit 1
fi

if [[ ! -f "$package_lock" ]]; then
  echo "error: missing ${package_lock}" >&2
  exit 1
fi

rust_version_raw="$(
  rg --no-line-number --no-filename --pcre2 \
    'const\s+TYPESCRIPT_VERSION:\s*&str\s*=\s*"([^"]+)"\s*;' \
    "$build_rs" \
    --replace '$1'
)"

rust_version="$(printf '%s' "$rust_version_raw" | head -n 1)"

if [[ -z "$rust_version" ]]; then
  echo "error: failed to read TYPESCRIPT_VERSION from ${build_rs}" >&2
  exit 1
fi

if [[ "$(printf '%s\n' "$rust_version_raw" | wc -l | tr -d ' ')" != "1" ]]; then
  echo "error: expected exactly one TYPESCRIPT_VERSION in ${build_rs}, got:" >&2
  printf '%s\n' "$rust_version_raw" >&2
  exit 1
fi

if command -v python3 >/dev/null 2>&1; then
  harness_package_json_version="$(
    python3 - <<'PY'
import json
from pathlib import Path

data = json.loads(Path("typecheck-ts-harness/package.json").read_text(encoding="utf-8"))
print(data["dependencies"]["typescript"])
PY
  )"

  harness_package_lock_root_version="$(
    python3 - <<'PY'
import json
from pathlib import Path

data = json.loads(Path("typecheck-ts-harness/package-lock.json").read_text(encoding="utf-8"))
print(data["packages"][""]["dependencies"]["typescript"])
PY
  )"

  harness_package_lock_module_version="$(
    python3 - <<'PY'
import json
from pathlib import Path

data = json.loads(Path("typecheck-ts-harness/package-lock.json").read_text(encoding="utf-8"))
print(data["packages"]["node_modules/typescript"]["version"])
PY
  )"
elif command -v node >/dev/null 2>&1; then
  harness_package_json_version="$(node -p "require('./${package_json}').dependencies.typescript")"
  harness_package_lock_root_version="$(node -p "require('./${package_lock}').packages[''].dependencies.typescript")"
  harness_package_lock_module_version="$(node -p "require('./${package_lock}').packages['node_modules/typescript'].version")"
else
  echo "error: python3 or node is required to parse ${package_lock}" >&2
  exit 1
fi

errors=0

if [[ "$harness_package_json_version" != "$harness_package_lock_root_version" ]]; then
  echo "error: TypeScript version mismatch between:" >&2
  echo "  - ${package_json}:      ${harness_package_json_version}" >&2
  echo "  - ${package_lock} (root deps): ${harness_package_lock_root_version}" >&2
  echo "help: run \`cd ${harness_dir} && npm install --ignore-scripts\` to refresh package-lock.json" >&2
  errors=1
fi

if [[ "$harness_package_lock_root_version" != "$harness_package_lock_module_version" ]]; then
  echo "error: TypeScript version mismatch inside ${package_lock}:" >&2
  echo "  - packages[\"\"] deps:                 ${harness_package_lock_root_version}" >&2
  echo "  - packages[\"node_modules/typescript\"]: ${harness_package_lock_module_version}" >&2
  echo "help: re-run \`cd ${harness_dir} && npm install --ignore-scripts\` to refresh package-lock.json" >&2
  errors=1
fi

if [[ "$rust_version" != "$harness_package_lock_root_version" ]]; then
  echo "error: TypeScript version drift detected:" >&2
  echo "  - Rust (typecheck-ts/build.rs):              ${rust_version}" >&2
  echo "  - Node harness (typecheck-ts-harness/*lock*): ${harness_package_lock_root_version}" >&2
  echo "help: keep these in sync when bumping TypeScript." >&2
  echo "help: see typecheck-ts-harness/docs/bumping_typescript.md" >&2
  errors=1
fi

vendored_dir="typecheck-ts/fixtures/typescript-libs/${rust_version}"
if [[ ! -d "$vendored_dir" ]]; then
  echo "error: missing vendored TypeScript libs directory: ${vendored_dir}" >&2
  echo "help: vendor \`lib.*.d.ts\` from the TypeScript npm package into that directory." >&2
  echo "help: see typecheck-ts-harness/docs/bumping_typescript.md" >&2
  errors=1
fi

if [[ $errors -ne 0 ]]; then
  exit 1
fi

echo "ok: TypeScript versions are in sync (${rust_version})"
