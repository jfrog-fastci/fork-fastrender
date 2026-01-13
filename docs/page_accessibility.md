# Page accessibility (status + developer workflow)

FastRender currently has **page accessibility semantics** (roles/names/states) implemented in the
renderer, but **page→OS screen reader integration is not wired yet**. The desktop `browser` UI does
use **AccessKit** for the *chrome* (egui widgets), so you can already regression-test that layer.

For deeper details on the browser chrome’s AccessKit wiring (including the experimental non-egui
backend), see [chrome_accessibility.md](chrome_accessibility.md).

This doc is a short, “what exists today” guide for:

- Inspecting the computed page accessibility tree.
- Understanding how page element bounds are computed (for UI overlays and eventual a11y bounds).
- Testing the existing screen reader integration (browser chrome), and what to do once page a11y is
  connected.

## Current architecture (what runs today)

### Semantic tree (roles / names / states)

- Renderer builds a semantic accessibility tree in [`src/accessibility.rs`](../src/accessibility.rs).
  - Entry point: `build_accessibility_tree` (returns `AccessibilityNode`).
  - Output schema: [`AccessibilityNode`](../src/accessibility.rs) is `Serialize` and is what
    `dump_a11y` prints as JSON.
  - Input: styled DOM (`StyledNode`) + optional [`InteractionState`](../src/interaction/state.rs) to
    populate dynamic state such as focus/selection (when available).

The public API entrypoint used by the CLI is:

- [`FastRender::accessibility_tree_fetched_html`](../src/api.rs) (see the call site in
  [`src/bin/dump_a11y.rs`](../src/bin/dump_a11y.rs)).

### Bounds / geometry (viewport CSS px → UI coords)

FastRender uses multiple coordinate spaces. The important ones for UI integration are:

- **Page-space CSS px**: layout output in document coordinates (includes scroll).
- **Viewport-local CSS px**: (0,0) is the visible viewport top-left; *excludes* scroll offset.
- **UI points / window coordinates**: what egui/winit and AccessKit ultimately work in.

How bounds are computed today (used for positioning UI popups like `<select>` dropdowns):

1. Find the first `BoxNode` produced by a given styled DOM node (`styled_node_id`).
2. Compute absolute **page-space** fragment bounds for that box id by walking the fragment tree and
   unioning matching fragments:
   - `crate::interaction::absolute_bounds_for_box_id` in
     [`src/interaction/fragment_geometry.rs`](../src/interaction/fragment_geometry.rs).
3. Apply scroll offsets to the fragment tree (so element scroll containers are accounted for):
   - `crate::scroll::apply_scroll_offsets` (called from the UI worker helper).
4. Convert page-space → viewport-local by subtracting `scroll_state.viewport`.

The UI worker’s helper that implements this end-to-end is:

- `styled_node_anchor_css` in [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)
  (returns a `Rect` in **viewport-local CSS pixels**).

The UI then maps viewport-local CSS rects into egui/window coordinates via:

- [`src/ui/input_mapping.rs`](../src/ui/input_mapping.rs) (`InputMapping::rect_css_to_rect_points*`).
  - Note: this produces **egui points** (logical pixels). If you need physical window pixels for an
    OS-facing API, multiply by `egui_ctx.pixels_per_point()` / `Window::scale_factor()` as
    appropriate for the frontend.

For more general-purpose, scroll- and sticky-aware geometry queries (mirroring the paint pipeline),
see [`src/api/dom2_geometry.rs`](../src/api/dom2_geometry.rs) (`Dom2GeometryContext::*_in_viewport`).

### Browser UI worker protocol (page a11y snapshot status)

The render worker currently sends frames and some element geometry (for popups/overlays), but it does
**not** currently send a full page accessibility snapshot.

- UI↔worker messages: [`src/ui/messages.rs`](../src/ui/messages.rs)
- Worker loop: [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)

When page accessibility is wired into the browser UI, the intended shape is:

- Add a `WorkerToUi::PageAccessibility { ... }` message in `src/ui/messages.rs` that carries:
  - The `AccessibilityNode` tree (semantics).
  - Per-node bounds in viewport-local CSS px (geometry), if/when bounds are implemented.
