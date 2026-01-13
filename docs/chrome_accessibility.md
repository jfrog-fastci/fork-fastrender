# Chrome accessibility (AccessKit)

FastRender has two accessibility-related layers:

1. **Renderer semantics export** (FastRender → JSON): `src/accessibility.rs` builds a static accessibility tree derived from the styled DOM (`AccessibilityNode`). This is used by tests, the library API, and the `dump_a11y` CLI.
2. **OS accessibility** (browser UI → screen readers): the windowed `browser` app (feature `browser_ui`) uses **AccessKit** so the **browser chrome** (tabs/address bar/menus) is exposed to platform assistive tech (VoiceOver/Narrator/Orca). The render worker can also emit a page accessibility snapshot (`WorkerToUi::PageAccessibility`, stored as `ui::browser_app::PageAccessibilitySnapshot`) that includes the semantic tree and best-effort bounds. Injecting a per-element page content subtree into the same OS-facing AccessKit tree is still in progress.
   - **Default backend:** egui’s AccessKit integration (the egui widget tree becomes the OS accessibility tree).
   - **Renderer-chrome backend (experimental):** `FASTR_BROWSER_RENDERER_CHROME=1` switches the browser to a custom `accesskit_winit::Adapter` (`ui::compositor_accessibility::CompositorAccessibility`) intended for when the chrome is rendered by FastRender (HTML/CSS) instead of egui. Today this provides a minimal Window/Chrome/Page region tree so platform assistive tech can still discover the main UI regions.

Visually, the rendered page content is still a pixmap. The worker-produced page snapshot
(`WorkerToUi::PageAccessibility`) is already part of the UI↔worker protocol, but wiring that
snapshot into egui’s `PlatformOutput.accesskit_update` as a full per-element page subtree is still
in progress.

This document describes the current AccessKit wiring and the conventions to follow so it stays
maintainable, plus notes on how the renderer’s `accessibility.rs` output feeds into AccessKit for
page content and will eventually do the same when chrome/content are rendered by FastRender.

For a page-focused workflow doc (inspecting the renderer’s accessibility tree via `dump_a11y`, how
viewport CSS bounds are computed/mapped, and manual screen reader testing), see
[page_accessibility.md](page_accessibility.md).

---

## Overview: why AccessKit + where it lives

**AccessKit** is the cross-platform abstraction used to expose a semantic accessibility tree to the operating system.

In this repo, AccessKit is used in the **browser/UI process**:

- The windowed app entry point is [`src/bin/browser.rs`](../src/bin/browser.rs).
- AccessKit is pulled in behind the `browser_ui` feature (`Cargo.toml` `features.browser_ui` enables `accesskit` + `accesskit_winit` and egui’s accesskit integration).
- With the **egui backend**, egui produces `accesskit::TreeUpdate` values (available on `egui::PlatformOutput::accesskit_update`), and the winit adapter delivers those to the OS accessibility API.
- With the **renderer-chrome backend**, FastRender (not egui) is responsible for producing `accesskit::TreeUpdate` updates and feeding them into `accesskit_winit::Adapter`.

### Where AccessKit is wired in the windowed `browser` app

There are two wiring paths, depending on which chrome accessibility backend is active.

#### Egui backend (default)

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

Note: the windowed browser enables AccessKit output by default (see `new_browser_egui_context` in
[`src/bin/browser.rs`](../src/bin/browser.rs), which calls `egui::Context::enable_accesskit()`), and
headless unit tests / `dump_accesskit` rely on that so egui always emits a `TreeUpdate`.

#### Renderer-chrome backend (experimental / FastRender-driven)

When `FASTR_BROWSER_RENDERER_CHROME=1` is set, `App::new` selects `ChromeA11yBackend::FastRender` and
creates a `ui::compositor_accessibility::CompositorAccessibility` wrapper around
`accesskit_winit::Adapter` (see [`src/ui/compositor_accessibility.rs`](../src/ui/compositor_accessibility.rs)
and wiring in [`src/bin/browser.rs`](../src/bin/browser.rs)).

