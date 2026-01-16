# FastRender

An experimental browser engine written in Rust, developed through collaborative parallel AI coding agents. Currently under development.

## Goals

- Spec-compliant HTML/CSS rendering
- JavaScript execution with DOM bindings
- Multiprocess architecture with sandboxed renderers
- Cross-platform support (Linux, macOS, Windows)

## Building

```bash
cargo run --release --features browser_ui --bin browser
```

Requires Rust 1.70+. See [`docs/`](docs/index.md) for more.
