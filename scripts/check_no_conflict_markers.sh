#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Guardrail: never commit unresolved merge conflict markers into Rust sources.
#
# Why this exists:
# - `vendor/ecma-rs/vm-js/src/exec.rs` previously landed with `<<<<<<<` / `=======` / `>>>>>>>`
#   markers and broke compilation.
# - Some vendored fixtures (notably TypeScript's test suite) intentionally contain conflict-marker
#   strings; those directories must be excluded.

required_roots=(
  "vendor/ecma-rs/vm-js/src"
  "vendor/ecma-rs/parse-js/src"
  "vendor/ecma-rs/semantic-js/src"
  "vendor/ecma-rs/test262-semantic/src"
)

missing_roots=()
for root in "${required_roots[@]}"; do
  if [[ ! -d "${root}" ]]; then
    missing_roots+=("${root}")
  fi
done
if [[ "${#missing_roots[@]}" -ne 0 ]]; then
  echo "warning: expected source directories missing (conflict-marker scan may be incomplete):" >&2
  for root in "${missing_roots[@]}"; do
    echo "  - ${root}" >&2
  done
  echo "warning: if this is a legacy checkout with submodules, run: bash scripts/ci_init_ecma_rs_submodule.sh" >&2
fi

# Common conflict markers (including diff3-style `|||||||`).
#
# Keep the separator strict (`^=======$`) so we don't trip on long "======" rulers in docs/logs.
conflict_re='^(<<<<<<<|>>>>>>>|[|]{7}|=======[[:space:]]*$)'

have_rg=0
if command -v rg >/dev/null 2>&1; then
  have_rg=1
fi

matches=""

append_matches() {
  local new_matches="$1"
  if [[ -z "${new_matches}" ]]; then
    return 0
  fi
  if [[ -z "${matches}" ]]; then
    matches="${new_matches}"
  else
    matches+=$'\n'"${new_matches}"
  fi
}

if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  # Prefer `git grep`: it is fast, and only scans tracked files (avoids traversing large fixture
  # trees like WPT).
  pathspecs=(
    ':(glob)src/**/*.rs'
    ':(glob)crates/**/*.rs'
    ':(glob)tests/**/*.rs'
    ':(glob)xtask/**/*.rs'
    ':(glob)benches/**/*.rs'
    ':(glob)fuzz/**/*.rs'
    ':(glob)tools/**/*.rs'

    # Required vendored sources.
    ':(glob)vendor/ecma-rs/vm-js/src/**/*.rs'
    ':(glob)vendor/ecma-rs/parse-js/src/**/*.rs'
    ':(glob)vendor/ecma-rs/semantic-js/src/**/*.rs'
    ':(glob)vendor/ecma-rs/test262-semantic/src/**/*.rs'

    # Known conflict-marker fixtures (allowed).
    ':!vendor/ecma-rs/parse-js/tests/TypeScript/**'
  )
  set +e
  git_matches="$(
    git -c grep.recurseSubmodules=false grep -nE "${conflict_re}" -- "${pathspecs[@]}"
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: git grep failed while scanning for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
  append_matches "${git_matches}"
elif [[ "${have_rg}" -eq 1 ]]; then
  # Fallback for environments without git (rare).
  set +e
  rg_matches="$(
    rg -n \
      --glob '*.rs' \
      --glob '!vendor/ecma-rs/parse-js/tests/TypeScript/**' \
      "${conflict_re}" \
      src crates tests xtask benches fuzz tools \
      vendor/ecma-rs/vm-js/src \
      vendor/ecma-rs/parse-js/src \
      vendor/ecma-rs/semantic-js/src \
      vendor/ecma-rs/test262-semantic/src \
      2>/dev/null
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: rg failed while scanning for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
  append_matches "${rg_matches}"
else
  # Fallback for environments without ripgrep (e.g. minimal Windows shells).
  grep_matches="$(
    grep -RInE \
      --include='*.rs' \
      "${conflict_re}" \
      src crates tests xtask benches fuzz tools \
      vendor/ecma-rs/vm-js/src \
      vendor/ecma-rs/parse-js/src \
      vendor/ecma-rs/semantic-js/src \
      vendor/ecma-rs/test262-semantic/src \
      2>/dev/null || true
  )"
  append_matches "${grep_matches}"
fi

