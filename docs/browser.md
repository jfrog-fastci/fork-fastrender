# Desktop browser (`browser` binary)

FastRender includes an experimental cross-platform desktop “browser” app: a small windowed shell
(chrome) that hosts the renderer and displays the resulting framebuffer.

Code lives in:

- Entry point: [`src/bin/browser.rs`](../src/bin/browser.rs)
- Shared browser UI helpers: [`src/ui/`](../src/ui/)

## What this is / is not

**This is:**

- A cross-platform *desktop* app (Linux/macOS/Windows) built on `winit + wgpu + egui`.
- A host for the FastRender HTML/CSS renderer, displaying a rendered `tiny_skia::Pixmap` inside the
  window.
- An experimental foundation for an interactive surface over the renderer (no JS required).

**This is not:**

- A “real” web browser engine (no multi-process architecture, no extensions/devtools/service
  workers, etc.).
- A JavaScript-capable browser: there is currently **no author JS engine** and `<script>` does not
  execute. (See [docs/javascript.md](javascript.md) for the separate JS workstream.)

## Build / run

The `browser` binary is feature-gated behind `browser_ui` so the core renderer can compile without
pulling in the GUI stack.

```bash
# Debug build:
cargo run --features browser_ui --bin browser

# Release build:
cargo run --release --features browser_ui --bin browser
```

If you try to run it without `--features browser_ui`, `Cargo.toml` will refuse because the binary
has `required-features = ["browser_ui"]`. See:

- [`Cargo.toml`](../Cargo.toml)
- Platform prerequisites + MSRV constraints + UI architecture details: [browser_ui.md](browser_ui.md)

## Current UI status

The `browser` window UI is still intentionally minimal (early-stage wiring):

- Top chrome bar with back/forward/reload buttons + an address bar text field.
  - Note: the navigation buttons and address bar submission are not yet wired in the window UI.
- Content area currently shows a dummy checkerboard pixmap.
- Clicking in the page prints the local click position to stdout.

The repository contains additional browser UI building blocks (tab history, URL normalization,
interaction engine, etc.) under [`src/ui/`](../src/ui/) that are intended to be wired into the
windowed UI; see [browser_ui.md](browser_ui.md) for details.

## Environment variables / resource limits

Browser-related environment variables live in [env-vars.md](env-vars.md). Notably:

- `FASTR_BROWSER_MEM_LIMIT_MB=<MiB>` – best-effort address-space (virtual memory) limit for the
  `browser` process. This is applied at process start (and may be unsupported on some platforms).

When running against arbitrary real-world pages, consider using the repo’s resource limit wrapper:

```bash
scripts/run_limited.sh --as 64G -- cargo run --release --features browser_ui --bin browser
```

## Implementation notes

For implementation details (code layout, message protocol, cancellation, platform prerequisites),
see [browser_ui.md](browser_ui.md).
