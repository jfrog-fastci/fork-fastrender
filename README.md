# FastRender

An experimental browser engine written in Rust, developed as part of [research into collaborative parallel AI coding agents](https://cursor.com/blog/scaling-agents). Currently under development.

## Goals

- Spec-compliant HTML/CSS rendering
- JavaScript execution with DOM bindings
- Multiprocess architecture with sandboxed renderers
- Cross-platform support (Linux, macOS)

## Build Requirements

### All Platforms

- **Rust 1.70+** (stable channel)
- **Git** (for submodule checkout)
- **CMake** (for AVIF/libaom build)

### macOS

```bash
# Install Xcode Command Line Tools (provides clang)
xcode-select --install

# Install build dependencies via Homebrew
brew install cmake make

# GNU Make is required for the libvpx codec build.
# macOS ships BSD make by default; the build will use `gmake` when available.
```

**Note:** Ensure `gmake` is in your PATH after installing GNU make.

### Ubuntu / Debian

```bash
# Install build essentials and Clang (required as linker driver)
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  pkg-config \
  cmake \
  clang \
  lld

# Install GUI/X11 dependencies:
sudo apt-get install -y \
  libasound2-dev \
  libwayland-dev \
  libxkbcommon-dev \
  libvulkan-dev \
  libegl1-mesa-dev \
  libx11-dev
```

**Optional but recommended:** Install `mold` for significantly faster linking:

```bash
sudo apt-get install -y mold
```

The build system automatically uses `mold` when available. Disable with `FASTR_USE_MOLD=0`.

## Building

**Note:** Under heavy development and change, may not be in a stable state. Builds are currently focused on Linux x64 and macOS ARM64 only.

First, initialize the submodules:

```bash
git submodule update --init vendor/ecma-rs
```

```bash
cargo run --release --features browser_ui --bin browser
```

## Documentation

See [`docs/`](docs/index.md) for detailed documentation.