This is the integration point for “chrome rendered by FastRender” (HTML/CSS) where egui widgets no
longer exist, so egui cannot generate the OS accessibility tree.

Key plumbing points:

- **Window events:** `App::handle_winit_input_event` forwards every `WindowEvent` to the adapter via
  `CompositorAccessibility::on_window_event` so AccessKit can track focus, window activation, etc.
- **Tree updates:** every rendered frame calls `App::update_chrome_accesskit_tree()`, which calls
  `CompositorAccessibility::update_if_active(&CompositorA11yState)` to keep bounds + names in sync.
- **Action requests:** OS “press/focus/set value” requests arrive as
  `accesskit_winit::ActionRequestEvent` user events, converted into `UserEvent::AccessKitAction` and
  routed to `App::handle_accesskit_action_request`.

Current status note: the compositor tree is intentionally minimal (Window → chrome region + page
region). Only `Action::Focus` is routed today so assistive tech can move focus between chrome and
page. When the real FastRender-rendered chrome document is wired into the windowed browser, action
routing should be extended to translate `Default`/`SetValue` etc into the chrome interaction engine.

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
  - For tests that merge or inject additional AccessKit nodes (e.g. page/content subtree injection), use the reachability helpers (`AccessKitTestTree`, `accesskit_reachable_*`, `accesskit_connectivity_*`) to assert that injected nodes are connected to the root (no orphans / target nodes are reachable).

#### Dynamic list rows: avoid auto-generated egui ids (omnibox suggestions, tab search results)

**Do not** rely on egui’s auto-generated ids (`ui.allocate_*` / `ui.add` without an explicit id) for list rows whose contents can change between frames (items added/removed/reordered).

Why this matters:

- In the egui backend, **AccessKit `NodeId`s are derived from egui ids**. When a row’s egui id changes, AccessKit sees the old node disappear and a new one appear.
- Screen readers often keep a “virtual cursor” / focus target by node id. If ids churn:
  - accessibility focus can jump unexpectedly,
  - screen readers may repeatedly re-announce “new” rows while navigating,
  - pending action requests can target stale nodes.

Required conventions for dynamic rows:

- Give each row an id derived from a **stable key** (tab id, suggestion id/key), not the row’s index.
  - Indices are only stable if the list never reorders or changes length ahead of the item, which is not true for omnibox suggestion lists.
- Prefer `ui.interact(rect, id, sense)` with an explicit id:
  - `let id = ui.make_persistent_id(("omnibox_row", suggestion_key));`
  - `let response = ui.interact(rect, id, egui::Sense::click());`
- Alternatively, wrap each row in `ui.push_id(stable_key, |ui| { ... })` so any nested auto ids are scoped by a stable parent.

If you need to validate id stability, use `dump_accesskit` for manual inspection and add/extend headless tests like
`error_infobar_details_toggle_keeps_focus_and_id_when_label_changes` in [`src/bin/browser.rs`](../src/bin/browser.rs).

### FastRender node ids (renderer-chrome / page content)

When chrome and/or page content are rendered by FastRender (instead of egui widgets), we mint
AccessKit ids from FastRender’s own stable node identifiers (e.g. DOM pre-order ids).

FastRender DOM nodes have stable **1-indexed pre-order ids** (`crate::dom::enumerate_dom_ids`), but
we cannot store them “as-is” in `accesskit::NodeId` because:

- chrome wrapper nodes (window/chrome/page placeholders) also need stable ids, and
- multiple documents/tabs will reuse DOM ids starting at `1`.

To avoid collisions (and to make action routing reversible without a global hashmap), page/content
nodes use a packed `u128` encoding implemented in:

- [`src/ui/page_a11y.rs`](../src/ui/page_a11y.rs) (`encode_page_node_id` / `decode_page_node_id`)

#### Encoding (page/content nodes)

Layout (u128):

- bits 127..64: `TabId` (u64, non-zero)
- bits 63..32: document generation (u32)
- bits 31..0: DOM pre-order node id (u32; clamped)

