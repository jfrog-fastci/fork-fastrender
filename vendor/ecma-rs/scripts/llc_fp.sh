#!/usr/bin/env bash
set -euo pipefail

# llc wrapper that enforces frame pointers in generated code.
#
# Our runtime stack walking assumes a canonical frame-pointer chain (Tasks
# 297/366/385/411). LLVM will omit frame pointers in optimized builds unless
# told not to.
#
# x86_64 + AArch64:
#   --frame-pointer=all
#
# Usage:
#   bash scripts/llc_fp.sh -O3 -filetype=obj -o out.o input.ll
#
# Optional:
#   LLC_BIN=llc-18 bash scripts/llc_fp.sh ...

llc_bin="${LLC_BIN:-}"
if [[ -z "${llc_bin}" ]]; then
  # Prefer LLVM 18 explicitly when available; some hosts may have multiple LLVM
  # versions installed and `llc` might not be 18.x.
  if command -v llc-18 >/dev/null 2>&1; then
    llc_bin="llc-18"
  elif command -v llc >/dev/null 2>&1; then
    llc_bin="llc"
  else
    echo "error: llc not found (install llvm-18 and ensure llc is in PATH)" >&2
    exit 1
  fi
fi

has_fp=0
bad_fp_value=""
bad_fp_arg=""
argv=("$@")
for ((i = 0; i < ${#argv[@]}; i++)); do
  arg="${argv[$i]}"
  value=""

  case "${arg}" in
    --frame-pointer=*|-frame-pointer=*)
      has_fp=1
      value="${arg#*=}"
      ;;
    --frame-pointer|-frame-pointer)
      has_fp=1
      value="${argv[$((i + 1))]:-}"
      if [[ -z "${value}" ]]; then
        echo "error: llc_fp.sh: ${arg} expects a value (use --frame-pointer=all)" >&2
        exit 1
      fi
      i=$((i + 1))
      ;;
  esac

  if [[ -n "${value}" && "${value}" != "all" ]]; then
    bad_fp_value="${value}"
    bad_fp_arg="${arg}"
    break
  fi
done

if [[ -n "${bad_fp_value}" ]]; then
  got="${bad_fp_arg}"
  if [[ "${got}" != *"="* ]]; then
    got="${bad_fp_arg} ${bad_fp_value}"
  fi
  cat >&2 <<EOF
error: llc_fp.sh: frame pointers are required for stack-walking, but got ${got}
hint: use --frame-pointer=all (or remove the flag and let llc_fp.sh inject it)
EOF
  exit 1
fi

extra=()
if [[ "${has_fp}" -eq 0 ]]; then
  extra+=(--frame-pointer=all)
fi

exec "${llc_bin}" "${extra[@]}" "$@"
