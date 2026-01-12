#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
usage: ./scripts/bump_typescript_version.sh <version>

Automates the mechanical parts of bumping the pinned TypeScript version:

  - Updates `typecheck-ts/build.rs` (TYPESCRIPT_VERSION)
  - Updates `typecheck-ts/src/resolve/ts_node.rs` (TypeScriptVersion::default)
  - Vendors `lib*.d.ts` into `typecheck-ts/fixtures/typescript-libs/<version>/`
  - Updates `typecheck-ts-harness/package.json`
  - Refreshes `typecheck-ts-harness/package-lock.json` via npm

This script does NOT regenerate difftsc baselines or conformance snapshots.
Follow `typecheck-ts-harness/docs/bumping_typescript.md` after running it.

Requirements:
  - npm (network access to the npm registry)
  - tar
  - python3

Notes:
  - This script currently expects a stable `x.y.z` TypeScript version string.
EOF
}

if [[ $# -ne 1 ]] || [[ "${1:-}" == "-h" ]] || [[ "${1:-}" == "--help" ]]; then
  usage
  exit 2
fi

new_version="$1"

if ! [[ "$new_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: invalid TypeScript version string: '$new_version'" >&2
  echo "help: expected something like '5.10.0' (semver)" >&2
  exit 1
fi

for cmd in npm tar python3; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: missing required command: $cmd" >&2
    exit 1
  fi
done

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

export NEW_TYPESCRIPT_VERSION="$new_version"

python3 - <<'PY'
import os
import re
from pathlib import Path

new_version = os.environ["NEW_TYPESCRIPT_VERSION"]
major, minor, patch = new_version.split(".")

build_rs = Path("typecheck-ts/build.rs")
text = build_rs.read_text(encoding="utf-8")
updated, count = re.subn(
    r'(const\s+TYPESCRIPT_VERSION:\s*&str\s*=\s*")[^"]+("\s*;)',
    rf"\g<1>{new_version}\g<2>",
    text,
)
if count != 1:
  raise SystemExit(f"error: expected exactly 1 TYPESCRIPT_VERSION in {build_rs}, found {count}")
build_rs.write_text(updated, encoding="utf-8")

package_json = Path("typecheck-ts-harness/package.json")
text = package_json.read_text(encoding="utf-8")
updated, count = re.subn(
    r'("typescript"\s*:\s*")[^"]+(")',
    rf"\g<1>{new_version}\g<2>",
    text,
)
if count != 1:
  raise SystemExit(f"error: expected exactly 1 typescript dependency entry in {package_json}, found {count}")
package_json.write_text(updated, encoding="utf-8")

resolver_rs = Path("typecheck-ts/src/resolve/ts_node.rs")
text = resolver_rs.read_text(encoding="utf-8")
updated, count = re.subn(
    r"TypeScriptVersion::new\(\s*\d+\s*,\s*\d+\s*,\s*\d+\s*\)",
    f"TypeScriptVersion::new({major}, {minor}, {patch})",
    text,
)
if count != 1:
  raise SystemExit(
      f"error: expected exactly 1 TypeScriptVersion::new(M,m,p) in {resolver_rs}, found {count}"
  )
resolver_rs.write_text(updated, encoding="utf-8")
PY

tmp="$(mktemp -d)"
cleanup() {
  rm -rf "$tmp"
}
trap cleanup EXIT

(
  cd "$tmp"
  npm pack "typescript@${new_version}" >/dev/null
  tarball="$(ls -1 typescript-*.tgz | head -n 1)"
  if [[ -z "$tarball" ]]; then
    echo "error: npm pack did not produce a typescript-*.tgz tarball" >&2
    exit 1
  fi
  tar -xzf "$tarball"
)

src_dir="${tmp}/package"
if [[ ! -d "$src_dir/lib" ]]; then
  echo "error: unexpected npm tarball layout: missing ${src_dir}/lib/" >&2
  exit 1
fi

dest_dir="typecheck-ts/fixtures/typescript-libs/${new_version}"
if [[ -e "$dest_dir" ]]; then
  echo "error: destination already exists: ${dest_dir}" >&2
  echo "help: delete it first if you want to re-vendor the libs" >&2
  exit 1
fi

mkdir -p "$dest_dir"

for f in LICENSE.txt ThirdPartyNoticeText.txt; do
  if [[ -f "${src_dir}/${f}" ]]; then
    cp "${src_dir}/${f}" "${dest_dir}/${f}"
  fi
done

shopt -s nullglob
lib_files=("${src_dir}"/lib/lib*.d.ts)
shopt -u nullglob

if [[ ${#lib_files[@]} -eq 0 ]]; then
  echo "error: no lib*.d.ts files found under ${src_dir}/lib/" >&2
  exit 1
fi

cp "${lib_files[@]}" "$dest_dir/"

cat >"${dest_dir}/README.md" <<EOF
# TypeScript standard library declarations (${new_version})

This directory vendors the upstream TypeScript \`lib*.d.ts\` files from the
official \`typescript@${new_version}\` npm package (the contents of \`typescript/lib/\`).

These files are used by \`typecheck-ts\` when built with the \`bundled-libs\`
feature so tests and offline runs can typecheck against the real TypeScript
standard library without relying on a local Node installation.

License information:

- \`LICENSE.txt\` (TypeScript, Apache 2.0)
- \`ThirdPartyNoticeText.txt\`
EOF

(
  cd typecheck-ts-harness
  npm install --ignore-scripts
)

./scripts/check_typescript_version_sync.sh

echo "ok: bumped pinned TypeScript version to ${new_version}"
echo "next: regenerate difftsc baselines + verify/update conformance snapshots:"
echo "  see typecheck-ts-harness/docs/bumping_typescript.md"