# In legacy checkouts `vendor/ecma-rs` may be a git submodule. The top-level `git grep` will not
# search inside submodules, so explicitly scan the ecma-rs repository if it exists as its own git
# work tree.
if command -v git >/dev/null 2>&1 && [[ -e vendor/ecma-rs/.git ]]; then
  set +e
  ecma_rs_matches="$(
    git -c grep.recurseSubmodules=false -C vendor/ecma-rs grep -nE "${conflict_re}" -- \
      ':(glob)vm-js/src/**/*.rs' \
      ':(glob)parse-js/src/**/*.rs' \
      ':(glob)semantic-js/src/**/*.rs' \
      ':(glob)test262-semantic/src/**/*.rs' \
      ':!parse-js/tests/TypeScript/**'
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: git grep failed while scanning vendor/ecma-rs submodule for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
  if [[ -n "${ecma_rs_matches}" ]]; then
    # Prefix submodule-local paths so errors point at the superproject file locations.
    ecma_rs_matches="$(printf '%s\n' "${ecma_rs_matches}" | sed 's|^|vendor/ecma-rs/|')"
    append_matches "${ecma_rs_matches}"
  fi
fi

if [[ -n "${matches}" ]]; then
  matches="$(printf '%s\n' "${matches}" | sort -u)"
fi

if [[ -n "${matches}" ]]; then
  echo "error: found unresolved merge conflict markers in Rust source files:" >&2
  echo "${matches}" >&2
  echo >&2
  echo "hint: resolve the merge conflicts and remove the marker lines (<<<<<<< / ||||||| / ======= / >>>>>>>)." >&2
  echo "note: conflict-marker fixtures under vendor/ecma-rs/parse-js/tests/TypeScript/** are allowed." >&2

  # GitHub Actions integration: annotate each match so the UI can hyperlink directly to the
  # offending file/line. Keep the plain `path:line:` output above for local runs and non-GHA CI.
  if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    max_annotations=50
    n=0

    gh_escape_prop() {
      local value="$1"
      value="${value//'%'/'%25'}"
      value="${value//$'\r'/'%0D'}"
      value="${value//$'\n'/'%0A'}"
      value="${value//':'/'%3A'}"
      value="${value//','/'%2C'}"
      printf '%s' "${value}"
    }

    while IFS= read -r match; do
      [[ -z "${match}" ]] && continue
      n=$((n + 1))
      if [[ "${n}" -gt "${max_annotations}" ]]; then
        echo "::error::too many conflict-marker hits (${n}+); showing first ${max_annotations}" >&2
        break
      fi

      file="${match%%:*}"
      rest="${match#*:}"
      line="${rest%%:*}"
      text="${rest#*:}"

      # Escape message per GitHub Actions command format.
      esc="${text//'%'/'%25'}"
      esc="${esc//$'\r'/'%0D'}"
      esc="${esc//$'\n'/'%0A'}"

      file_esc="$(gh_escape_prop "${file}")"
      line_esc="$(gh_escape_prop "${line}")"

      echo "::error file=${file_esc},line=${line_esc}::unresolved merge conflict marker: ${esc}" >&2
    done <<< "${matches}"
  fi
  exit 1
fi

# Guardrail: prevent a recurring `parse-js` merge issue where identical inherent methods are
# duplicated on `Parser`, breaking compilation with `E0592 duplicate definitions`.
#
# This has happened multiple times due to bad merges landing on `main`. Catch it early with a cheap
# textual scan so CI can fail before (or even without) building the full ecma-rs workspace.
parse_js_parser_mod="vendor/ecma-rs/parse-js/src/parse/mod.rs"
if [[ -f "${parse_js_parser_mod}" ]]; then
  check_unique_inherent_method() {
    local method_name="$1"
    local re="^[[:space:]]*(pub(\\([^)]*\\))?[[:space:]]+)?fn[[:space:]]+${method_name}\\b"
    local hits
    hits="$(grep -nE "${re}" "${parse_js_parser_mod}" || true)"
    # Strip empty line that `grep` may produce via `|| true`.
    local n
    n="$(printf '%s\n' "${hits}" | sed '/^$/d' | wc -l)"
    if [[ "${n}" -gt 1 ]]; then
      echo "error: duplicate inherent method definitions in ${parse_js_parser_mod}: ${method_name}" >&2
      echo "${hits}" >&2
      exit 1
    fi
  }

  check_unique_inherent_method "with_disallow_arguments_in_class_init"
  check_unique_inherent_method "validate_arguments_not_disallowed_in_class_init"
fi

