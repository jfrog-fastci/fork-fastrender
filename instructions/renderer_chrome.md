# Workstream: Renderer Chrome (dogfood: UI rendered by FastRender)

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

---

## The vision

**Render the browser UI with FastRender itself.**

Instead of egui, the tabs/address bar/menus are HTML/CSS rendered by our own engine.

This is the ultimate dogfooding:
- Forces the renderer to be fast (UI must be responsive)
- Forces the renderer to be correct (we use it daily)
- Simplifies the codebase (one rendering path)
- Makes theming trivial (just CSS)
- Proves FastRender is production-ready

## Current state

- Browser UI uses **egui** (immediate mode Rust GUI)
- egui renders tabs, address bar, menus
- FastRender renders page content only
- Two completely separate rendering paths

## Target state

```
┌─────────────────────────────────────────────────────────────┐
│                     Browser Window                          │
│  ┌───────────────────────────────────────────────────────┐ │
│  │  Chrome Frame (HTML/CSS, rendered by FastRender)      │ │
│  │  ┌─────────────────────────────────────────────────┐  │ │
│  │  │ Tab bar, address bar, toolbar                   │  │ │
│  │  └─────────────────────────────────────────────────┘  │ │
│  │  ┌─────────────────────────────────────────────────┐  │ │
│  │  │  Content Frame (web page)                       │  │ │
│  │  │  (also rendered by FastRender)                  │  │ │
│  │  └─────────────────────────────────────────────────┘  │ │
│  └───────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

Both chrome and content are HTML/CSS rendered by FastRender.

## What counts

A change counts if it lands at least one of:

- **Chrome component migrated**: A UI element moves from egui to HTML/CSS.
- **Chrome CSS created**: Styling for chrome components.
- **Chrome JS logic**: Interaction handling for chrome.
- **Performance parity**: Migrated chrome is as fast as egui version.

## Why this makes sense

### Forcing functions

| Problem | How renderer-chrome forces a fix |
|---------|----------------------------------|
| Renderer is slow | Chrome is slow → must fix |
| JS doesn't work | Chrome needs JS → must fix |
| CSS bugs | Chrome looks wrong → must fix |
| Layout bugs | Chrome is broken → must fix |

### Benefits

1. **Single rendering codepath** - Less code to maintain
2. **Themeable** - Chrome styling via CSS
3. **Extensible** - Extensions could modify chrome HTML
4. **Consistent** - Chrome and content look unified
5. **Proof of quality** - If FastRender can render its own UI, it's ready

## Security model

**Critical**: Chrome renders in the **browser process**, not renderer process.

```
Browser Process (TRUSTED)
├── Chrome Frame (HTML/CSS)
│   └── Rendered by FastRender (trusted instance)
│
└── IPC to Renderer Processes (SANDBOXED)
    └── Content Frame (HTML/CSS)
        └── Rendered by FastRender (sandboxed instance)
```

The chrome frame:
- Never renders untrusted content
- Has access to browser state
- Is NOT sandboxed
- Its HTML/CSS is OUR code (built into the binary or loaded from trusted location)

## Priority order

### P0: Prove feasibility

Before migrating anything, prove it works.

1. **Internal page rendering**
   - Render `about:settings` with FastRender
   - Render `about:history` with FastRender
   - These are low-risk (not critical chrome)

2. **Performance validation**
   - Chrome frame renders at 60fps
   - No perceptible lag vs egui
   - Memory usage is reasonable
   - Tip: set `FASTR_LOG_INTERACTION_INVALIDATION=1` to print one line per chrome render describing whether it was paint-only vs restyle/relayout (useful when dogfooding hover/focus performance).

### P1: Simple chrome components

Migrate simple, non-critical UI pieces.

1. **Status bar** - Simple text display
2. **Context menus** - Popup HTML/CSS
3. **Tooltips** - Floating HTML/CSS
4. **Dialogs** - Alert, confirm, prompt

### P2: Core chrome

Migrate the main chrome components.

1. **Tab bar**
   - Tab rendering
   - Tab interactions (click, close, drag)
   - Tab overflow

2. **Address bar**
   - URL display
   - Text input
   - Autocomplete dropdown

3. **Toolbar**
   - Navigation buttons
   - Menu buttons

### P3: Full migration

Complete the transition.

1. **Remove egui dependency** (for chrome, keep for debugging tools if useful)
2. **Chrome CSS polish**
3. **Platform-specific styling**
4. **Accessibility for chrome**
   - See: [`docs/chrome_accessibility.md`](../docs/chrome_accessibility.md)

## Technical approach

### Chrome HTML structure

```html
<!DOCTYPE html>
<html class="chrome-frame">
<head>
  <link rel="stylesheet" href="chrome://styles/chrome.css">
</head>
<body>
  <div class="tab-strip">
    <div class="tab active" data-tab-id="1">
      <span class="tab-title">Google</span>
      <button class="tab-close">×</button>
    </div>
    <!-- more tabs -->
  </div>
  
  <div class="toolbar">
    <button class="nav-back">←</button>
    <button class="nav-forward">→</button>
    <button class="nav-reload">↻</button>
    <input class="address-bar" type="text" value="https://google.com">
  </div>
  
  <div class="content-frame">
    <!-- Content iframe/embed goes here -->
  </div>
</body>
</html>
```

Note: `chrome://` (assets) and `chrome-action:` (browser actions) are privileged internal schemes
reserved for the trusted browser-process chrome renderer. See
[`docs/renderer_chrome_schemes.md`](../docs/renderer_chrome_schemes.md).

For a JS-free bootstrap approach (trusted HTML/CSS + `chrome-action:` navigations/forms), see
[`docs/renderer_chrome_non_js.md`](../docs/renderer_chrome_non_js.md).

### Chrome JS

For the canonical JS API surface and trust boundary, see
[`docs/chrome_js_bridge.md`](../docs/chrome_js_bridge.md).

```javascript
// chrome://scripts/chrome.js
document.querySelector('.nav-back').onclick = () => {
  chrome.navigation.goBack();
};

document.querySelector('.address-bar').onkeydown = (e) => {
  if (e.key === 'Enter') {
    chrome.navigation.navigate(e.target.value);
  }
};
```

### Rust ↔ Chrome JS bridge

```rust
// Browser process exposes APIs to chrome JS
impl ChromeRuntime {
    fn navigate(&self, url: &str) { ... }
    fn go_back(&self) { ... }
    fn go_forward(&self) { ... }
    fn new_tab(&self) { ... }
    fn close_tab(&self, id: TabId) { ... }
}
```

## Dependencies

This workstream depends on:

- **live_rendering.md**: Chrome needs dynamic rendering (hover states, focus, etc.)
- **multiprocess_security.md**: Chrome must be in browser process, content in renderer process
- **browser_responsiveness.md**: Chrome must be fast

## Testing

### Functional tests

- All chrome interactions work
- Keyboard shortcuts work
- Accessibility features work

### Performance tests

- Chrome frame renders at 60fps
- Input latency <50ms
- Memory usage reasonable

### Comparison tests

- Side-by-side with egui version
- No regressions in functionality

## Success criteria

Renderer chrome is **done** when:
- Browser chrome is 100% HTML/CSS rendered by FastRender
- egui is removed from chrome (keep for dev tools if useful)
- Performance matches or exceeds egui version
- All chrome functionality works
- Theming works via CSS

This is a long-term goal, not immediate priority. Get live_rendering and multiprocess_security first.
