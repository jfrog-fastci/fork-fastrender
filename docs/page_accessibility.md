# Page accessibility (status + developer workflow)

FastRender currently has **page accessibility semantics** (roles/names/states) implemented in the
renderer. This semantic tree (`AccessibilityNode`) is used by tests, the library API, and the
`dump_a11y` CLI.

The desktop `browser` UI exposes the **browser chrome** (tabs/address bar/menus) to OS screen readers
via **AccessKit**. Page content is still *visually* rendered as a pixel buffer; wiring the page
semantics tree into the OS-facing AccessKit tree is **in progress**.

However, the render worker already emits a page accessibility snapshot as part of the UI↔worker
protocol: `WorkerToUi::PageAccessibility` contains the semantic tree plus best-effort per-node bounds
in viewport-local CSS pixels. The windowed UI stores that snapshot as
`ui::browser_app::PageAccessibilitySnapshot` (for future AccessKit subtree injection and other UI
features), but not every build wires it into the OS-facing tree yet.

For deeper details on the browser chrome’s AccessKit wiring (including the experimental non-egui
backend), see [chrome_accessibility.md](chrome_accessibility.md).

This doc is a short, “what exists today” guide for:

- Inspecting the computed page accessibility tree.
- Understanding how page element bounds are computed (for UI overlays and eventual a11y bounds).
- Testing screen reader integration (browser chrome + page content).

## Current architecture (what runs today)

### Semantic tree (roles / names / states)

- Renderer builds a semantic accessibility tree in [`src/accessibility.rs`](../src/accessibility.rs).
  - Entry point: `build_accessibility_tree` (returns `AccessibilityNode`).
  - Output schema: [`AccessibilityNode`](../src/accessibility.rs) is `Serialize` and is what
    `dump_a11y` prints as JSON.
    - `AccessibilityNode.node_id` is the renderer’s 1-indexed pre-order id for the originating DOM
      node. It is intentionally **not** part of the stable JSON output (`#[serde(skip)]`), but it is
      used internally for mapping (for example the keys in `WorkerToUi::PageAccessibility.bounds_css`
      match these preorder ids).
  - Input: styled DOM (`StyledNode`) + optional [`InteractionState`](../src/interaction/state.rs) to
    populate dynamic state such as focus/selection (when available).

The public API entrypoint used by the CLI is:

- [`FastRender::accessibility_tree_fetched_html`](../src/api.rs) (see the call site in
  [`src/bin/dump_a11y.rs`](../src/bin/dump_a11y.rs)).

### Bounds / geometry (viewport CSS px → UI coords)

FastRender uses multiple coordinate spaces. The important ones for UI integration are:

- **Page-space CSS px**: layout/document coordinates (origin at the top-left of the document).
- **Viewport-local CSS px**: (0,0) is the visible viewport top-left; *excludes* scroll offset.
- **UI points / window coordinates**: what egui/winit and AccessKit ultimately work in.

Coordinate conventions that matter when extending bounds/a11y:

- **Page ↔ viewport conversion** (for non-fixed content):
  - `viewport = page - scroll_state.viewport`
  - `page = viewport + scroll_state.viewport`
- **Element scroll offsets** (`scrollLeft`/`scrollTop`) are applied by mutating a cloned fragment tree:
  - `crate::scroll::apply_scroll_offsets` (see [`src/scroll.rs`](../src/scroll.rs)).
- **Viewport-fixed scroll cancel** for `position: fixed` is applied when producing page-space geometry
  (so hit testing with `page_point = viewport_point + scroll_state.viewport` matches paint-time):
  - `crate::scroll::apply_viewport_scroll_cancel` (see [`src/scroll.rs`](../src/scroll.rs)).
- **Viewport scroll** is applied during paint via a global translation (`-scroll_state.viewport`), and
  `position: fixed` subtrees cancel that translation so they remain pinned:
  - see `needs_viewport_scroll_cancel` in [`src/paint/painter.rs`](../src/paint/painter.rs).
- For “paint-time geometry” (sticky- + scroll-container- + `position: fixed`-aware, but still in
  **page** coordinates), prefer [`PreparedDocument::fragment_tree_for_geometry`](../src/api.rs) and
  subtract `scroll_state.viewport` yourself when you need viewport-local CSS pixels.

How bounds are computed today (used for positioning UI popups like `<select>` dropdowns):

