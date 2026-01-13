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
decoding VP8/VP9 without extra tools/tests:

- `--target=generic-gnu` (portable build; avoids assembler requirements)
- `--disable-examples --disable-tools --disable-unit-tests --disable-docs`
- `--disable-debug-libs` (avoid building `libvpx_g.a`, etc)
- `--enable-vp9 --enable-vp8`
- `--disable-webm-io`
- `--disable-libyuv` (avoid building libyuv / C++ compilation)
- `--enable-static --disable-shared --enable-pic`

## Supported targets

Currently supported:

- `x86_64-unknown-linux-gnu`

Other targets will emit a clear build error.

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
