# libvpx-sys-bundled

This crate provides **vendored** (bundled) Rust FFI bindings for **libvpx** and builds libvpx
from source during `cargo build`.

Goals:

- No dependency on system `libvpx` packages.
- No `bindgen` required at build time (bindings are checked in).
- No `nasm`/`yasm` requirement by default (portable C-only build).

## Vendored version

The libvpx sources are vendored under:

`upstream/libvpx/`

Current version: **libvpx 1.13.1**

`build.rs` explicitly lists all files under `upstream/libvpx/` via `cargo:rerun-if-changed=...`
so that changes to the vendored source reliably trigger a rebuild.

## Build configuration

`build.rs` runs libvpx's `configure` + `make` inside `OUT_DIR`, using a configuration intended for
VP8/VP9 decoding without extra tools/tests.

Note: libvpx's build system requires **GNU make**. On macOS, the system `make` is typically BSD
make, so `build.rs` will prefer `gmake` when available (or you can override via the `MAKE` / scoped
`MAKE_<TARGET>` environment variables).

Target selection:

- Linux x86_64: `--target=generic-gnu`
- macOS x86_64: `--target=x86_64-darwinXX-gcc` (XX is detected from the host and clamped to the
  libvpx release's known Darwin toolchains)
- macOS aarch64 (Apple Silicon): `--target=arm64-darwinXX-gcc`
- Windows x86-gnu (MinGW): `--target=x86-win32-gcc`
- Windows x86_64-gnu (MinGW): `--target=x86_64-win64-gcc`
- Windows aarch64-gnu (MinGW): `--target=arm64-win64-gcc`
- Windows x86-msvc: `--target=x86-win32-vsNN` (NN is detected from the host, defaulting to 16)
- Windows x86_64-msvc: `--target=x86_64-win64-vsNN` (NN is detected from the host, defaulting to 16)
- Windows aarch64-msvc: `--target=arm64-win64-vsNN` (NN is detected from the host; VS2017+ / vs15+ required)

Common flags:

- `--disable-examples --disable-tools --disable-unit-tests --disable-docs`
- Decoder-only build:
  - `--enable-vp8-decoder --enable-vp9-decoder`
  - `--enable-vp9-highbitdepth` (allows decoding 10/12-bit VP9 streams; callers must handle
    16-bit output frames)
  - `--disable-vp8-encoder --disable-vp9-encoder`
- `--disable-webm-io` (no libwebm dependency)
- `--disable-libyuv` (avoid C++ compilation)
- `--enable-static --disable-shared --enable-pic`

To avoid requiring `nasm`/`yasm` on x86/x86_64 targets, `build.rs` disables runtime CPU detection and
all x86 SIMD feature flags, and sets `AS=true` to bypass assembler auto-detection.

## Supported targets

Supported (best-effort; CI coverage may vary):

- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin` (native builds only; cross-compiling the bundled libvpx is not supported)
- `aarch64-apple-darwin` (native builds only; cross-compiling the bundled libvpx is not supported)
- `i686-pc-windows-gnu` (MinGW; may require `CROSS` / a MinGW-w64 toolchain when cross-compiling)
- `x86_64-pc-windows-gnu` (MinGW; may require `CROSS` / a MinGW-w64 toolchain when cross-compiling)
- `aarch64-pc-windows-gnu` (MinGW; may require `CROSS` / a MinGW-w64 toolchain when cross-compiling)
- `i686-pc-windows-msvc` (MSVC; requires MSYS2/Cygwin + `msbuild.exe` in PATH)
- `x86_64-pc-windows-msvc` (MSVC; requires MSYS2/Cygwin + `msbuild.exe` in PATH)
- `aarch64-pc-windows-msvc` (MSVC; requires MSYS2/Cygwin + `msbuild.exe` in PATH; VS2017+ / vs15+ required)

Unsupported:

- Linux `musl` targets (use a GNU target or a system libvpx)

Other targets will emit a clear build error (`cargo:warning` + panic) with guidance.

Notes for Windows:

- libvpx's upstream README requires **MSYS2 or Cygwin**. On Windows hosts, this crate runs the
  vendored `configure` script via `sh`/`bash`, so a POSIX shell must be in `PATH`.
- When cross-compiling to MinGW from a non-Windows host, you may need to set `CROSS=` or a
  target-scoped `CC_*` toolchain prefix.
- For MSVC targets, the build uses libvpx's generated Visual Studio solution and runs `msbuild.exe`
  via the vendored makefiles. Ensure you are running under a VS Developer Command Prompt (or
  otherwise have `msbuild.exe` available in PATH).
- By default, the MSVC build is also C-only (no `yasm`/`nasm` required). If you change the build
  configuration to enable x86 SIMD/assembly, you may need to install `yasm` or `nasm`.

## ABI helpers

The upstream C headers provide convenience macros like `vpx_codec_dec_init()` that expand to
`vpx_codec_dec_init_ver(..., VPX_DECODER_ABI_VERSION)`. Since macros aren't available through FFI,
this crate defines the relevant ABI constants and provides an equivalent Rust wrapper:

- `VPX_DECODER_ABI_VERSION`
- `unsafe fn vpx_codec_dec_init(...)`

## Updating libvpx

High-level steps:

1. Replace `upstream/libvpx/` with the new libvpx release source (keep `LICENSE`/`PATENTS`).
2. Regenerate bindings (or update them) and update `src/lib.rs` (the `vpx_image_t` layout must
   match the vendored libvpx version).
3. Run `cargo test -p libvpx-sys-bundled`.

## Tests

In addition to a basic version-string smoke test, this crate includes an integration test that
parses the repo's deterministic CC0 WebM fixture and decodes a VP9 frame via libvpx to validate
the FFI surface (`tests/decode_vp9.rs`).

## Decoder helper (experimental)

This crate also exposes a small convenience wrapper, [`Vp9Decoder`](src/vp9_decoder.rs), which:

- wraps libvpx's VP9 decode API
- converts decoded YUV frames to RGBA8
- supports libvpx high-bit-depth output (`VPX_IMG_FMT_HIGHBITDEPTH`) by downshifting 10/12-bit
  16-bit planes to 8-bit before RGB conversion (lossy, but avoids silent corruption)

This is intended as a building block for higher-level container/media plumbing in the main
FastRender crate.
