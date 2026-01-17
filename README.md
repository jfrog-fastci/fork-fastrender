# FastRender

An experimental browser engine written in Rust, developed as part of [research into collaborative parallel AI coding agents](https://cursor.com/blog/scaling-agents). Currently under development.

## Goals

- Spec-compliant HTML/CSS rendering
- JavaScript execution with DOM bindings
- Multiprocess architecture with sandboxed renderers
- Cross-platform support (Linux, macOS, Windows)

## Building

Under heavy development and change, may not be in a stable state. Builds are currently focused on Linux x64 and macOS ARM64 only.

```bash
cargo run --release --features browser_ui --bin browser
```

Requires Rust 1.70+. See [`docs/`](docs/index.md) for more.
