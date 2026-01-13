#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Guardrail 0) Never allow unresolved merge conflict markers in vendored Rust sources.
# This is intentionally run before the test-architecture gating logic so it is never skipped.
bash scripts/check_no_conflict_markers.sh

doc_ref="instructions/test_cleanup.md"

if [[ ! -d tests ]]; then
  echo "error: missing tests/ directory at repo root" >&2
  exit 1
fi

if [[ ! -f Cargo.toml ]]; then
  echo "error: missing Cargo.toml at repo root" >&2
  exit 1
fi

# `tests/integration.rs` is the unified integration-test harness. Even while the broader test
# cleanup is in-flight, we want to fail fast if it disappears.
if [[ ! -f tests/integration.rs ]]; then
  echo "error: missing tests/integration.rs (unified integration-test harness)" >&2
  echo "see: ${doc_ref}" >&2
  exit 1
fi

have_rg=0
if command -v rg >/dev/null 2>&1; then
  have_rg=1
fi

# Check 4) Never allow `#[path = "..."]` shims under `tests/`.
#
# The broader test-binary consolidation is still in-flight, but `#[path]` shims are always wrong
# (they create duplicate binaries / module graphs). Enforce this invariant unconditionally so we
# don't regress while other migrations are ongoing.
if [[ "${have_rg}" -eq 1 ]]; then
  # Only Rust sources can contain `#[path = "..."]`; avoid scanning large fixture trees (WPT, HTML
  # fixtures, etc.) so this check stays fast even as test data grows.
  #
  # Anchor to the start of the line (with optional whitespace) so docs/doc-comments that mention the
  # pattern don't trigger false positives.
  shim_matches="$(
    rg -n \
      --glob '*.rs' \
      --glob '!tests/pages/**' \
      --glob '!tests/fonts/**' \
      --glob '!tests/fuzz_corpus/**' \
      --glob '!tests/wpt_dom/**' \
      --glob '!tests/wpt/tests/**' \
      --glob '!tests/wpt/expected/**' \
      '^\s*#\[\s*path\s*=\s*"' \
      tests || true
  )"
else
  shim_matches="$(grep -RInE --include='*.rs' '^[[:space:]]*#\\[[[:space:]]*path[[:space:]]*=[[:space:]]*"' tests || true)"
fi
if [[ -n "${shim_matches}" ]]; then
  echo "error: found #[path = \"...\"] shims under tests/ (these create extra test binaries):" >&2
  echo "${shim_matches}" >&2
  echo >&2
  echo "see: ${doc_ref}" >&2
  echo "hint: delete the shim and include the module normally via mod.rs + tests/integration.rs" >&2
  exit 1
fi

# Check 3) Always forbid hidden integration-test binaries via Cargo.toml [[test]] entries.
# This can regress independently of the unified-harness migration.
if [[ "${have_rg}" -eq 1 ]]; then
  cargo_test_entries="$(rg -n '^\s*\[\[test\]\]' Cargo.toml || true)"
else
  cargo_test_entries="$(grep -nE '^[[:space:]]*\\[\\[test\\]\\]' Cargo.toml || true)"
fi
if [[ -n "${cargo_test_entries}" ]]; then
  echo "error: root Cargo.toml contains [[test]] entries (hidden integration-test binaries):" >&2
  echo "${cargo_test_entries}" >&2
  echo >&2
  echo "see: ${doc_ref}" >&2
  echo "hint: remove [[test]] entries; integration tests must live under tests/integration.rs (plus allocation_failure.rs)" >&2
  exit 1
fi

# Check 2) Unified harness files must be pure module lists.
#
# `tests/integration.rs` exists to *consolidate* the integration-test module tree into a single
# binary. Reintroducing `#[test] fn ...` wrappers (or other helper functions) in the harness file
# brings back the "top-level harness code" pattern we are deleting (see Rule 5 in
# `instructions/test_cleanup.md`).
#
# Run focused subsets via the standard test filter instead of creating new `tests/*.rs` binaries:
#   bash scripts/cargo_agent.sh test --test integration ui::appearance_settings
harness_files=(
  "tests/integration.rs"
)
if [[ -f tests/allocation_failure.rs ]]; then
  harness_files+=("tests/allocation_failure.rs")
fi

