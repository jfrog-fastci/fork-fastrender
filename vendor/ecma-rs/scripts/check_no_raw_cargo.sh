#!/usr/bin/env bash
set -euo pipefail

# Guard against accidentally re-introducing raw `cargo <subcommand>` invocations into
# `vendor/ecma-rs` developer tooling.
#
# In the FastRender monorepo, all cargo builds/tests must go through
# `scripts/cargo_agent.sh` (and `scripts/cargo_llvm.sh` for LLVM-heavy crates) so
# we don't OOM multi-agent hosts.

if ! command -v rg >/dev/null 2>&1; then
  echo "error: rg (ripgrep) is required for cargo hygiene checks" >&2
  exit 1
fi

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Match raw cargo invocations in scripts/justfile. We intentionally only scan
# developer tooling (scripts + justfile), not docs.
#
# This catches common "inline env var" invocations too, e.g.:
#   RUSTFLAGS="..." cargo test ...
#   env RUSTFLAGS="..." cargo test ...
# and toolchain overrides:
#   cargo +nightly test ...
#
# Note: we support both quoted and unquoted env var values since things like
# `RUSTFLAGS="-C foo=bar"` commonly include spaces.
#
# The command may also be chained, e.g.:
#   cd vendor/ecma-rs && cargo test ...
#
# Ignore comment-only lines (leading `#`), otherwise it's too easy to trip the guard
# from documentation within scripts/justfile.
pattern="^(?![[:space:]]*#)(?:[[:space:]]*(?:env[[:space:]]+)?(?:[A-Za-z_][A-Za-z0-9_]*=(?:\"[^\"]*\"|'[^']*'|\\S+)[[:space:]]+)*cargo\\b(?:[[:space:]]+\\+[^[:space:]]+)?(?:[[:space:]]+|$)|.*?(?:&&|\\|\\||;|\\||\\(|\\))[[:space:]]*(?:env[[:space:]]+)?(?:[A-Za-z_][A-Za-z0-9_]*=(?:\"[^\"]*\"|'[^']*'|\\S+)[[:space:]]+)*cargo\\b(?:[[:space:]]+\\+[^[:space:]]+)?(?:[[:space:]]+|$))"

fail=0

check_path() {
  local path="$1"
  if rg -n --pcre2 "${pattern}" "${path}"; then
    fail=1
  fi
}

check_path justfile
check_path scripts
check_path format
check_path version
check_path parse-js/scripts
check_path bench/minify-js/build

# Also disallow spawning a nested Cargo process from Rust code (commonly seen in
# integration tests). This bypasses the wrapper's global slot limiting + memory
# caps and can OOM shared hosts.
if rg -n --fixed-strings 'Command::new("cargo")' -g'*.rs' .; then
  fail=1
fi

if [[ "${fail}" -ne 0 ]]; then
  echo "error: raw cargo invocations found; use scripts/cargo_agent.sh instead" >&2
  exit 1
fi
