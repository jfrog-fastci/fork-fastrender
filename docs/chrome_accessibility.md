# Chrome accessibility (AccessKit)

FastRender has two accessibility-related layers:

1. **Renderer semantics export** (FastRender → JSON): `src/accessibility.rs` builds a static accessibility tree derived from the styled DOM (`AccessibilityNode`). This is used by tests, the library API, and the `dump_a11y` CLI.
2. **OS accessibility** (browser UI → screen readers): the windowed `browser` app (feature `browser_ui`) uses **AccessKit** via egui’s `accesskit` support so the **browser chrome** (tabs/address bar/menus) is exposed to platform assistive tech (VoiceOver/Narrator/Orca).

Today these are *separate*: the rendered page content is a pixmap, so only the egui chrome participates in OS accessibility. This document describes the current AccessKit wiring and the conventions to follow so it stays maintainable, plus notes on how the renderer’s `accessibility.rs` output will eventually feed into AccessKit when chrome/content are rendered by FastRender.

---

## Overview: why AccessKit + where it lives

**AccessKit** is the cross-platform abstraction used to expose a semantic accessibility tree to the operating system.

In this repo, AccessKit is used in the **browser/UI process**:

- The windowed app entry point is [`src/bin/browser.rs`](../src/bin/browser.rs).
- AccessKit is pulled in behind the `browser_ui` feature (`Cargo.toml` `features.browser_ui` enables `accesskit` + `accesskit_winit` and egui’s accesskit integration).
- egui produces `accesskit::TreeUpdate` values (available on `egui::PlatformOutput::accesskit_update`), and the winit adapter delivers those to the OS accessibility API.

### Where AccessKit is wired in the windowed `browser` app

At a high level:

1. `egui_winit::State` is created in `App::new` (see [`src/bin/browser.rs`](../src/bin/browser.rs)) and
   owns the platform adapter glue (including AccessKit when egui-winit is built with its `accesskit`
   feature).
2. Each frame:
   - The UI calls `egui_ctx.end_frame()` to get `FullOutput`.
   - The UI forwards `platform_output` to winit via
     `egui_state.handle_platform_output(&window, &egui_ctx, platform_output)`.

That `handle_platform_output` call is the important “plumbing” point: it is responsible for applying
platform-side effects (clipboard, cursor, IME, **and AccessKit updates**). If you refactor the event
loop, ensure this still happens every frame.

Note: in headless unit tests (and in `dump_accesskit`), we explicitly call
`egui::Context::enable_accesskit()` so egui always emits a `TreeUpdate`. In the real windowed app,
AccessKit output is typically enabled/disabled by the platform adapter.

Renderer-side semantics live in [`src/accessibility.rs`](../src/accessibility.rs):

- `FastRender::accessibility_tree*` APIs expose semantics as `AccessibilityNode` (role/name/state/relations) without requiring layout/paint.
- This is what `dump_a11y` prints (see below).
- **Important:** `AccessibilityNode` currently has *no geometry*, so it is not sufficient by itself for OS accessibility (which needs bounds for hit-testing/focus rings, etc.).

---

## Coordinate systems (CSS px vs egui points vs physical px)

There are three coordinate systems you will see in the chrome + renderer stacks:

### 1) Viewport CSS pixels (FastRender “page-space”)

The render worker and interaction protocol speak in **viewport-local CSS pixels**:

- Origin is the **top-left of the viewport**.
- Coordinates **do not include scroll offset** (the worker adds the scroll offset internally when hit-testing against page-space).
- This convention is called out in [`docs/browser_ui.md`](browser_ui.md) and used throughout `UiToWorker`/`WorkerToUi`.

### 2) Physical pixels (raster output)

FastRender paints into a `tiny_skia::Pixmap` in **physical pixels**:

- pixmap dimensions are approximately `viewport_css * dpr` (plus any internal clamping/safety limits).
- This is what the `browser` UI uploads as a texture and draws as an image.

### 3) egui “points” (logical pixels)

egui layout/input uses **points** (logical pixels), with a scale factor `pixels_per_point`:

- winit reports pointer positions and some wheel deltas in physical pixels; egui converts them to points.
- `pixels_per_point` is usually derived from the OS scale factor, and can also be influenced by UI scaling preferences.

### How this matters for accessibility bounds

For the **current egui chrome tree**, bounds are handled by egui/accesskit-winit; most code should not manually compute AccessKit bounds.

For future **page/content accessibility** (or chrome rendered by FastRender), we will need to map renderer geometry to the window coordinate space that AccessKit expects. The same conversion already exists for input and overlay placement:

- [`src/ui/input_mapping.rs`](../src/ui/input_mapping.rs) maps between:
  - `viewport_css` (CSS px)
  - `image_rect_points` (where the pixmap is drawn in the egui UI, in points)

When you need “screen reader bounds for a layout rect”, the rule is:

