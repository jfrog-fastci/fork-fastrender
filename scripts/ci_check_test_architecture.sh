#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

if ! command -v rg >/dev/null 2>&1; then
  echo "error: rg (ripgrep) is required (used for CI test-architecture guardrails)" >&2
  echo "hint: install ripgrep (e.g. 'brew install ripgrep' or 'apt-get install ripgrep')" >&2
  exit 1
fi

if [[ ! -d tests ]]; then
  echo "error: missing tests/ directory at repo root" >&2
  exit 1
fi

if [[ ! -f Cargo.toml ]]; then
  echo "error: missing Cargo.toml at repo root" >&2
  exit 1
fi

allowed_test_binaries=(
  "tests/allocation_failure.rs"
  "tests/integration.rs"
)

mapfile -t found_test_binaries < <(find tests -maxdepth 1 -type f -name '*.rs' -print | sort)

missing=()
for allowed in "${allowed_test_binaries[@]}"; do
  if [[ ! -f "${allowed}" ]]; then
    missing+=("${allowed}")
  fi
done

extra=()
for found in "${found_test_binaries[@]}"; do
  is_allowed=0
  for allowed in "${allowed_test_binaries[@]}"; do
    if [[ "${found}" == "${allowed}" ]]; then
      is_allowed=1
      break
    fi
  done
  if [[ "${is_allowed}" -eq 0 ]]; then
    extra+=("${found}")
  fi
done

if [[ "${#missing[@]}" -ne 0 || "${#extra[@]}" -ne 0 ]]; then
  echo "error: integration test binaries must be exactly:" >&2
  for allowed in "${allowed_test_binaries[@]}"; do
    echo "  - ${allowed}" >&2
  done
  echo >&2
  echo "found (tests/*.rs):" >&2
  if [[ "${#found_test_binaries[@]}" -eq 0 ]]; then
    echo "  - <none>" >&2
  else
    for found in "${found_test_binaries[@]}"; do
      echo "  - ${found}" >&2
    done
  fi
  echo >&2
  if [[ "${#missing[@]}" -ne 0 ]]; then
    echo "missing expected test binaries:" >&2
    for path in "${missing[@]}"; do
      echo "  - ${path}" >&2
    done
    echo >&2
  fi
  if [[ "${#extra[@]}" -ne 0 ]]; then
    echo "unexpected extra test binaries (each tests/*.rs is a separate integration-test binary):" >&2
    for path in "${extra[@]}"; do
      echo "  - ${path}" >&2
    done
    echo >&2
  fi
  echo "hint: add integration tests as modules under tests/ and include them from tests/integration.rs" >&2
  echo "hint: unit tests belong in src/ (run with: cargo test --lib)" >&2
  exit 1
fi

shim_matches="$(rg -n '#\[path\s*=\s*"' tests || true)"
if [[ -n "${shim_matches}" ]]; then
  echo "error: found #[path = \"...\"] shims under tests/ (these create extra test binaries):" >&2
  echo "${shim_matches}" >&2
  echo >&2
  echo "hint: delete the shim and include the module normally via mod.rs + tests/integration.rs" >&2
  exit 1
fi

cargo_test_entries="$(rg -n '^\s*\[\[test\]\]' Cargo.toml || true)"
if [[ -n "${cargo_test_entries}" ]]; then
  echo "error: root Cargo.toml contains [[test]] entries (hidden integration-test binaries):" >&2
  echo "${cargo_test_entries}" >&2
  echo >&2
  echo "hint: remove [[test]] entries; integration tests must live under tests/integration.rs (plus allocation_failure.rs)" >&2
  exit 1
fi
