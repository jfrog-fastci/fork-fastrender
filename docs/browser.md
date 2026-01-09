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
- A small interactive surface (navigation + scroll + hit-testing + basic form interactions) without
  requiring JavaScript.

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

## Current capabilities (MVP)

The desktop browser UI is intentionally minimal, but it provides a **real interactive surface** over
the renderer (no JS required):

- **Navigation**
  - type a URL into the address bar and press Enter
  - supports `about:*` pages (e.g. `about:newtab`, `about:blank`, `about:error`)
  - supports `file://` URLs and filesystem paths (converted to `file://`)
  - per-tab history: back/forward/reload
- **Tabs**: create/close/switch tabs (each tab has its own navigation state)
- **Scrolling**: mouse wheel/trackpad updates the viewport scroll offset and repaints
- **Hit-testing + links**: click links (hit-test fragments → navigate)
- **Basic forms (non-JS)**
  - basic focus + text input for `<input>` / `<textarea>`
  - checkbox/radio toggling

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