1. Start with **viewport-local CSS px** rects (e.g. an element border box in viewport coordinates).
2. Convert to **egui points** using `InputMapping::rect_css_to_rect_points[_clamped]`.
3. Offset is automatically handled because `InputMapping` already incorporates the `image_rect_points.min` origin.
4. If the AccessKit adapter expects **physical pixels**, multiply points by `pixels_per_point` (or use the adapter’s helper if it exposes one).

In the windowed `browser` app, the “content offset” (toolbar height, menu bar, etc.) is reflected in
where the page pixmap is drawn:

- The UI records the page image rect in points (see `App::content_rect_points` in
  [`src/bin/browser.rs`](../src/bin/browser.rs)).
- `InputMapping { image_rect_points, viewport_css }` uses that rect as its origin, so conversions
  automatically include the chrome-to-content offset.

If bounds appear “shifted”:

- check whether you accidentally used **document/page-space** instead of **viewport-local** coordinates (viewport-local should already reflect scrolling),
- check whether you forgot the **content offset** (the page is drawn below the chrome toolbar),
- check whether you mixed **points** and **physical pixels** (`pixels_per_point` / `Window::scale_factor()` mismatches).

### Where viewport-local CSS rects come from (content)

FastRender has existing geometry helpers that already return **viewport-local CSS pixels** (i.e.
translated by `-scroll_state.viewport`), matching the convention needed for overlay placement and
event hit-testing:

- [`src/api/dom2_geometry.rs`](../src/api/dom2_geometry.rs): `Dom2GeometryContext::{border_box_in_viewport, padding_box_in_viewport, content_box_in_viewport, scrollport_box_in_viewport}`.

These are primarily used by DOM/JS APIs (IntersectionObserver/ResizeObserver-style geometry), but
they are also the kind of building blocks we’ll want when wiring content accessibility bounds into
AccessKit in the future.

---

## NodeId scheme (stability + collision avoidance)

AccessKit node ids are `NonZeroU128` (`accesskit::NodeId`). Stability matters: if ids change every frame, focus and screen-reader cursors will jump.

### Current: egui chrome ids

For the egui-based chrome UI, AccessKit ids are derived from **egui widget ids** (`egui::Id`).

Maintainability rules (to keep ids stable and avoid collisions):

- Prefer **explicit** ids (`ui.make_persistent_id(...)`, `egui::Id::new(...)`, `id.with(...)`) rather than relying on labels.
  - Example: the address bar uses `ui.make_persistent_id("address_bar")` in [`src/ui/chrome.rs`](../src/ui/chrome.rs).
- When a label changes based on state (“Details” → “Hide details”), keep the underlying id stable by scoping the widget in a stable parent id.
  - Example: tests in [`src/bin/browser.rs`](../src/bin/browser.rs) assert that a toggle keeps focus and id stable across label changes.
- Use `ui.push_id(...)` / `id.with(...)` to namespace repeating widgets (tab rows, menu items, list entries) so ids don’t collide.
- In tests, prefer **name/role snapshots** over comparing raw ids. AccessKit ids are implementation details and can change when egui’s internal hashing changes.
  - See [`src/ui/a11y_test_util.rs`](../src/ui/a11y_test_util.rs) helpers used by chrome/menu unit tests.

### Future: FastRender node ids

When chrome and/or page content are rendered by FastRender (instead of egui widgets), we will need to mint AccessKit ids from FastRender’s own node identifiers (e.g. DOM/styled/layout node ids).

Constraints to keep in mind (even if the exact scheme changes):

- Avoid collisions between:
  - chrome tree vs content tree
  - multiple tabs/documents
  - “virtual” wrapper nodes (window root, split panes, etc) vs DOM nodes
- Prefer a reversible mapping (helpful for debugging action routing): you should be able to recover `(tree_kind, tab_id, node_id)` from an AccessKit `NodeId` without a global hashmap where possible.

One reasonable (but **not implemented**) approach is to encode a small namespace header into the high bits of the `u128`, for example:

- high bits: `(tree_kind, tab_id)` (or a per-tab “document generation”)
- low bits: a stable FastRender node id (DOM/styled/layout id)

If/when this lands, update this section to match the real encoding and ensure it is documented in the code where ids are minted (so tooling can reverse-map ids during debugging).

---

## Action routing (what we support today, and where actions go)

### Current: egui chrome actions

Most actions (“click”, focus, keyboard activation) are handled by egui itself once widgets are correctly described.

Two patterns are used in this repo:

1. **Provide good names/roles** so default actions are meaningful:
   - Use `Response::widget_info(...)` to give icon-only controls an accessible label.
   - Many icon buttons should go through `crate::ui::BrowserIcon` + `crate::ui::icon_button` so labels are centralized (see [`src/ui/a11y.rs`](../src/ui/a11y.rs) and `BrowserIcon::a11y_label` in [`src/ui/icons.rs`](../src/ui/icons.rs)).
