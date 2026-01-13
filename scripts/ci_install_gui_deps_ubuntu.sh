#!/usr/bin/env bash
set -euo pipefail

# Install Linux system dependencies needed to compile GUI stacks on CI.
#
# We intentionally keep this in a script (instead of inlining apt commands in every
# workflow job) to reduce duplication and ensure jobs stay in sync.
#
# This is primarily required for CI builds that enable the optional `browser_ui` feature
# (winit/wgpu/egui stack). CI uses the `ci` Cargo feature-set rather than `--all-features` so we
# can keep system-dependent features (like real audio output backends) opt-in.

SUDO=""
if [[ "$(id -u)" != "0" ]]; then
  if command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
  else
    echo "error: need root or sudo to install packages" >&2
    exit 2
  fi
fi

# Soft guard: warn if not Ubuntu (GitHub Actions ubuntu-* runners).
if [[ -r /etc/os-release ]]; then
  # shellcheck disable=SC1091
  . /etc/os-release
  if [[ "${ID:-}" != "ubuntu" ]]; then
    echo "warning: scripts/ci_install_gui_deps_ubuntu.sh is tuned for Ubuntu (detected ID=${ID:-unknown}); continuing..." >&2
  fi
fi

export DEBIAN_FRONTEND=noninteractive

echo "Updating apt indices..."
${SUDO} apt-get update -y

echo "Installing Linux GUI build dependencies..."
${SUDO} apt-get install -y \
  pkg-config \
  libasound2-dev \
  libwayland-dev \
  libxkbcommon-dev \
  libvulkan-dev \
  libegl1-mesa-dev \
  libx11-dev
