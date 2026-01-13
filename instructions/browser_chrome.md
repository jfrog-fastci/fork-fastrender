# Workstream: Browser Chrome (shell, navigation, tabs)

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

This workstream owns the **browser application shell**: the chrome surrounding the rendered page content.

Note: this workstream currently targets the **egui**-rendered chrome used by the in-tree `browser`
binary. The longer-term *renderer chrome* workstream aims to render the chrome UI using FastRender
itself; trusted chrome pages in that world would use the privileged JS bridge documented in
[`docs/chrome_js_bridge.md`](../docs/chrome_js_bridge.md).
That renderer-chrome workstream also reserves privileged internal URL schemes (`chrome://` assets and
`chrome-action:` actions); see [`docs/renderer_chrome_schemes.md`](../docs/renderer_chrome_schemes.md).

## The job

Build a **production-quality browser shell** that users would actually use daily. Not a POC. Not ugly. Not flaky. A real browser.

## What counts

A change counts if it lands at least one of:

- **Feature complete**: A missing chrome feature is implemented (with regression test).
- **Bug fix**: A broken feature now works reliably (with regression test).
- **Keyboard/mouse parity**: Shortcuts and interactions match user expectations from Chrome/Firefox/Safari.
- **Platform parity**: Feature works correctly on Linux, macOS, and Windows.
- **Accessibility parity**: Chrome UI is usable with screen readers (labels, focus order, state) — see
  [`docs/chrome_accessibility.md`](../docs/chrome_accessibility.md).

## Scope

### Owned by this workstream

- **Address bar**: URL entry, autocompletion, URL normalization, domain display, HTTPS indicators
- **Navigation controls**: Back, forward, reload, stop, home
- **Tab management**: New tab, close tab, switch tabs, reopen closed, tab overflow, drag reorder
- **Keyboard shortcuts**: All standard browser shortcuts (see `docs/browser_ui.md` for current list)
- **Window management**: Multiple windows, window state persistence, minimize/maximize behavior
- **Session management**: Tab restoration, session persistence, crash recovery
- **Browser menus**: File/Edit/View/History/Bookmarks/Window/Help menus
- **Status bar**: Loading progress, link hover preview, zoom level display
- **Find in page**: Ctrl+F find bar with match highlighting
- **Downloads**: Download manager, download progress, download location
- **Chrome accessibility**: Screen-reader support for chrome UI (AccessKit labels/roles/focus/state)

### NOT owned (see other workstreams)

- Page rendering quality → `capability_buildout.md`
- Page interaction (forms, focus, selection) → `browser_interaction.md`
- Performance (frame rate, responsiveness) → `browser_responsiveness.md`
- JavaScript execution → `js_*.md` workstreams

## Priority order (P0 → P1 → P2)

### P0: Core reliability (users can browse)

These MUST work flawlessly before anything else:

1. **Address bar reliability**
   - URL entry works 100% of the time (current bug: "often doesn't work")
   - Enter key always navigates
   - Escape clears/cancels correctly
   - Focus behavior is predictable (Cmd+L, clicking, tabbing)
   - URL display shows current page URL accurately
   - Copy/paste works in address bar

2. **Navigation reliability**
   - Back/forward always work when history exists
   - Reload always reloads current page
   - Stop actually stops loading
   - Loading state is always accurate

3. **Tab reliability**
   - New tab always creates a tab
   - Close tab always closes the tab
   - Tab switching is instant
   - Tab state (scroll position, form data) is preserved

### P1: Feature completeness (users can be productive)

4. **Find in page** (Cmd+F)
   - Search bar appears/disappears reliably
   - Match highlighting in page
   - Next/previous match navigation
   - Match count display
   - Case sensitivity toggle

5. **Session management**
   - Tabs persist across browser restart
   - Scroll position restored
   - Form data optionally preserved
   - "Restore previous session" on crash

6. **Multi-window**
   - Cmd+N opens new window
   - Windows have independent tab sets
   - Window positions/sizes persist

### P2: Power user features

7. **Tab management**
   - Drag tabs to reorder
   - Drag tab out to create new window
   - Pin tabs
   - Tab groups
   - Tab search/switching (Cmd+Shift+A style)

8. **Bookmarks**
   - Bookmark current page
   - Bookmarks bar
   - Bookmark manager

9. **History**
   - History panel
   - Clear browsing data
   - History search

10. **Downloads**
    - Download manager panel
    - Download progress in toolbar
    - Open/reveal downloaded files

 ## Implementation notes
 
 ### Architecture (current)

```
src/bin/browser.rs      — winit/egui/wgpu integration, event loop
src/ui/browser_app.rs   — BrowserAppState, tab model
src/ui/chrome.rs        — Chrome widget helpers
src/ui/messages.rs      — UI ↔ Worker protocol
src/ui/render_worker.rs — Background rendering thread
src/ui/history.rs       — Per-tab navigation history
src/ui/session.rs       — Session persistence
```

### Key invariants

- **UI thread never blocks**: All rendering happens on worker thread
- **Worker owns history**: UI sends `GoBack`/`GoForward`, doesn't compute URLs
- **Cancellation is cooperative**: Use `CancelGens` for stale work

### Testing
  
- Unit tests for URL normalization, history logic, session serialization
- Integration tests in `tests/browser_integration/`
- Manual cross-platform smoke matrix (shortcuts + UX parity):
  [`docs/chrome_test_matrix.md`](../docs/chrome_test_matrix.md)
- Manual cross-platform regression checklist (deeper, end-to-end):
  [`docs/browser_chrome_manual_test_matrix.md`](../docs/browser_chrome_manual_test_matrix.md)
  
## Current bugs (fix these first)

Based on user reports:
- [ ] Address bar "often doesn't work" — needs investigation and fix
- [ ] (Add bugs here as discovered)

## Success criteria

The browser chrome is **done** when:
- A non-technical user can browse the web without confusion
- All standard keyboard shortcuts work as expected
- Session restore works reliably
- No known P0 bugs remain open