- UI maps those bounds through `InputMapping` into whatever coordinate space AccessKit needs.
  - In practice, the worker will likely reuse existing library plumbing:
    - Semantics: `RenderOptions::with_accessibility(true)` (see `FastRender::render_html_with_accessibility`)
      or `FastRender::accessibility_tree_with_interaction_state(...)`.
    - Construction should be gated on whether assistive tech is connected (see
      [`src/ui/accesskit_bridge.rs`](../src/ui/accesskit_bridge.rs)) so normal browsing doesn’t pay
      the full a11y build cost.

(As of this commit, `WorkerToUi::PageAccessibility` does **not** exist yet; the name is included here
to document the intended routing point.)

### AccessKit integration (browser chrome)

The desktop `browser` UI enables AccessKit for **egui widgets** (browser chrome).

- Shared chrome a11y helpers: [`src/ui/a11y.rs`](../src/ui/a11y.rs)
- Test utilities for extracting AccessKit output: [`src/ui/a11y_test_util.rs`](../src/ui/a11y_test_util.rs)
- Egui code uses `ctx.enable_accesskit()` in tests (search for `enable_accesskit` in
  `src/ui/chrome.rs`, `src/ui/menu_bar.rs`, etc).
- Future-facing (page/chrome rendered by FastRender rather than egui):
  - AccessKit update gating (avoid building trees/bounds when no AT is connected):
    [`src/ui/accesskit_bridge.rs`](../src/ui/accesskit_bridge.rs)
  - Action routing (screen reader “press/focus/set value” → `InteractionEngine`):
    [`src/ui/fast_accesskit_actions.rs`](../src/ui/fast_accesskit_actions.rs)
  - Experimental runtime toggle: `FASTR_BROWSER_RENDERER_CHROME=1` (see
    [env-vars.md](env-vars.md#appearance--accessibility--debugging-browser-ux) and
    [chrome_accessibility.md](chrome_accessibility.md)).

Page content is currently rendered as a bitmap in an egui panel, so screen readers can only see the
chrome tree today.

## Tooling: inspect the page accessibility tree

### `dump_a11y`

`dump_a11y` renders *just enough* of the pipeline to compute accessibility semantics (no painting),
then prints the tree as JSON.

- Entry: [`src/bin/dump_a11y.rs`](../src/bin/dump_a11y.rs)

Examples:

```bash
# Local HTML file (optionally with a sidecar .meta file)
cargo run --bin dump_a11y -- tests/pages/fixtures/apple.com/index.html

# URL
cargo run --bin dump_a11y -- https://example.com/

# Change viewport / DPR
cargo run --bin dump_a11y -- --viewport 800x600 --dpr 2.0 https://example.com/

# Pipe into jq for quick inspection
cargo run --bin dump_a11y -- https://example.com/ | jq '.role, .children[0].role'
```

Notes:

- In agent/CI environments, prefer the repo wrappers for resource limits and consistent Cargo flags:
  `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin dump_a11y -- ...`
- Cached HTML produced by `fetch_pages` (under `fetches/html/*.html`) can be passed directly; the tool
  will auto-load the `*.html.meta` sidecar when present.
- Debug-only fields:
  - In debug builds, `AccessibilityNode.debug` may be present (selection/caret state), gated by
    `#[cfg(debug_assertions)]`.
  - In release builds, build with `--features a11y_debug` to include the debug fields:

    ```bash
    cargo run --release --features a11y_debug --bin dump_a11y -- https://example.com/
    ```

### `dump_a11y --include-bounds` (not implemented yet)

There is currently **no** `--include-bounds` flag. If you need bounds today:

- `dump_a11y` builds semantics from the **styled DOM only** (no box generation / layout), so it does
  not have geometry available to report.
- Use the layout/interaction helpers described in “Bounds / geometry” above (especially
  `absolute_bounds_for_box_id` + `styled_node_anchor_css`), or
- Add a new tool flag and ensure it is covered by unit tests in the geometry modules.

If you just need to **inspect layout rectangles** for a particular node, `inspect_frag` is usually
the quickest route (it can dump fragment bounds and render overlay PNGs). See [cli.md](cli.md) for
`inspect_frag` usage.

### Library API: capture accessibility output while rendering

If you’re writing tests/tools in Rust and want the page accessibility tree alongside a rendered
pixmap, use `RenderOptions::with_accessibility(true)`:

```rust,no_run
use fastrender::api::{FastRender, RenderOptions};

let mut renderer = FastRender::new()?;
let options = RenderOptions::new()
  .with_viewport(800, 600)
  .with_accessibility(true);

let (_pixmap, a11y_tree) = renderer.render_html_with_accessibility("<button>OK</button>", options)?;
println!("{}", serde_json::to_string_pretty(&a11y_tree)?);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### `dump_accesskit` (browser chrome / OS-facing a11y)

To inspect what the **windowed browser chrome UI** is exposing to the OS via AccessKit (separate
from the page semantics tree), use `dump_accesskit`:

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin dump_accesskit -- --help
```

This tool only reflects the egui widget tree (tabs/address bar/menus). It is useful as a regression
test/debugging aid when working on screen reader support for the browser UI itself; page content is
still a bitmap and is not represented in AccessKit yet.

See [chrome_accessibility.md](chrome_accessibility.md) for recommended `dump_accesskit` invocations
and how to interpret the output.

## Testing guidance

### Unit tests: semantics (tree correctness)

Accessibility semantics are validated by unit tests in:

- [`src/accessibility.rs`](../src/accessibility.rs) (`#[cfg(test)] mod tests`)

Run a focused subset:

```bash
cargo test accessibility_
```

### Unit tests: geometry (bounds + mapping)

Geometry/bounds logic is validated separately from semantics:

- Fragment-tree bounds unioning / offsets:
  - [`src/interaction/fragment_geometry.rs`](../src/interaction/fragment_geometry.rs)
- UI mapping (viewport CSS px ↔ egui points):
  - [`src/ui/input_mapping.rs`](../src/ui/input_mapping.rs)

Focused runs:

```bash
cargo test absolute_bounds_for_box_id
cargo test input_mapping
```

### AccessKit tests (browser chrome)

Chrome-level screen reader exposure is validated by tests that snapshot egui’s AccessKit output (when
compiled with `browser_ui`):

- `src/ui/chrome.rs`, `src/ui/chrome/tab_strip.rs`, `src/ui/menu_bar.rs`, plus helpers in
  `src/ui/a11y_test_util.rs`.

Run:

```bash
cargo test --features browser_ui accesskit
```

## Manual testing with screen readers (current + future)

### Preconditions

- Build/run the windowed browser UI with AccessKit enabled (requires `browser_ui`):

  ```bash
  bash scripts/run_limited.sh --as 64G -- \
    bash scripts/cargo_agent.sh run --features browser_ui --bin browser
  ```

  See [browser_ui.md](browser_ui.md) for platform prerequisites (system GUI deps).

### What you can validate today

- Screen readers should be able to navigate and announce **browser chrome controls** (tabs, address
  bar, toolbar buttons, menus).
- Page content is *not* exposed yet (rendered as a bitmap).

### Inspecting the OS accessibility tree (optional)

Sometimes it’s faster to use an accessibility inspector to confirm what nodes/roles/names are being
exposed, instead of relying on screen reader speech alone.

- macOS: **Accessibility Inspector** (Xcode → Open Developer Tool → Accessibility Inspector)
- Windows: **Inspect.exe** (Windows SDK, UI Automation tree)
- Linux: **Accerciser** (AT-SPI tree)

Note: today these tools will mostly show the browser chrome (egui widgets). The rendered page is a
bitmap so it won’t expose a rich web content subtree yet.

### macOS: VoiceOver

1. Enable VoiceOver: `Cmd+F5`.
2. Launch `browser`.
3. Use VoiceOver navigation (VO+Arrow keys) to move through the window controls.
4. Confirm key controls have meaningful labels (e.g. “Address bar”, “Back”, “Forward”, “New tab”).

### Windows: Narrator

1. Enable Narrator: `Win+Ctrl+Enter`.
2. Launch `browser`.
3. Use `Tab` / `Shift+Tab` to move focus through chrome controls and ensure Narrator announces them.

### Linux: Orca

1. Enable Orca (varies by distro; commonly `Alt+Super+S`).
2. Launch `browser`.
3. Use `Tab` / arrow-key navigation and ensure Orca announces chrome widgets.

### Once page a11y is wired

When page content starts emitting an AccessKit tree, manual testing should expand to:

- Screen reader “read all” on actual pages.
- Heading/landmark navigation.
- Link/button activation via keyboard.
- Text field editing (caret movement, selection announcements).
- Bounds correctness: focus rings / highlight overlays should match the element bounds reported to
  the OS.
