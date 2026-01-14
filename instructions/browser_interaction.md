# Workstream: Browser Page Interaction

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

### Build speed matters

See [`docs/build_performance.md`](../docs/build_performance.md). Key rules:
- Use `cargo check` for validation (10-50x faster)
- Use `--release` for performance testing (fast, no LTO)
- Use `--features browser_ui --bin browser` when testing browser interaction

---

This workstream owns how users **interact with page content**: forms, text selection, focus, scrolling, and input handling.

## The job

Make page interaction **feel native and reliable**. Users should be able to fill out forms, select text, click links, and scroll without thinking about it.

## What counts

A change counts if it lands at least one of:

- **Interaction parity**: A common interaction pattern now works (e.g., triple-click selects paragraph).
- **Form support**: A form control type now works correctly.
- **Input reliability**: An input method that was flaky now works consistently.
- **Accessibility**: Interaction is keyboard-accessible or screen reader compatible.

## Scope

### Owned by this workstream

- **Link clicking**: Single click navigates, middle-click/ctrl-click opens in new tab
- **Text selection**: Click-drag, double-click word, triple-click paragraph, shift-click extend
- **Copy/paste**: Selection to clipboard, paste into fields
- **Form controls**: `<input>`, `<textarea>`, `<select>`, `<button>`, checkboxes, radios, file inputs
- **Focus management**: Tab order, focus ring, autofocus, focus trapping in dialogs
- **Scrolling**: Mouse wheel, trackpad, keyboard (Page Up/Down, arrow keys, Home/End)
- **Hit testing**: Clicks land on the correct element
- **Hover states**: Cursor changes (including `cursor: none` → `CursorKind::Hidden` to hide the OS cursor), tooltips, hover styling (requires `:hover` in CSS)
- **Drag and drop**: Native drag/drop for files, text, links
- **Context menus**: Right-click menus for links, images, selections
- **Touch input**: (Future) Touch scrolling, tap, long-press

### NOT owned (see other workstreams)

- JavaScript event handlers → `js_dom.md`
- Form submission behavior → `js_html_integration.md` (for JS) / this workstream (for non-JS)
- Visual styling of forms → `capability_buildout.md` (CSS)
- Chrome interactions (address bar, tabs) → `browser_chrome.md`

## Priority order (P0 → P1 → P2)

### P0: Core interactions (browsing works)

1. **Link clicking**
   - Single click navigates to href
   - Middle-click opens in new tab
   - Ctrl/Cmd-click opens in new tab
   - Shift-click opens in new window (optional)
   - Right-click shows context menu with "Open in new tab", "Copy link"

2. **Scrolling**
   - Mouse wheel scrolls smoothly
   - Trackpad two-finger scroll works
   - Keyboard: Space, Shift+Space, Page Up/Down, Home/End, Arrow keys
   - Scroll position persists across history navigation

3. **Text input (basic)**
   - Click to focus `<input type="text">` and `<textarea>`
   - Type to insert text
   - Backspace/Delete to remove
   - Arrow keys to move cursor
   - Home/End to jump to start/end of line
   - Enter submits forms (for single-line inputs)

4. **Focus management (basic)**
   - Tab moves focus forward
   - Shift+Tab moves focus backward
   - Visual focus ring on focused element
   - Autofocus works on page load

### P1: Common interactions (forms work)

5. **Text selection**
   - Click-drag to select
   - Double-click to select word
   - Triple-click to select paragraph/line
   - Shift+click to extend selection
   - Ctrl/Cmd+A to select all (in focused field)

6. **Clipboard**
   - Ctrl/Cmd+C to copy selection
   - Ctrl/Cmd+X to cut selection (in editable)
   - Ctrl/Cmd+V to paste
   - Right-click → Copy/Paste in context menu

7. **Form controls (full)**
   - `<input type="text/email/password/search/url/tel">`
   - `<textarea>` with multiline editing
   - `<select>` with dropdown (current implementation partial)
   - `<input type="checkbox/radio">` toggle on click
   - `<button>` click activation
   - `<input type="submit">` / `<button type="submit">` form submission

8. **Text editing**
   - Ctrl/Cmd+Arrow to jump words
   - Ctrl/Cmd+Backspace to delete word
   - Shift+Arrow to select
   - Undo/Redo (Ctrl/Cmd+Z, Ctrl/Cmd+Shift+Z)

### P2: Advanced interactions

9. **Context menus**
   - Link context menu (open in new tab, copy link, etc.)
   - Image context menu (save image, copy image, etc.)
   - Selection context menu (copy, search for, etc.)
   - Editable context menu (cut, copy, paste, select all)

10. **Drag and drop**
    - Drag text selection to another field
    - Drag links to address bar
    - Drop files on `<input type="file">`
    - Drop images/files where applicable

11. **Additional form controls**
    - `<input type="date/time/datetime-local/month/week">`
    - `<input type="number">` with increment buttons
    - `<input type="range">` slider
    - `<input type="color">` picker
    - `<input type="file">` with file browser
    - `<datalist>` autocomplete

12. **Advanced selection**
    - Multi-range selection (Ctrl+click)
    - Table cell selection
    - Selection across elements

13. **Touch input** (future platform expansion)
    - Tap = click
    - Long-press = right-click
    - Two-finger scroll
    - Pinch to zoom

## Implementation notes

### Architecture

```
src/interaction/         — Interaction engine
  engine.rs              — Main interaction dispatcher
  hit_test.rs            — Hit testing against fragments
  hit_testing.rs         — Additional hit test utilities
  state.rs               — Focus, selection, hover state
  scroll_wheel.rs        — Scroll event handling
  form_submit.rs         — Form submission logic

src/ui/                  — UI integration
  messages.rs            — PointerDown/Up/Move, KeyAction, TextInput events
  render_worker.rs       — Event routing to interaction engine
```

### Key types

```rust
// From src/interaction/state.rs
pub struct InteractionState {
    pub focused_node: Option<NodeId>,
    pub hover_node: Option<NodeId>,
    pub selection: Option<Selection>,
    // ...
}

// From src/ui/messages.rs
pub enum UiToWorker {
    PointerDown { tab_id, pos_css, button, modifiers },
    PointerUp { tab_id, pos_css, button, modifiers },
    PointerMove { tab_id, pos_css },
    KeyAction { tab_id, key, state, modifiers },
    TextInput { tab_id, text },
    // ...
}
```

### Coordinate systems

- **Window coordinates**: Raw from winit, may include DPI scaling
- **Viewport CSS coordinates**: CSS pixels relative to viewport origin
- **Page coordinates**: CSS pixels relative to page origin (viewport + scroll offset)

The UI sends **viewport CSS coordinates** in messages. The worker adds scroll offset for hit testing.

### Testing

- Unit tests are colocated with the interaction engine in `src/interaction/**` (e.g. `src/interaction/hit_test.rs`, `src/interaction/engine.rs`)
- End-to-end (public API) tests live under `tests/` (see `tests/integration.rs`); browser/UI scenarios live in `tests/browser_integration/` and are included by that harness
- Manual testing for platform-specific input behavior

## Current limitations

From `docs/browser_ui.md`:
- Text input is "intentionally minimal"
- `<select>` support is "basic" (no typeahead, no multi-select)
- Many controls not yet supported (contenteditable, file inputs)

## Success criteria

Page interaction is **done** when:
- Users can fill out and submit any standard HTML form
- Text selection works as expected (click-drag, double/triple-click)
- Copy/paste works reliably
- All form controls have basic functionality
- Focus management follows expected tab order
- Scrolling is smooth and responsive on all platforms
