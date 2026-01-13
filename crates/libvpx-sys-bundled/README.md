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

## Build configuration

`build.rs` runs libvpx's `configure` + `make` inside `OUT_DIR`, using a configuration intended for
decoding VP8/VP9 without extra tools/tests:

- `--target=generic-gnu` (portable build; avoids assembler requirements)
- `--disable-examples --disable-tools --disable-unit-tests --disable-docs`
- `--enable-vp9 --enable-vp8`
- `--disable-webm-io`
- `--enable-static --disable-shared --enable-pic`

## Supported targets

Currently supported:

- `x86_64-unknown-linux-gnu`

Other targets will emit a clear build error.

## Updating libvpx

High-level steps:

1. Replace `upstream/libvpx/` with the new libvpx release source (keep `LICENSE`/`PATENTS`).
2. Regenerate bindings (or copy updated bindings) and update `src/lib.rs`.
3. Run `cargo test -p libvpx-sys-bundled`.

