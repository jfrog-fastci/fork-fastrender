# Browser UI / chrome (cross-platform app frame)

Common repo-wide rules (non-negotiables, resource limits, disk hygiene) live in `AGENTS.md`.

This repo is not just a “PNG renderer” long-term: we want an **interactive desktop app** (“a real browser”) that hosts the renderer with an address bar, tabs, navigation controls, and basic input handling.

This document is the starting point for building that cross-platform browser chrome.

## Goals

- A cross-platform windowed app (Linux/macOS/Windows).
- Minimal browser chrome:
  - address bar (URL entry),
  - back/forward/reload,
  - tabs (new/close/switch),
  - basic status (loading/error).
- Show rendered content inside the window, with:
  - scroll (viewport offset),
  - basic pointer/keyboard routing for links and form controls (no JS required).

## Non-goals (for the first iterations)

- No author JavaScript engine.
- No full web compatibility layer (extensions, devtools, service workers, etc.).
- No pixel-diff/Chrome-baseline gating inside the app.

## Recommended architecture (MVP → scalable)

### 1) New binary

Add a new binary target (name TBD, e.g. `browser`):

- `src/bin/browser.rs`

It owns the OS window + event loop and renders UI + page content each frame.

Note: keep UI dependencies behind a feature gate so the core renderer stays lean.
Run it with:

```bash
cargo run --features browser_ui --bin browser
```

### 2) UI framework choice

Prefer a UI stack that is:

- cross-platform,
- easy to iterate on,
- can display a pixel buffer as a texture,
- integrates cleanly with Rust’s async/work queues.

Pragmatic default: **winit + wgpu + egui** (via `eframe` or direct integration).

### 3) Split responsibilities

- **UI thread**: event loop, input, chrome widgets, presenting pixels.
- **Render worker**: fetch + parse + style + layout + paint, producing an RGBA buffer for the current viewport.

Communication:

- UI → worker: “navigate to URL”, “scroll to (x,y)”, “viewport size changed”, “click at (x,y)”, “key press”.
- Worker → UI: “new frame ready”, “title/URL changed”, “load error”, “navigation state changed”.

Keep this message-based even if everything is initially synchronous; it prevents the UI from blocking on slow pages.

## Data model (suggested)

- `BrowserApp`
  - `tabs: Vec<Tab>`
  - `active_tab: usize`
  - `ui: ChromeState` (address bar text, focus, hover, etc.)
- `Tab`
  - `history: Vec<HistoryEntry>`
  - `history_index: usize`
  - `loading: bool`
  - `viewport: ViewportState` (width/height, scroll_x/scroll_y)
  - `latest_frame: Option<RenderedFrame>` (RGBA + dimensions)
  - `page_state: PageInteractionState` (focused element id, caret, selection, etc. — evolves over time)

## MVP milestone plan (what to implement first)

### Milestone 0: window + pixels

- Create the binary.
- Open a window.
- Present a dummy framebuffer (solid color or checkerboard).

### Milestone 1: render and display one page

- On startup, render `https://example.com` (or an offline HTML string) using the existing pipeline.
- Display the resulting RGBA buffer in the window.

### Milestone 2: address bar navigation

- Address bar input field.
- Press Enter → navigate (send to worker, render, update frame).
- Basic error display when navigation fails.

### Milestone 3: scroll

- Mouse wheel / trackpad scroll changes `scroll_y`.
- Re-render viewport at new scroll offset and present updated frame.

### Milestone 4: basic hit-testing + link navigation

- On click, map window coords → page coords (viewport + scroll).
- Hit-test fragments to detect links.
- Navigate on link click.

### Milestone 5: tabs + history

- New tab / close tab / switch tab.
- Back/forward/reload working per tab.

## Input handling (event routing)

Even without JS, the browser UI needs basic DOM interaction:

- focus (click to focus input/select/textarea),
- typing into focused text controls,
- clicking buttons/links,
- selection in `<select>`.

Implementation approach:

- Add an interaction layer that can:
  - hit-test to identify the target element/fragment,
  - update a small “UI state” model (focused node, value text, selection),
  - trigger reflow/repaint when state changes.

Keep this strictly spec-shaped and incremental: don’t invent page-specific behaviors.

## Rendering integration notes

- The renderer already produces pixel output; the UI layer should consume an **RGBA buffer** directly (avoid PNG encode/decode).
- Prefer a stable “render to buffer” API (e.g. `render_html_to_rgba(...)`) even if initially implemented by refactoring the PNG path.
- Renders must be cancellable: when the user types a new URL or scrolls rapidly, cancel/skip stale work.

## Where code should live (suggested)

- `src/ui/` (browser app model, messages, tab/history, small helpers)
- `src/bin/browser.rs` (winit/egui integration + wiring)
- Renderer core remains in `src/` modules; avoid UI-specific hacks inside core rendering.