This gives each node a stable `(tab_id, generation, dom_node_id)` identity and ensures stale action
requests from a previous navigation can be ignored (generation mismatch).

#### Wrapper nodes (window/chrome/page roots)

Wrapper/root nodes in the compositor/renderer-chrome accessibility tree (see
[`src/ui/compositor_accessibility.rs`](../src/ui/compositor_accessibility.rs)) use small fixed ids
in the “non-page” namespace (upper 64 bits are `0`, so `decode_page_node_id` returns `None`).

This guarantees that DOM node id `1` in any tab will never collide with wrapper/root ids like `1`/`2`/`3`.

---

## Action routing (what we support today, and where actions go)

### Current: egui chrome actions

Most actions (“click”, focus, keyboard activation) are handled by egui itself once widgets are correctly described.

Two patterns are used in this repo:

1. **Provide good names/roles** so default actions are meaningful:
   - Use `Response::widget_info(...)` to give icon-only controls an accessible label.
   - Many icon buttons should go through `crate::ui::BrowserIcon` + `crate::ui::icon_button` so labels are centralized (see [`src/ui/a11y.rs`](../src/ui/a11y.rs) and `BrowserIcon::a11y_label` in [`src/ui/icons.rs`](../src/ui/icons.rs)).
2. **Expose stateful widget semantics** (state + any non-default actions):
   - Popup opener buttons should expose `expanded` state and support `accesskit::Action::{Expand,Collapse}`.
   - Toggle buttons should expose a pressed/checked state (don’t rely on swapping icons/text alone).
   - Tabs should expose selected state.
   - In [`src/bin/browser.rs`](../src/bin/browser.rs), expandable toggles check for requests via `has_accesskit_action_request(..., Action::Expand/Collapse)` and update their `accesskit_node_builder` to advertise the correct actions each frame.

If you add a new stateful chrome control, make sure:

- its egui id is stable (see NodeId section),
- it emits a meaningful label via `WidgetInfo`,
- if it has an “expanded” concept, it sets `expanded` state and exposes `Expand`/`Collapse` actions consistently,
- if it is a toggle, it exposes pressed/checked state,
- if it is part of a tab strip, it exposes selected state.

### Stateful chrome controls (expanded / pressed / selected)

Screen readers rely on explicit state properties on AccessKit nodes. Swapping labels (“Bookmark this page” → “Remove bookmark”) or swapping icons (outline → filled) is not a substitute for exposing state.

This repo’s egui backend uses two key APIs:

- `egui::Context::accesskit_node_builder(id, |builder| { ... })` — mutate AccessKit node properties for a widget each frame (state, role, supported actions).
- `egui::InputState::has_accesskit_action_request(id, action)` — detect OS-originated action requests (e.g. `Expand`/`Collapse`) and update your chrome state accordingly.

#### Popup opener buttons (Menu, Appearance, ...)

Buttons that open/close a popup (menu, appearance panel, tab search overlay, etc) must:

- expose `expanded=true/false` on the opener node, and
- expose **exactly one** of `Expand` / `Collapse` as a supported action (matching the current state).

Use the same pattern as the expandable controls in [`src/bin/browser.rs`](../src/bin/browser.rs) (warning toast title, error infobar details toggle):

```rust
let expand_requested = ui.input(|i| {
  i.has_accesskit_action_request(opener.id, accesskit::Action::Expand)
});
let collapse_requested = ui.input(|i| {
  i.has_accesskit_action_request(opener.id, accesskit::Action::Collapse)
});

if expand_requested {
  popup_open = true;
  opener.request_focus();
} else if collapse_requested {
  popup_open = false;
  opener.request_focus();
}

let _ = opener.ctx.accesskit_node_builder(opener.id, |builder| {
  builder.set_expanded(popup_open);
  if popup_open {
    builder.add_action(accesskit::Action::Collapse);
    builder.remove_action(accesskit::Action::Expand);
  } else {
    builder.add_action(accesskit::Action::Expand);
    builder.remove_action(accesskit::Action::Collapse);
  }
});
```

