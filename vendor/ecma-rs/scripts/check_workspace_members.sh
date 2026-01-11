#!/usr/bin/env bash
set -euo pipefail

# Validate that the nested ecma-rs workspace member list is deterministic and
# does not contain accidental duplicates.

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_toml="${1:-"$repo_root/Cargo.toml"}"

if [[ ! -f "$cargo_toml" ]]; then
  echo "error: Cargo.toml not found: $cargo_toml" >&2
  exit 1
fi

members="$(
  awk '
    BEGIN { in_members = 0 }
    /^[[:space:]]*members[[:space:]]*=[[:space:]]*\[/ { in_members = 1 }
    !in_members { next }
    {
      line = $0
      sub(/#.*/, "", line)
      while (match(line, /"[^"]+"/)) {
        m = substr(line, RSTART + 1, RLENGTH - 2)
        print m
        line = substr(line, RSTART + RLENGTH)
      }
    }
    in_members && /\]/ { exit }
  ' "$cargo_toml"
)"

if [[ -z "$members" ]]; then
  echo "error: failed to parse [workspace].members from $cargo_toml" >&2
  exit 1
fi

duplicates="$(
  printf '%s\n' "$members" | LC_ALL=C sort | uniq -c | awk '$1 > 1 { print $2 " (" $1 "x)" }'
)"

if [[ -n "$duplicates" ]]; then
  echo "error: duplicate entries found in [workspace].members ($cargo_toml):" >&2
  printf '%s\n' "$duplicates" | sed 's/^/  /' >&2
  exit 1
fi

count="$(printf '%s\n' "$members" | wc -l | tr -d ' ')"
echo "workspace members OK (${count})"
