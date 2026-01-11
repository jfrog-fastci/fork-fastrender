#!/usr/bin/env bash
set -euo pipefail

# Guard against new public APIs that accept raw byte buffers *as source text*.
# UTF-8 validation should happen at IO boundaries; once source code enters our
# Rust APIs it should be represented as `&str`/`Arc<str>`.
#
# IMPORTANT: This guard is intentionally scoped to crates whose public APIs
# accept source text (parser/typechecker/minifier/etc.). Many other crates in
# this workspace legitimately expose byte-oriented APIs for binary data (socket
# buffers, typed arrays, stackmap parsing, object/bitcode linking, ...); those
# are out of scope.

if ! command -v rg >/dev/null 2>&1; then
  echo "error: rg (ripgrep) is required for UTF-8 API checks" >&2
  exit 1
fi

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Only scan crates that take *source text* as an input.
#
# Note: `native-js` exposes some byte-oriented APIs for linking LLVM bitcode
# and object files. Those are binary inputs, not source text, so we exclude the
# linker module from this guard.
scoped_paths=(
  parse-js/src
  parse-js-cli/src
  hir-js/src
  typecheck-ts/src
  typecheck-ts-cli/src
  optimize-js/src
  minify-js/src
  minify-js-cli/src
  semantic-js/src
  emit-js/src
  native-js/src
)

scoped_globs=(
  --glob '*.rs'
  --glob '!native-js/src/link.rs'
)

pattern_bytes='pub(?:\s*\([^)]*\))?(?:\s+(?:async|const|unsafe))*\s+fn\s+(?!fuzz_)[^(]+\([^)]*&\[u8\]'
pattern_vec='pub(?:\s*\([^)]*\))?(?:\s+(?:async|const|unsafe))*\s+fn\s+(?!fuzz_)[^(]+\((?![^)]*&mut\s*Vec<u8>)[^)]*Vec<u8>'

if rg --pcre2 --multiline -n "${scoped_globs[@]}" "$pattern_bytes" "${scoped_paths[@]}"; then
  echo "error: UTF-8 source-text API policy violation: public API taking \`&[u8]\` found" >&2
  echo "help: accept source text as \`&str\` or \`Arc<str>\` and validate/convert bytes at IO boundaries" >&2
  echo "note: \`pub fn fuzz_*\` entrypoints are allowed to accept bytes" >&2
  echo "note: byte output buffers like \`&mut Vec<u8>\` are allowed" >&2
  echo "note: run \`just utf8-apis\` (or \`./scripts/check_utf8_apis.sh\`) to reproduce locally" >&2
  exit 1
else
  status=$?
  if [[ $status -ne 1 ]]; then
    exit "$status"
  fi
fi

if rg --pcre2 --multiline -n "${scoped_globs[@]}" "$pattern_vec" "${scoped_paths[@]}"; then
  echo "error: UTF-8 source-text API policy violation: public API taking \`Vec<u8>\` found" >&2
  echo "help: accept source text as \`&str\` or \`Arc<str>\` and validate/convert bytes at IO boundaries" >&2
  echo "note: \`pub fn fuzz_*\` entrypoints are allowed to accept bytes" >&2
  echo "note: byte output buffers like \`&mut Vec<u8>\` are allowed" >&2
  echo "note: run \`just utf8-apis\` (or \`./scripts/check_utf8_apis.sh\`) to reproduce locally" >&2
  exit 1
else
  status=$?
  if [[ $status -ne 1 ]]; then
    exit "$status"
  fi
fi