1. Compute absolute **page-space** fragment bounds for a `styled_node_id` by walking the fragment
   tree and unioning all fragments produced for any box associated with that styled node:
   - `crate::interaction::absolute_bounds_by_styled_node_id` in
     [`src/interaction/fragment_geometry.rs`](../src/interaction/fragment_geometry.rs).
2. Apply scroll offsets to the fragment tree (so element scroll containers are accounted for):
   - `crate::scroll::apply_scroll_offsets` (called from the UI worker helper).
3. Convert page-space → viewport-local by subtracting `scroll_state.viewport`.

The shared helper that implements this end-to-end (used by the UI worker and other runtime code) is:

- `styled_node_anchor_css` in [`src/interaction/anchor_geometry.rs`](../src/interaction/anchor_geometry.rs)
  (returns a `Rect` in **viewport-local CSS pixels**).

The UI then maps viewport-local CSS rects into egui/window coordinates via:

- [`src/ui/input_mapping.rs`](../src/ui/input_mapping.rs) (`InputMapping::rect_css_to_rect_points*`).
  - Note: this produces **egui points** (logical pixels). If you need physical window pixels for an
    OS-facing API, multiply by `egui_ctx.pixels_per_point()` / `Window::scale_factor()` as
    appropriate for the frontend.
  - The windowed `browser` UI builds an `InputMapping` per-frame from the egui `Rect` where the page
    image is drawn (`response.rect`) and the worker-reported `viewport_css`; see the `InputMapping::new(...)`
    call in [`src/bin/browser.rs`](../src/bin/browser.rs).

For more general-purpose, scroll- and sticky-aware geometry queries (mirroring the paint pipeline),
see [`src/api/dom2_geometry.rs`](../src/api/dom2_geometry.rs) (`Dom2GeometryContext::*_in_viewport`).

### Browser UI worker protocol (page a11y snapshot status)

The render worker sends frames and some element geometry (for popups/overlays).

Page content accessibility is surfaced both via `dump_a11y` (renderer semantics inspection) and via
the live UI worker protocol (`WorkerToUi::PageAccessibility`).

- UI↔worker messages: [`src/ui/messages.rs`](../src/ui/messages.rs)
- Worker loop: [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)

High-level flow:

- The worker builds a `RenderedFrame` (pixmap + scroll state + viewport metadata) and sends it to the
  UI via `WorkerToUi::FrameReady` (see `src/ui/messages.rs`).
- The worker can also send a semantic page accessibility snapshot via `WorkerToUi::PageAccessibility`
  (stored on each tab as `PageAccessibilitySnapshot`).
- The windowed UI renders the pixmap as an egui image widget.
- When layout artifacts exist, the worker also computes a **page accessibility snapshot** and sends
  it as `WorkerToUi::PageAccessibility { tree, bounds_css }`:
  - `tree`: the `AccessibilityNode` semantic tree (built in `src/accessibility.rs`, with
    `InteractionState` applied).
  - `bounds_css`: a mapping from DOM preorder node id → **viewport-local CSS rect**, derived from
    `PreparedDocument::fragment_tree_for_geometry(scroll_state)` and then translated into viewport
    coordinates by subtracting `scroll_state.viewport`.
  - The windowed UI stores this snapshot as `PageAccessibilitySnapshot` in `src/ui/browser_app.rs`.
- The UI has scaffolding to merge a future **page content subtree** into egui’s AccessKit output (see
  `App::merge_page_subtree_accesskit_update` in `src/bin/browser.rs`). This is the intended insertion
  point for exposing per-element page semantics to OS screen readers.
- AccessKit action requests targeted at page nodes are routed back to the worker as DOM interactions;
  today `BrowserTabController::handle_accesskit_action` implements `Focus` and `ScrollIntoView`
  (including best-effort focus scrolling when layout artifacts exist).

### AccessKit integration (browser chrome)

The desktop `browser` UI enables AccessKit for **egui widgets** (browser chrome).

- Shared chrome a11y helpers: [`src/ui/a11y.rs`](../src/ui/a11y.rs)
- Test utilities for extracting AccessKit output: [`src/ui/a11y_test_util.rs`](../src/ui/a11y_test_util.rs)
- Egui code uses `ctx.enable_accesskit()` in tests (search for `enable_accesskit` in
  `src/ui/chrome.rs`, `src/ui/menu_bar.rs`, etc).