Tests to reference:

- `error_infobar_details_toggle_exposes_expanded_state_and_expand_collapse_actions` in [`src/bin/browser.rs`](../src/bin/browser.rs)

#### Toggle buttons (bookmark star, case-sensitive, ...)

If a control represents an on/off state, expose that state explicitly:

- Use a pressed/checked state on the AccessKit node (and keep a stable egui id).
- Pick the semantic that matches the control’s role:
  - “toggle button” style controls (e.g. bookmark star) should expose a **pressed**-like state,
  - checkbox-style controls should expose a **checked** state.

When implementing custom-painted toggles via `ui.interact`, you will typically need to set this with `accesskit_node_builder` (egui does not infer toggle semantics from “icon changed”).

#### Tabs

Tabs should expose which tab is currently selected:

- Ensure each tab node has a stable id derived from the tab id (not its index in the strip).
- Expose `selected=true` for the active tab, `selected=false` for all others.

This keeps screen-reader navigation predictable when the active tab changes, and allows assistive tech to announce which tab is selected.

### Suggestion lists (omnibox, tab search)

For “typeahead” UIs where **keyboard focus stays in a text input** but a **list of suggestions/results**
appears underneath, model the accessibility tree like a standard ARIA combobox:

- **Input**: expose as `Role::SearchBox` (AccessKit 0.11) / `Role::ComboBox` (when available) and keep
  its `expanded` state in sync with dropdown visibility.
- **List**: expose the suggestions/results container as `Role::ListBox` with a stable, descriptive
  name (for example: “Omnibox suggestions” / “Tab search results”).
- **Rows**: expose each row as `Role::ListBoxOption` (AccessKit 0.11) / `Role::Option` (when available)
  and set `selected=true` for the highlighted row.
- **Focus behavior**: keep egui focus on the input; use **active-descendant** on the input to point
  to the currently highlighted option so screen readers announce selection changes without moving
  focus into the list.

In egui, prefer doing this by overriding semantics via `ctx.accesskit_node_builder(id, |builder| { … })`
so the widget layout/interaction behavior stays unchanged while the OS accessibility tree matches
combobox/listbox conventions.

### FastRender documents: mapping AccessKit actions into the interaction engine (prototype)

For FastRender-rendered UI (renderer-chrome and, eventually, page content), AccessKit action requests
need to become FastRender interaction operations.

The current shared helper is [`src/ui/fast_accesskit_actions.rs`](../src/ui/fast_accesskit_actions.rs):

- `handle_accesskit_action_request(ctx, request)` decodes the target `NodeId` into a FastRender DOM
  node id (see NodeId scheme above).
- It then translates a subset of AccessKit actions into `InteractionEngine` calls and marks
  `needs_redraw` when state changes.

Actions currently handled by the helper:

| AccessKit action | FastRender-side dispatch |
|---|---|
| `Focus` | `InteractionEngine::focus_node_id(..., Some(node_id), true)` |
| `Default` | focus the node, then `InteractionEngine::key_activate(..., KeyAction::Enter, document_url, base_url)` |
| `SetValue` | focus the node, then `InteractionEngine::set_text_control_value(dom, node_id, value)` |
| `Expand` / `Collapse` | best-effort: toggles `<details open>` or updates `aria-expanded="true/false"` |
| `Increment` / `Decrement` | focus the node, then `InteractionEngine::key_action_with_box_tree(..., KeyAction::{ArrowUp,ArrowDown})` (requires a `BoxTree`) |

Unrecognized actions return `false` (ignored).

Note: the windowed browser’s renderer-chrome backend currently only routes `Action::Focus` for the
chrome/page wrapper nodes. When wiring up a real FastRender chrome document, `App::handle_accesskit_action_request`
should route relevant actions into `fast_accesskit_actions` (or a successor) so screen reader
activation drives the same interaction paths as pointer/keyboard input.