2. **Explicitly handle non-default AccessKit actions** for stateful widgets:
   - Some controls expose expanded/collapsed state and support `accesskit::Action::{Expand,Collapse}`.
   - In [`src/bin/browser.rs`](../src/bin/browser.rs), toggles check for requests via `has_accesskit_action_request(..., Action::Expand/Collapse)` and update their `accesskit_node_builder` to advertise the correct actions each frame.

If you add a new stateful chrome control, make sure:

- its egui id is stable (see NodeId section),
- it emits a meaningful label via `WidgetInfo`,
- if it has an “expanded” concept, it sets `expanded` state and exposes `Expand`/`Collapse` actions consistently.

### Future: mapping AccessKit actions into FastRender interaction

The renderer already has an interaction engine (focus, text editing, click dispatch, etc). When content/chrome are represented as FastRender nodes in the OS accessibility tree, AccessKit actions will need to be translated into the interaction protocol (e.g. `UiToWorker` messages) and/or direct interaction engine calls, depending on process boundaries.

Examples of likely mappings (not implemented today):

| AccessKit action | Typical browser behavior | FastRender-side target |
|---|---|---|
| `Default` | Activate (click) | dispatch click / submit / follow link |
| `Focus` | Move focus | focus the target node + update `InteractionState` |
| `SetValue` | Edit text field | update text edit state, selection, caret |
| `Scroll*` | Scroll container | translate to scroll deltas in viewport CSS px |

---

## Debugging

### `dump_a11y` (FastRender semantics)

`dump_a11y` prints FastRender’s computed accessibility tree as JSON (no OS integration, no bounds):

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin dump_a11y -- --help
```

Use this when you’re debugging **role/name/state** mapping coming out of `src/accessibility.rs`.

### `dump_accesskit` (OS-facing tree)

`dump_accesskit` prints the AccessKit update produced by the egui chrome layer:

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin dump_accesskit -- --help
```

Use this when you’re debugging what screen readers actually see for the **browser chrome** (widget names/roles, focus target, expanded state/actions, etc).

Common invocations:

```bash
# Dump only named nodes (noise-reduced).
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin dump_accesskit -- \
    --named-only

# Include the in-window menu bar (useful for menu role/name checks).
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin dump_accesskit -- \
    --show-menu-bar --named-only

# Force the address bar into “editing” mode so it appears as a text field.
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin dump_accesskit -- \
    --focus-address-bar --named-only
```

Output notes:

- The output is intentionally lossy and is meant to be “diff-friendly” for debugging.
- Each node includes:
  - `role` and `name` (primary debugging fields)
  - `expanded` plus `supports_expand` / `supports_collapse` (useful when debugging widgets that use
    explicit AccessKit `Expand`/`Collapse` action routing, e.g. details toggles/toasts)
  - a stringified `id` (helpful when correlating focus updates, but avoid asserting on ids unless
    you are explicitly debugging id stability)

### Unit tests (headless AccessKit snapshots)

Many chrome widgets have unit tests that force-enable AccessKit (`ctx.enable_accesskit()`) and assert on the emitted `TreeUpdate`.

Useful helpers live in [`src/ui/a11y_test_util.rs`](../src/ui/a11y_test_util.rs):

- `accesskit_names_from_full_output` / `accesskit_named_roles_from_full_output` (stable, ID-free)
- `accesskit_pretty_json_from_full_output` (full snapshot, includes ids)

Prefer the ID-free helpers unless you are explicitly debugging id stability.

### Screen reader smoke tests (manual)

The fastest “does this basically work?” check is a real screen reader:

- **macOS:** VoiceOver (`Cmd+F5`) → Tab through the toolbar/address bar, ensure labels are announced.
- **Windows:** Narrator (`Win+Ctrl+Enter`) → scan the toolbar controls.
- **Linux:** Orca (varies by distro) → ensure your build supports your winit backend (`browser_ui_wayland` if needed).

If you need a lower-level view than a screen reader, use platform accessibility inspection tools:

- **macOS:** Accessibility Inspector (Xcode → Open Developer Tool → Accessibility Inspector)
- **Windows:** Inspect.exe (Windows SDK) / Accessibility Insights
- **Linux:** Accerciser / other AT-SPI inspection tools

Current scope note: the rendered page is still an image, so the screen reader will not traverse document content yet (see future work below).

---

## Future work: composing chrome + content accessibility trees

Renderer-chrome (HTML/CSS chrome rendered by FastRender) ultimately wants a single OS-visible tree that contains:

- chrome UI (tabs, address bar, menus), and
- the active document content.

The hard parts to plan for:

- **Geometry composition:** content bounds must be offset into the window coordinate space (chrome height, splits, device scale).
- **Id namespaces:** ids must not collide across chrome/content/tabs/processes.
- **Action routing:** AccessKit actions on content nodes must reach the correct renderer/interaction instance (possibly over IPC in a multi-process model).

Until content accessibility is wired up, keep chrome AccessKit behavior healthy (stable ids, correct labels, correct focus) so we have a solid foundation to compose on top of later.
