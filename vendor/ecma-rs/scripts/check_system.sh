#!/usr/bin/env bash
set -euo pipefail

# Quick system check for ecma-rs native compilation project.
# Run this to verify dependencies are installed correctly.
#
# Usage:
#   scripts/check_system.sh
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

errors=0
warnings=0

check_cmd() {
  local cmd="$1"
  local pkg="${2:-$1}"
  if command -v "$cmd" >/dev/null 2>&1; then
    local ver
    ver="$("$cmd" --version 2>/dev/null | head -1 || echo "unknown")"
    echo -e "${GREEN}✓${NC} $cmd: $ver"
    return 0
  else
    echo -e "${RED}✗${NC} $cmd not found (install: $pkg)"
    ((errors++))
    return 1
  fi
}

check_cmd_optional() {
  local cmd="$1"
  local pkg="${2:-$1}"
  if command -v "$cmd" >/dev/null 2>&1; then
    local ver
    ver="$("$cmd" --version 2>/dev/null | head -1 || echo "unknown")"
    echo -e "${GREEN}✓${NC} $cmd: $ver"
    return 0
  else
    echo -e "${YELLOW}?${NC} $cmd not found (optional: $pkg)"
    ((warnings++))
    return 1
  fi
}

echo "=== System Check for ecma-rs Native Compilation ==="
echo ""

echo "--- Core Tools ---"
check_cmd rustc "rustup (https://rustup.rs)"
check_cmd cargo "rustup"
check_cmd gcc "build-essential"
check_cmd make "build-essential"
check_cmd git "git"

echo ""
echo "--- Workspace Sanity ---"
if bash "${SCRIPT_DIR}/check_workspace_members.sh"; then
  :
else
  errors=$((errors + 1))
fi

echo ""
echo "--- Resource Limiting (required for multi-agent) ---"
check_cmd flock "util-linux"
check_cmd prlimit "util-linux"

echo ""
echo "--- LLVM 18 (for native codegen) ---"
# Try versioned commands first, then unversioned
if check_cmd_optional llvm-config-18 "llvm-18-dev"; then
  :
elif check_cmd_optional llvm-config "llvm-dev"; then
  llvm_ver="$(llvm-config --version 2>/dev/null || echo "0")"
  if [[ "${llvm_ver%%.*}" -lt 18 ]]; then
    echo -e "${YELLOW}  Warning: LLVM ${llvm_ver} found, but LLVM 18+ recommended${NC}"
    ((warnings++))
  fi
fi

if check_cmd_optional clang-18 "clang-18"; then
  :
elif check_cmd_optional clang "clang"; then
  :
fi

if check_cmd_optional lld-18 "lld-18"; then
  :
elif check_cmd_optional lld "lld"; then
  :
fi
if check_cmd_optional llc-18 "llvm-18"; then
  :
elif check_cmd_optional llc "llvm"; then
  :
fi

if check_cmd_optional llvm-readobj-18 "llvm-18"; then
  :
elif check_cmd_optional llvm-readobj "llvm"; then
  :
fi

if check_cmd_optional llvm-objdump-18 "llvm-18"; then
  :
elif check_cmd_optional llvm-objdump "llvm"; then
  :
fi

echo ""
echo "--- LLVM Statepoint StackMap ABI (recommended) ---"
if (command -v llc-18 >/dev/null 2>&1 || command -v llc >/dev/null 2>&1) &&
  (command -v llvm-readobj-18 >/dev/null 2>&1 || command -v llvm-readobj >/dev/null 2>&1) &&
  (command -v llvm-objdump-18 >/dev/null 2>&1 || command -v llvm-objdump >/dev/null 2>&1); then
  if bash "${SCRIPT_DIR}/test_stackmap_abi.sh"; then
    echo -e "${GREEN}✓${NC} stackmap ABI test passed"
  else
    echo -e "${RED}✗${NC} stackmap ABI test failed (run: bash ${SCRIPT_DIR}/test_stackmap_abi.sh)"
    ((errors++))
  fi

  # Additional x86_64-only corner-case checks (flags + patch_bytes).
  if [[ "$(uname -m)" == "x86_64" ]]; then
    if bash "${SCRIPT_DIR}/test_statepoint_flags_patchbytes.sh"; then
      echo -e "${GREEN}✓${NC} statepoint flags/patch_bytes test passed"
    else
      echo -e "${RED}✗${NC} statepoint flags/patch_bytes test failed (run: bash ${SCRIPT_DIR}/test_statepoint_flags_patchbytes.sh)"
      ((errors++))
    fi
  else
    echo -e "${YELLOW}?${NC} statepoint flags/patch_bytes test skipped (requires x86_64 host)"
    ((warnings++))
  fi