if [[ "${have_rg}" -eq 1 ]]; then
  harness_test_matches="$(rg -n '^\s*#\[\s*(tokio::)?test\s*\]' "${harness_files[@]}" || true)"
else
  harness_test_matches="$(grep -nE '^[[:space:]]*#\\[[[:space:]]*(tokio::)?test[[:space:]]*\\]' "${harness_files[@]}" || true)"
fi

harness_bad_lines="$(
  awk '
  {
    line = $0
    trimmed = line
    sub(/^[ \t]+/, "", trimmed)

    if (trimmed == "") next
    if (trimmed ~ /^\/\//) next
    if (trimmed ~ /^\/\*/) next
    if (trimmed ~ /^\*/) next
    if (trimmed ~ /^#/) next
    if (trimmed ~ /^(pub(\([^)]*\))?[ \t]+)?mod[ \t]+[A-Za-z0-9_#]+[ \t]*;/) next

    print FILENAME ":" NR ":" line
  }
  ' "${harness_files[@]}"
)"

integration_mod_dups="$(
  awk '
  {
    line = $0
    sub(/^[ \t]+/, "", line)
    if (line ~ /^(pub(\([^)]*\))?[ \t]+)?mod[ \t]+[A-Za-z0-9_#]+[ \t]*;/) {
      sub(/^(pub(\([^)]*\))?[ \t]+)?mod[ \t]+/, "", line)
      sub(/[ \t]*;.*/, "", line)
      sub(/^r#/, "", line)
      counts[line]++
    }
  }
  END {
    for (name in counts) {
      if (counts[name] > 1) print name
    }
  }
  ' tests/integration.rs | sort
)"

if [[ -n "${harness_test_matches}" || -n "${harness_bad_lines}" || -n "${integration_mod_dups}" ]]; then
  echo "error: unified integration harness files must be pure module lists:" >&2
  echo "  - tests/integration.rs (and tests/allocation_failure.rs) should be module declarations only" >&2
  echo >&2
  if [[ -n "${harness_test_matches}" ]]; then
    echo "found test attributes in harness files:" >&2
    echo "${harness_test_matches}" >&2
    echo >&2
  fi
  if [[ -n "${harness_bad_lines}" ]]; then
    echo "found non-module code in harness files:" >&2
    echo "${harness_bad_lines}" >&2
    echo >&2
  fi
  if [[ -n "${integration_mod_dups}" ]]; then
    echo "found duplicate mod declarations in tests/integration.rs:" >&2
    echo "${integration_mod_dups}" >&2
    echo >&2
  fi
  echo "see: ${doc_ref}" >&2
  echo "hint: move the test body into a module under tests/ and include it from tests/integration.rs" >&2
  echo "hint: run subsets via filters, e.g.:" >&2
  echo "  bash scripts/cargo_agent.sh test --test integration ui::appearance_settings" >&2
  exit 1
fi

allowed_test_binaries=(
  "tests/allocation_failure.rs"
  "tests/integration.rs"
)

found_test_binaries=()
while IFS= read -r path; do
  found_test_binaries+=("${path}")
done < <(find tests -maxdepth 1 -type f -name '*.rs' -print | sort)

# The strict 2-binary + no-shims checks are only valid once the test-cleanup migration lands.
#
# To avoid permanently breaking CI while the migration is still in-flight, we only enforce once:
# - `tests/allocation_failure.rs` exists (final harness name), OR
# - the top-level harness count has dropped low enough that we should not silently accept a
#   near-final-but-wrong layout (e.g. `allocation_failure_tests.rs`).
#
# Keep this threshold conservative so intermediate cleanups can land without being forced to
# complete the entire migration in one PR.
if [[ ! -f tests/allocation_failure.rs && "${#found_test_binaries[@]}" -gt 3 ]]; then
  echo "info: test-cleanup migration still in-flight; skipping strict test-architecture checks" >&2
  echo "info: (found ${#found_test_binaries[@]} top-level tests/*.rs files; strict checks activate once tests/allocation_failure.rs exists or tests/*.rs count <= 3)" >&2
  echo "info: see ${doc_ref}" >&2
  exit 0
fi

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
  echo "see: ${doc_ref}" >&2
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
  echo "hint: unit tests belong in src/ (run with: bash scripts/cargo_agent.sh test --quiet -p fastrender --lib)" >&2
  exit 1
fi
