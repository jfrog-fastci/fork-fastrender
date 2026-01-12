#!/usr/bin/env bash
set -euo pipefail

# Keep the pinned TypeScript version in sync across:
# - Rust bundled libs (`typecheck-ts/build.rs`)
# - Node harness oracle (`typecheck-ts-harness/package.json` + `package-lock.json`)
# - Vendored `.d.ts` lib files (`typecheck-ts/fixtures/typescript-libs/<ver>/`)
#
# This is enforced in CI to prevent “drift” where baselines/libs are generated
# with one TypeScript version but the Rust build points at another.

json_runtime=""
if command -v python3 >/dev/null 2>&1; then
  json_runtime="python3"
elif command -v node >/dev/null 2>&1; then
  json_runtime="node"
else
  echo "error: python3 or node is required for TypeScript version sync checks" >&2
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

if [[ "$json_runtime" == "python3" ]]; then
  rust_version="$(
    python3 - <<'PY'
import re
from pathlib import Path

path = Path("typecheck-ts/build.rs")
text = path.read_text(encoding="utf-8")
matches = re.findall(r'const\s+TYPESCRIPT_VERSION:\s*&str\s*=\s*"([^"]+)"\s*;', text)
if len(matches) != 1:
  raise SystemExit(f"error: expected exactly 1 TYPESCRIPT_VERSION in {path}, found {len(matches)}")
print(matches[0])
PY
  )"

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
elif [[ "$json_runtime" == "node" ]]; then
  rust_version="$(
    node - <<'NODE'
const fs = require("fs");

const text = fs.readFileSync("typecheck-ts/build.rs", "utf8");
const re = /const\s+TYPESCRIPT_VERSION:\s*&str\s*=\s*"([^"]+)"\s*;/g;
const matches = [...text.matchAll(re)].map((m) => m[1]);
if (matches.length !== 1) {
  console.error(
    `error: expected exactly 1 TYPESCRIPT_VERSION in typecheck-ts/build.rs, found ${matches.length}`,
  );
  process.exit(1);
}
process.stdout.write(matches[0]);
NODE
  )"

  harness_package_json_version="$(node -p "require('./${package_json}').dependencies.typescript")"
  harness_package_lock_root_version="$(node -p "require('./${package_lock}').packages[''].dependencies.typescript")"
  harness_package_lock_module_version="$(node -p "require('./${package_lock}').packages['node_modules/typescript'].version")"
else
  echo "error: unreachable: unknown json runtime '${json_runtime}'" >&2
  exit 2
fi

if [[ -z "${rust_version}" ]]; then
  echo "error: failed to read TYPESCRIPT_VERSION from ${build_rs}" >&2
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