# Guardrail: prevent merge drift from duplicating top-level native binding helpers in the generated
# vmjs window realm, breaking compilation with `E0428: the name ... is defined multiple times`.
vmjs_window_realm_rs="src/js/vmjs/window_realm.rs"
if [[ -f "${vmjs_window_realm_rs}" ]]; then
  # Extract function names from top-level `fn` definitions only (column 0) so we don't trip on
  # methods inside `impl` blocks.
  duplicate_toplevel_fns="$(
    awk '
      /^(pub|fn)/ && $0 ~ /(^|[[:space:]])fn[[:space:]]/ {
        for (i = 1; i <= NF; i++) {
          if ($i == "fn" && (i + 1) <= NF) {
            name = $(i + 1)
            sub(/[^A-Za-z0-9_].*/, "", name)
            if (name ~ /^[A-Za-z_][A-Za-z0-9_]*$/) {
              print name
            }
            break
          }
        }
      }
    ' "${vmjs_window_realm_rs}" | sort | uniq -d
  )"

  if [[ -n "${duplicate_toplevel_fns}" ]]; then
    echo "error: duplicate top-level function definitions in ${vmjs_window_realm_rs}:" >&2
    while IFS= read -r fn_name; do
      [[ -z "${fn_name}" ]] && continue
      echo "duplicate function: ${fn_name}" >&2
      grep -nE "^(pub|fn).*\\bfn[[:space:]]+${fn_name}\\b" "${vmjs_window_realm_rs}" >&2 || true
      echo >&2
    done <<< "${duplicate_toplevel_fns}"
    exit 1
  fi
fi

# Guardrail: prevent merge drift from duplicating top-level constants/helpers in the WebIDL host
# dispatch, which breaks compilation with errors like:
# - `E0428: the name ... is defined multiple times`
# - `unexpected closing delimiter` (often caused by accidentally duplicated blocks).
vmjs_host_dispatch_rs="src/js/webidl/vmjs_host_dispatch.rs"
if [[ -f "${vmjs_host_dispatch_rs}" ]]; then
  duplicate_toplevel_consts="$(
    awk '
      # Only match top-level const/static defs (column 0) to avoid capturing locals or nested items.
      /^(pub|const|static)/ && $0 ~ /(^|[[:space:]])(const|static)[[:space:]]/ {
        for (i = 1; i <= NF; i++) {
          if (($i == "const" || $i == "static") && (i + 1) <= NF) {
            name = $(i + 1)
            if (name == "mut" && (i + 2) <= NF) {
              name = $(i + 2)
            }
            sub(/[^A-Za-z0-9_].*/, "", name)
            if (name ~ /^[A-Za-z_][A-Za-z0-9_]*$/) {
              print name
            }
            break
          }
        }
      }
    ' "${vmjs_host_dispatch_rs}" | sort | uniq -d
  )"

  if [[ -n "${duplicate_toplevel_consts}" ]]; then
    echo "error: duplicate top-level const/static definitions in ${vmjs_host_dispatch_rs}:" >&2
    while IFS= read -r name; do
      [[ -z "${name}" ]] && continue
      echo "duplicate const/static: ${name}" >&2
      grep -nE "^(pub|const|static).*\\b(const|static)[[:space:]]+(mut[[:space:]]+)?${name}\\b" "${vmjs_host_dispatch_rs}" >&2 || true
      echo >&2
    done <<< "${duplicate_toplevel_consts}"
    exit 1
  fi

  duplicate_toplevel_fns="$(
    awk '
      /^(pub|fn)/ && $0 ~ /(^|[[:space:]])fn[[:space:]]/ {
        for (i = 1; i <= NF; i++) {
          if ($i == "fn" && (i + 1) <= NF) {
            name = $(i + 1)
            sub(/[^A-Za-z0-9_].*/, "", name)
            if (name ~ /^[A-Za-z_][A-Za-z0-9_]*$/) {
              print name
            }
            break
          }
        }
      }
    ' "${vmjs_host_dispatch_rs}" | sort | uniq -d
  )"

  if [[ -n "${duplicate_toplevel_fns}" ]]; then
    echo "error: duplicate top-level function definitions in ${vmjs_host_dispatch_rs}:" >&2
    while IFS= read -r fn_name; do
      [[ -z "${fn_name}" ]] && continue
      echo "duplicate function: ${fn_name}" >&2
      grep -nE "^(pub|fn).*\\bfn[[:space:]]+${fn_name}\\b" "${vmjs_host_dispatch_rs}" >&2 || true
      echo >&2
    done <<< "${duplicate_toplevel_fns}"
    exit 1
  fi
fi