- Page/content in the windowed UI is still drawn as a single **egui image widget**, with a stable
  accessible label (“Web page content (rendered image)”).
- There is scaffolding to inject a web content subtree (from `WorkerToUi::PageAccessibility`) into
  the AccessKit tree, but not every build/config wires up a page subtree yet (see “Browser UI worker
  protocol” above).
- Future-facing (page/chrome rendered by FastRender rather than egui):
  - AccessKit update gating (avoid building trees/bounds when no AT is connected):
    [`src/ui/accesskit_bridge.rs`](../src/ui/accesskit_bridge.rs)
  - Action routing (screen reader “press/focus/set value” → `InteractionEngine`):
    [`src/ui/fast_accesskit_actions.rs`](../src/ui/fast_accesskit_actions.rs)
  - Experimental runtime toggle: `FASTR_BROWSER_RENDERER_CHROME=1` (see
    [env-vars.md](env-vars.md#appearance--accessibility--debugging-browser-ux) and
    [chrome_accessibility.md](chrome_accessibility.md)).

Page content is still rendered as a bitmap in an egui panel. Until a page content subtree is wired
up, screen readers will typically only see a single labeled “page” node rather than per-element
semantics.

## Tooling: inspect the page accessibility tree

### `dump_a11y`

`dump_a11y` renders *just enough* of the pipeline to compute accessibility semantics (no painting),
then prints the tree as JSON.

- Entry: [`src/bin/dump_a11y.rs`](../src/bin/dump_a11y.rs)
  - Note: `dump_a11y` does **not** execute JavaScript (`--js` is not supported). It reflects the
    accessibility semantics of the input HTML/CSS as loaded; for JS-driven DOM changes, use a
    JS-capable container like `api::BrowserTab` (see [`docs/runtime_stacks.md`](runtime_stacks.md)).

Examples:

```bash
# Local HTML file (optionally with a sidecar .meta file)
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run -p fastrender --release --bin dump_a11y -- \
  tests/pages/fixtures/apple.com/index.html

# URL
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run -p fastrender --release --bin dump_a11y -- \
  https://example.com/

# Change viewport / DPR
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run -p fastrender --release --bin dump_a11y -- \
  --viewport 800x600 --dpr 2.0 https://example.com/

# Pipe into jq for quick inspection
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run -p fastrender --release --bin dump_a11y -- \
  https://example.com/ | jq '.role, .children[0].role'
```

Worked example (stable fixture + expected output):

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run -p fastrender --release --bin dump_a11y -- \
  tests/fixtures/accessibility/headings_links.html \
  | jq '.. | objects | select(.id? == "title") | {role,name,level,html_tag,id,states,children}'
```

Expected output snippet:

```json
{
  "role": "heading",
  "name": "Main Title",
  "level": 1,
  "html_tag": "h1",
  "id": "title",
  "states": {
    "focusable": false,
    "disabled": false,
    "required": false,
    "invalid": false,
    "visited": false,
    "readonly": false
  },
  "children": []
}
```

Notes:

- In agent/CI environments, prefer the repo wrappers for resource limits and consistent Cargo flags:
  `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run -p fastrender --release --bin dump_a11y -- ...`
- Cached HTML produced by `fetch_pages` (under `fetches/html/*.html`) can be passed directly; the tool
  will auto-load the `*.html.meta` sidecar when present.
- Debug-only fields:
  - In debug builds, `AccessibilityNode.debug` may be present (selection/caret state), gated by
    `#[cfg(debug_assertions)]`.
  - In release builds, build with `--features a11y_debug` to include the debug fields:

    ```bash
    timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
      bash scripts/cargo_agent.sh run -p fastrender --release --features a11y_debug --bin dump_a11y -- \
      https://example.com/
    ```

### `dump_a11y --include-bounds` (Task 141 / optional)

Some checkouts add a `--include-bounds` flag to `dump_a11y` (introduced after Task 141). Always
confirm the flag exists in your build by checking `dump_a11y --help`.

When enabled, the intent is:

- Compute **layout/fragment geometry** in addition to semantic roles/names/states.
- Include a per-node bounds field in the JSON output.

Coordinate conventions to expect (and to keep consistent with the UI/input pipeline):

- Bounds should be in **viewport-local CSS pixels** by default:
  - (0,0) is the viewport top-left.
  - Bounds do **not** include `ScrollState.viewport`; page-space coordinates can be recovered by
    adding the scroll offset.
- Element scroll offsets (`scrollLeft`/`scrollTop`) should be applied via
  `crate::scroll::apply_scroll_offsets`.
- `position: fixed` should remain pinned under viewport scroll (paint cancels the global viewport
  translation for fixed subtrees; see `needs_viewport_scroll_cancel` in `src/paint/painter.rs`).

Example (when available):

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run -p fastrender --release --bin dump_a11y -- \
  --include-bounds tests/fixtures/accessibility/headings_links.html | jq '.children[0]'
```

If you need bounds today and the flag is not available:

- `dump_a11y` builds semantics from the **styled DOM only** (no box generation / layout), so it does
  not have geometry available to report.
- Use the layout/interaction helpers described in “Bounds / geometry” above (especially
  `absolute_bounds_by_styled_node_id` + `styled_node_anchor_css`), or
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

Note: `render_html_with_accessibility` currently builds the accessibility tree with no explicit
`InteractionState` (focus/visited/selection). If you need those dynamic states populated, call
`FastRender::accessibility_tree_with_interaction_state(...)` directly (see `src/api.rs`).

### `dump_accesskit` (browser chrome / OS-facing a11y)

To inspect what the **windowed browser chrome UI** is exposing to the OS via AccessKit (separate
from the page semantics tree), use `dump_accesskit`:

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin dump_accesskit -- --help
```

This tool only snapshots the egui widget tree (tabs/address bar/menus). It does **not** run the
render worker, so it will not include any page/content subtree update (from
`WorkerToUi::PageAccessibility`, if/when that is wired); use the real windowed browser + a platform
accessibility inspector to validate page content exposure.

See [chrome_accessibility.md](chrome_accessibility.md) for recommended `dump_accesskit` invocations
and how to interpret the output.

## Testing guidance

### Integration tests: `tests/accessibility/**` (semantics + fixtures)

Most accessibility semantics coverage lives under `tests/accessibility/` and uses HTML+JSON fixtures
in `tests/fixtures/accessibility/`.

These tests are compiled into the unified integration test binary (`tests/integration.rs`), so run
them via:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration accessibility::
```

Helpful patterns:

- Fixtures are in `tests/fixtures/accessibility/*.html`.
- Many tests look up nodes by HTML `id=...` (matching the `AccessibilityNode.id` field in `dump_a11y`
  output).
- If you change the accessibility output schema or naming rules, expect to update fixtures and/or
  snapshots accordingly.

### Unit tests: semantics (tree correctness)

Accessibility semantics are validated by unit tests in:

- [`src/accessibility.rs`](../src/accessibility.rs) (`#[cfg(test)] mod tests`)

Run a focused subset:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender accessibility_
```

### Unit tests: geometry (bounds + mapping)

Geometry/bounds logic is validated separately from semantics:

- Fragment-tree bounds unioning / offsets:
  - [`src/interaction/fragment_geometry.rs`](../src/interaction/fragment_geometry.rs)
- UI mapping (viewport CSS px ↔ egui points):
  - [`src/ui/input_mapping.rs`](../src/ui/input_mapping.rs)

Focused runs:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender absolute_bounds_for_box_id
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender input_mapping
```

### AccessKit tests (browser chrome)

Chrome-level screen reader exposure is validated by tests that snapshot egui’s AccessKit output (when
compiled with `browser_ui`):

- `src/ui/chrome.rs`, `src/ui/chrome/tab_strip.rs`, `src/ui/menu_bar.rs`, plus helpers in
  `src/ui/a11y_test_util.rs`.

Run:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --features browser_ui accesskit
```

### Browser integration tests (a11y interaction plumbing)

Some browser-level tests exercise accessibility-adjacent interaction paths (e.g. selection actions
used by native controls / eventual AT action routing):

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration browser_integration::a11y_select_action
```

The UI worker also has an integration test that asserts it emits the `WorkerToUi::PageAccessibility`
snapshot (semantic tree + bounds) after navigating:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --features browser_ui --test integration browser_integration::ui_worker_page_accessibility
```

There are also focused AccessKit bridge tests:

- `tests/accesskit_dom2_node_ids.rs` (requires `--features a11y_accesskit`)
- `tests/accesskit_scroll.rs` (requires `--features browser_ui`)

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --features a11y_accesskit --test accesskit_dom2_node_ids
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --features browser_ui --test accesskit_scroll
```

## Known limitations (current gaps)

- **No JS-driven live region updates**: `dump_a11y` and most renderer-side tree export is a snapshot
  of the styled DOM at build time. ARIA live region events and incremental “tree update” delivery are
  not implemented yet.
- **No general per-node bounds in the semantic tree**: `AccessibilityNode` has no geometry; AccessKit
  bridge code often uses conservative default bounds today. Wiring real bounds requires layout +
  scroll/sticky transforms (see “Bounds / geometry” above).
- **Text selection/caret exposure is partial**:
  - Selection/caret state currently exists mainly for `<input>`/`<textarea>`.
  - Some selection metadata is debug-only (`#[cfg(debug_assertions)]` or `--features a11y_debug`).
- **Role/state coverage is incomplete**: many ARIA roles map to `"generic"` in the semantic tree or
  to a generic container role in AccessKit bridges until explicit mappings are added.

## Extending safely (guidelines)

- **Keep the JSON schema stable**:
  - `AccessibilityNode.node_id` is intentionally `#[serde(skip)]` so snapshot tests don’t depend on
    renderer preorder numbering.
  - Prefer adding **optional** fields (`Option<T>` + `#[serde(skip_serializing_if = "Option::is_none")]`)
    to avoid breaking downstream tooling.
  - Keep debug-only fields behind `#[cfg(any(debug_assertions, feature = "a11y_debug"))]`.
- **Thread `InteractionState` through the places that need dynamic state**:
  - The tree builder accepts `Option<&InteractionState>`; passing `None` means no focus/visited/selection.
  - If you add new dynamic states, update `src/interaction/state.rs` and add coverage in
    `tests/accessibility/**` (and/or browser integration tests).
- **Bounds work must respect scroll + fixed positioning**:
  - Prefer `PreparedDocument::fragment_tree_for_geometry` (page-space) + `ScrollState.viewport`
    subtraction (viewport-local).
  - Ensure element scroll offsets are applied (`scroll::apply_scroll_offsets`).
  - Add regression tests that cover viewport scroll + `position: fixed` and element scroll containers.
- **When bridging to AccessKit, preserve NodeId stability**:
  - If you derive AccessKit ids from DOM2 ids, ensure they are stable across DOM insertions (see
    `tests/accesskit_dom2_node_ids.rs`).
  - Avoid collisions between “wrapper” nodes (window/chrome/page region) and per-element nodes by
    using a clear namespacing scheme (see `ui::page_a11y::encode_page_node_id`; `ui::page_accesskit_ids`
    is an alternative tag-bit encoding).

## Manual testing with screen readers (current + future)

### Preconditions

- Build/run the windowed browser UI with AccessKit enabled (requires `browser_ui`):

  ```bash
  timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
    bash scripts/cargo_agent.sh run --features browser_ui --bin browser
  ```

  See [browser_ui.md](browser_ui.md) for platform prerequisites (system GUI deps).

### What you can validate today

- Screen readers should be able to navigate and announce **browser chrome controls** (tabs, address
  bar, toolbar buttons, menus).
- Page content is currently exposed as a **single labeled region/widget** in the egui tree (the page
  pixmap). When/if a page content subtree is wired into AccessKit, screen readers should be able to
  traverse page content nodes (headings/links/buttons) and trigger focus/activate.

### Inspecting the OS accessibility tree (optional)

Sometimes it’s faster to use an accessibility inspector to confirm what nodes/roles/names are being
exposed, instead of relying on screen reader speech alone.

- macOS: **Accessibility Inspector** (Xcode → Open Developer Tool → Accessibility Inspector)
- Windows: **Inspect.exe** (Windows SDK, UI Automation tree)
- Linux: **Accerciser** (AT-SPI tree)

Note: if/when the browser UI merges a page content subtree (derived from
`WorkerToUi::PageAccessibility`) into AccessKit output, these tools should show both the chrome tree
(egui widgets) and the injected page subtree.

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

### When page a11y subtree is wired

Manual testing should include:

- Screen reader “read all” on actual pages.
- Heading/landmark navigation.
- Link/button activation via keyboard.
- Text field editing (caret movement, selection announcements).
- Bounds correctness: focus rings / highlight overlays should match the element bounds reported to
  the OS.