The renderer already has an interaction engine (focus, text editing, click dispatch, etc). When
content/chrome are represented as FastRender nodes in the OS accessibility tree, AccessKit actions
will need to be translated into the interaction protocol (e.g. `UiToWorker` messages) and/or direct
interaction engine calls, depending on process boundaries.

Additional likely mappings (not implemented today) include scroll actions (e.g. mapping `Scroll*`
into viewport-local scroll deltas in CSS px).

---

## Debugging

### `dump_a11y` (FastRender semantics)

`dump_a11y` prints FastRender’s computed accessibility tree as JSON (semantics only; not an
OS-facing AccessKit tree, and no bounds):

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

Limitations:

- `dump_accesskit` only snapshots the **egui** backend (`egui::PlatformOutput::accesskit_update`). It
  does not exercise the renderer-chrome backend (`ui::compositor_accessibility::CompositorAccessibility`).
- `dump_accesskit` does **not** run the browser worker, so it will not include any injected page
  content subtree update (from `WorkerToUi::PageAccessibility`, if/when wired). Use the real windowed
  `browser` + a platform accessibility inspector when debugging page nodes.

Note: on Linux, building with `--features browser_ui` requires system GUI development headers
(X11/Wayland headers, EGL/Vulkan, etc). Real-time audio output via `--features audio_cpal`
additionally requires ALSA headers (`libasound2-dev`). (The `browser_ui` feature does not enable
`audio_cpal` by default.) See
[`docs/browser_ui.md#platform-prerequisites`](browser_ui.md#platform-prerequisites).

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
  - common state flags when present:
    - `selected`
    - `checked` / `toggled` / `pressed`
    - `disabled`
    - `value` / `numeric_value` (for value-bearing controls like text fields and sliders)
  - a stringified `id` (helpful when correlating focus updates, but avoid asserting on ids unless
    you are explicitly debugging id stability)
- The top-level snapshot also includes:
  - `root_id` (the current tree root node id, when egui emits a tree update)
  - `focus_id` (the AccessKit node id that currently has accessibility focus, when any)

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

Scope note: the rendered page is still a pixmap. The windowed UI has scaffolding to inject a page
content subtree (derived from `WorkerToUi::PageAccessibility`) into the OS-facing AccessKit tree so
screen readers can traverse basic document content, but per-element page exposure is still in
progress. Action/bounds completeness is still evolving; see
[`docs/browser_ui.md`](browser_ui.md) for the current limitations checklist.

To smoke-test the **renderer-chrome AccessKit adapter path** (even before a real chrome document is
wired up), run the windowed browser with:

```bash
FASTR_BROWSER_RENDERER_CHROME=1 \
  bash scripts/run_limited.sh --as 64G -- \
    bash scripts/cargo_agent.sh run --features browser_ui --bin browser
```

Then inspect with a screen reader / platform accessibility inspector and ensure the app exposes an
accessibility tree without panics. (Today it is a placeholder tree; this is purely a plumbing check.)

---

## Future work: composing chrome + content accessibility trees

With the default egui backend, the windowed browser composes the egui chrome widget tree with a page
host region. There is also scaffolding to inject a richer page/content subtree. Renderer-chrome
(HTML/CSS chrome rendered by FastRender) still needs
to provide an equivalent single OS-visible tree without relying on egui’s widget/accessibility
integration.

Renderer-chrome (HTML/CSS chrome rendered by FastRender) ultimately wants a single OS-visible tree that contains:

- chrome UI (tabs, address bar, menus), and
- the active document content.

The hard parts to plan for:

- **Geometry composition:** content bounds must be offset into the window coordinate space (chrome height, splits, device scale).
- **Id namespaces:** ids must not collide across chrome/content/tabs/processes.
- **Action routing:** AccessKit actions on content nodes must reach the correct renderer/interaction
  instance (possibly over IPC in a multi-process model).

Keep chrome AccessKit behavior healthy (stable ids, correct labels, correct focus) so we have a
solid foundation to compose on top of—both for the current egui backend and for future
renderer-chrome/multiprocess work.
