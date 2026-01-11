#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

LLC_BIN="${LLC_BIN:-llc-18}"
if ! command -v "${LLC_BIN}" >/dev/null 2>&1; then
  LLC_BIN="llc"
fi

echo "Using ${LLC_BIN}: $(${LLC_BIN} --version | head -n 1)"
exec "${LLC_BIN}" -O0 -filetype=obj -o statepoint_fixture.o statepoint_fixture.ll