else
  echo -e "${YELLOW}?${NC} stackmap ABI tests skipped (missing llc/llvm-readobj/llvm-objdump)"
  ((warnings++))
fi

echo ""
echo "--- Environment Variables ---"
if [[ -n "${LLVM_SYS_180_PREFIX:-}" ]]; then
  if [[ -d "${LLVM_SYS_180_PREFIX}" ]]; then
    echo -e "${GREEN}✓${NC} LLVM_SYS_180_PREFIX=${LLVM_SYS_180_PREFIX}"
  else
    echo -e "${RED}✗${NC} LLVM_SYS_180_PREFIX=${LLVM_SYS_180_PREFIX} (directory not found)"
    ((errors++))
  fi
else
  if [[ -d /usr/lib/llvm-18 ]]; then
    echo -e "${YELLOW}?${NC} LLVM_SYS_180_PREFIX not set (but /usr/lib/llvm-18 exists, will auto-detect)"
  else
    echo -e "${YELLOW}?${NC} LLVM_SYS_180_PREFIX not set"
    ((warnings++))
  fi
fi

echo ""
echo "--- System Resources ---"
# Memory
if [[ -f /proc/meminfo ]]; then
  mem_kb=$(grep MemTotal /proc/meminfo | awk '{print $2}')
  mem_gb=$((mem_kb / 1024 / 1024))
  if [[ $mem_gb -ge 64 ]]; then
    echo -e "${GREEN}✓${NC} RAM: ${mem_gb} GB"
  elif [[ $mem_gb -ge 32 ]]; then
    echo -e "${YELLOW}?${NC} RAM: ${mem_gb} GB (64GB+ recommended for LLVM builds)"
    ((warnings++))
  else
    echo -e "${RED}✗${NC} RAM: ${mem_gb} GB (need at least 32GB for LLVM builds)"
    ((errors++))
  fi
else
  echo -e "${YELLOW}?${NC} RAM: unable to detect"
fi

# CPUs
if command -v nproc >/dev/null 2>&1; then
  cpus=$(nproc)
  echo -e "${GREEN}✓${NC} CPUs: ${cpus}"
fi

# Disk (just informational)
if command -v df >/dev/null 2>&1; then
  disk_avail=$(df -BG . 2>/dev/null | tail -1 | awk '{print $4}' | tr -d 'G')
  if [[ -n "$disk_avail" && "$disk_avail" =~ ^[0-9]+$ ]]; then
    if [[ $disk_avail -ge 100 ]]; then
      echo -e "${GREEN}✓${NC} Disk available: ${disk_avail} GB"
    elif [[ $disk_avail -ge 50 ]]; then
      echo -e "${YELLOW}?${NC} Disk available: ${disk_avail} GB (100GB+ recommended)"
      ((warnings++))
    else
      echo -e "${RED}✗${NC} Disk available: ${disk_avail} GB (need 50GB+ for builds)"
      ((errors++))
    fi
  fi
fi

echo ""
echo "=== Summary ==="
if [[ $errors -gt 0 ]]; then
  echo -e "${RED}${errors} error(s)${NC}, ${warnings} warning(s)"
  echo ""
  echo "Install missing dependencies:"
  echo "  sudo apt-get install build-essential util-linux llvm-18 llvm-18-dev clang-18 lld-18"
  exit 1
elif [[ $warnings -gt 0 ]]; then
  echo -e "${GREEN}OK${NC} with ${YELLOW}${warnings} warning(s)${NC}"
  exit 0
else
  echo -e "${GREEN}All checks passed${NC}"
  exit 0
fi
