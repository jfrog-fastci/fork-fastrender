# Browser chrome manual test matrix (Linux / macOS / Windows)

This document is a **manual regression checklist** for the windowed `browser` UI (feature
`browser_ui`) and its **egui-rendered chrome** (tabs, toolbar, address bar, menus, panels).

Goal: contributors can validate chrome UX changes on **Linux/macOS/Windows** without guessing the
expected behaviour.

If you only need a quick shortcut/UX parity smoke pass, see the shorter
[chrome_test_matrix.md](chrome_test_matrix.md).

Scope note: today the rendered page is a pixmap; **page content accessibility and DOM interaction
parity are out of scope here** (see `instructions/browser_interaction.md`). This matrix is for the
**browser shell**.

---

## Test setup (recommended)

1. **Run the windowed browser UI**:

   ```bash
   bash scripts/run_limited.sh --as 64G -- \
     bash scripts/cargo_agent.sh run --features browser_ui --bin browser
   ```

2. **Use an isolated session** so you don’t pollute your real profile while testing:

   ```bash
   # Preferred: CLI flag
   bash scripts/run_limited.sh --as 64G -- \
     bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- \
     --session-path ./target/manual_session.json
   #
   # Env var equivalent:
   FASTR_BROWSER_SESSION_PATH=./target/manual_session.json \
     bash scripts/run_limited.sh --as 64G -- \
     bash scripts/cargo_agent.sh run --features browser_ui --bin browser
   ```

3. Prefer deterministic built-in pages when possible:
   - `about:test-scroll` (contains the literal text `scroll`)
   - `about:test-form` (simple `<input>`/`<button>`)
   - `about:test-heavy` (large DOM; useful for stop-loading / cancellation)

---

## Shortcut notation + platform mapping

In this repo, docs often use “Ctrl/Cmd” as the *primary* browser modifier:

| Meaning in this doc | Windows | Linux | macOS |
|---|---|---|---|
| **Primary shortcut modifier** | Ctrl | Ctrl | Cmd |
| **Alt/Option** | Alt | Alt | Option |

### Known winit/egui shortcut caveats (important)

- **macOS `Cmd+Tab` is reserved by the OS** (app switcher) and generally won’t reach the app.
  - Tab cycling in FastRender is expected to work via **Ctrl+Tab / Ctrl+Shift+Tab** on macOS.
- **AltGr layouts (common on Windows/Linux)** can be reported as **Ctrl+Alt**.
  - FastRender’s shortcut mapping intentionally ignores **Ctrl+Alt+…** combinations to avoid
    breaking text entry on those layouts (see `src/ui/shortcuts.rs`).
- **Non-US keyboard layouts** may produce different physical keys for `[` / `]`. On macOS, the
  canonical back/forward shortcuts are `Cmd+[ / Cmd+]`; verify they work on your layout.
- winit may deliver text input (`ReceivedCharacter`) very close to focus changes; always test
  “press shortcut then type immediately” for address bar / find bar.

---

## 1) Address bar / omnibox (focus + typing)

Use `about:test-form` so you can easily switch focus between page inputs and chrome.

### Focus acquisition

- [ ] **Mouse click**:
  - [ ] Clicking the address bar focuses it.
  - [ ] The full contents are selected (typing replaces the whole value).
- [ ] **Ctrl/Cmd+L**:
  - [ ] Focuses address bar and selects all.
  - [ ] Works from: page focus, tab strip, downloads panel, find bar, tab search overlay.
- [ ] **Ctrl/Cmd+K**:
  - [ ] Same behaviour as Ctrl/Cmd+L (focus + select all).

### Typing behaviour

- [ ] After Ctrl/Cmd+L (or Ctrl/Cmd+K), **type immediately** (no pause):
  - [ ] The first character is not dropped.
  - [ ] The character is not forwarded into the page.
- [ ] Press **Enter**:
  - [ ] Navigates to the resolved URL (or performs a search when input is not a URL).
  - [ ] After navigation commits, the address bar displays the normalized URL for the active tab.
- [ ] **Alt/Option+Enter** (when address bar has text):
  - Win/Linux: Alt+Enter
  - macOS: Option+Enter
  - [ ] Opens the resolved navigation in a **new foreground tab**.
  - [ ] The original tab remains unchanged.

### Escape behaviour (omnibox dropdown + focus)

- [ ] With the omnibox dropdown open (type `exam` to trigger suggestions):
  - [ ] ArrowDown previews a suggestion (address bar text changes to the suggestion URL).
  - [ ] **First Esc** closes the dropdown and restores your original typed input (focus stays in
    the address bar).
  - [ ] **Second Esc** blurs the address bar and restores the active tab URL.
- [ ] With the dropdown closed but address bar focused:
  - [ ] Esc blurs and restores the active tab URL.

### Clipboard (copy / paste)

- [ ] Copy/paste works in the address bar and uses the **OS clipboard**:
  - [ ] Ctrl/Cmd+C copies the selection.
  - [ ] Ctrl/Cmd+V pastes.
  - [ ] Ctrl/Cmd+X cuts.
  - [ ] Ctrl/Cmd+A selects all.
- [ ] After pasting a URL into the address bar, pressing Enter navigates correctly.

---

## 2) Navigation controls (back / forward / reload / stop / home)

Use `about:test-scroll` → `about:test-form` to build deterministic back/forward history.

### Toolbar buttons

- [ ] **Back**:
  - [ ] Disabled when there is no history.
  - [ ] Enabled after navigating at least once.
  - [ ] Clicking navigates to the previous entry and updates:
    - [ ] active tab title (when available)
    - [ ] address bar text
    - [ ] back/forward enabled state
- [ ] **Forward**:
  - [ ] Disabled when there is no forward history.
  - [ ] Enabled after using Back.
  - [ ] Clicking navigates forward and updates chrome state as above.
- [ ] **Reload / Stop**:
  - [ ] When the tab is idle: reload icon is visible and triggers a reload.
  - [ ] While loading: stop icon is visible and cancels loading.
- [ ] **Home**:
  - [ ] Navigates to the configured home page URL (default is typically `about:newtab`).

### Keyboard shortcuts (platform variants)

- [ ] Back:
  - Win/Linux: Alt+Left
  - macOS: Cmd+[ (Ctrl+[ is also supported but not the canonical mac shortcut)
- [ ] Forward:
  - Win/Linux: Alt+Right
  - macOS: Cmd+]
- [ ] Reload:
  - Win/Linux/macOS: Ctrl/Cmd+R
  - Win/Linux/macOS: F5
- [ ] Stop loading:
  - Win/Linux/macOS: Esc (while loading)
- [ ] Home:
  - Win/Linux: Alt+Home
  - macOS: Cmd+Shift+H

### Mouse buttons (optional)

- [ ] Mouse back/forward buttons navigate back/forward.
  - Note: button numbering differs by platform (documented in `docs/browser_ui.md`).

---

## 3) Tabs (new / close / switch / reorder / detach)

### New tab

- [ ] New tab via:
  - [ ] Toolbar “+” button
  - [ ] Ctrl/Cmd+T
  - [ ] Menu: File → New Tab
- [ ] Expected results:
  - [ ] New tab becomes the active tab.
  - [ ] The new tab loads `about:newtab` (unless your build/config changes the default).
  - [ ] **Address bar is focused and selected** so you can type immediately.

### Close tab

- [ ] Close via:
  - [ ] Click tab close “×”
  - [ ] Ctrl/Cmd+W
  - [ ] Middle-click tab
  - [ ] Menu: File → Close Tab
- [ ] Expected results:
  - [ ] Closing the active tab activates a reasonable neighbour tab.
  - [ ] Closing the **last remaining tab** is a no-op (tab stays open).
  - [ ] Address bar updates to the newly active tab’s URL.

### Switch tabs

- [ ] Switch via:
  - [ ] Click a tab
  - [ ] Ctrl+Tab / Ctrl+Shift+Tab (all platforms; see macOS caveat above)
  - [ ] Ctrl/Cmd+1..9 (9 = last tab)
  - [ ] Ctrl+PageUp / Ctrl+PageDown
- [ ] Expected results:
  - [ ] Switching tabs is immediate (chrome updates even if page rendering is still catching up).
  - [ ] Address bar text matches the active tab.
  - [ ] Any in-progress address bar editing is cancelled (you should not “carry” a partially typed
    URL to a different tab).

### Reorder tabs (drag)

- [ ] Drag a tab left/right in the tab strip:
  - [ ] The tab moves and the order updates.
  - [ ] The active tab stays active (even while the strip reorders).
  - [ ] After restarting the browser (session restore), the reordered tab order persists.

### Detach tab (drag out)

- [ ] Drag a tab out of the tab strip:
  - [ ] A **new window** opens containing the detached tab.
  - [ ] The tab is removed from the original window.
  - [ ] Both windows remain usable (input focus, address bar, navigation).

---

## 4) Session restore + crash recovery

### Normal session restore (clean exit)

- [ ] With an isolated session (`--session-path ...` or `FASTR_BROWSER_SESSION_PATH=...`), create a meaningful state:
  - [ ] Open 3+ tabs and switch the active tab.
  - [ ] Reorder tabs.
  - [ ] Change per-tab zoom (Ctrl/Cmd +/-).
- [ ] Quit the browser normally (close the window, or File → Quit).
- [ ] Relaunch the browser **without** an explicit URL.
- [ ] Expected results:
  - [ ] Previous window/tabs restore.
  - [ ] Active tab index restores.
  - [ ] Per-tab zoom restores.
  - [ ] Best-effort scroll restoration occurs where supported.

### Crash recovery (unclean exit marker)

- [ ] Start the browser, open a few tabs.
- [ ] Force an unclean exit:
  - Linux/macOS: `kill -9 <pid>` (or “Force Quit” on macOS)
  - Windows: Task Manager → End task
- [ ] Relaunch the browser without a URL.
- [ ] Expected results:
  - [ ] The browser restores the previous session.
  - [ ] A crash-recovery infobar/toast appears indicating the previous session was restored.
    - [ ] It includes **Keep** (dismiss) and **Start new session** (discard restored tabs) actions.
    - [ ] Clicking **Keep** dismisses the infobar and preserves the restored tabs.
    - [ ] Clicking **Start new session** closes/discards the restored tabs and leaves a fresh
      `about:newtab` session (per window).
  - [ ] A warning is also printed to stderr indicating the previous session ended unexpectedly.

### Renderer crash isolation (optional, dev/testing hook)

This is separate from the session crash marker: it intentionally crashes the **render worker** while
the browser process stays alive.

- [ ] Run the browser with crash URLs enabled:
  - `browser --allow-crash-urls` (or `FASTR_BROWSER_ALLOW_CRASH_URLS=1`)
- [ ] Navigate (typed URL) to `crash://panic`.
- [ ] Expected results:
  - [ ] The worker crashes, but the browser UI remains alive.
  - [ ] The affected tab shows an error/unresponsive state.
  - [ ] You can open a new tab and continue browsing.

---

## 5) Downloads panel (open / cancel / retry / open / reveal)

### Offline download fixture (recommended)

Create a deterministic local page + payload (cross-platform Python):

```bash
python - <<'PY'
from pathlib import Path
p = Path("target/manual_download_fixture")
p.mkdir(parents=True, exist_ok=True)
(p / "payload.bin").write_bytes(b"\xAB" * (3 * 1024 * 1024))
(p / "page.html").write_text(
  """<!doctype html>
<html><head><meta charset="utf-8"><title>DL</title></head>
<body><a download="payload.bin" href="payload.bin">download</a></body></html>
""",
  encoding="utf-8",
)
print("Open this URL in the address bar:")
print((p / "page.html").resolve().as_uri())
PY
```

### Panel open/close

- [ ] Open downloads panel via:
  - [ ] Toolbar downloads icon
  - [ ] Shortcut: Ctrl+J (Win/Linux) / Cmd+Shift+J (macOS)
  - [ ] Menu: Window → Show Downloads…
- [ ] Close via:
  - [ ] Panel close button
  - [ ] Re-invoking the shortcut toggles closed

### Download lifecycle

- [ ] Start a download (click the link in the fixture page):
  - [ ] A new entry appears in the downloads panel.
  - [ ] Progress updates over time (bytes received / progress bar).
- [ ] Cancel:
  - [ ] Clicking “Cancel” stops the download.
  - [ ] The partially downloaded `*.part` file is removed (no leftovers in the download directory).
- [ ] Retry:
  - [ ] “Retry” restarts the download and uses a sane filename (with collision suffix if needed).
- [ ] Open / Reveal:
  - [ ] “Open” opens the downloaded file with the OS default handler.
  - [ ] “Reveal” opens the OS file manager at the download location:
    - Windows: Explorer
    - macOS: Finder
    - Linux: default file manager via `xdg-open`

Platform caveat: in some sandboxed/headless environments, “Open/Reveal” may fail (no handler
available). This should surface a user-visible error and must not crash the browser.

### Downloads folder / directory configuration

- [ ] “Show downloads folder” opens the current download directory in the OS file manager.
- [ ] Overriding the downloads directory works:
  - [ ] Run with `browser --download-dir <path>` (or `FASTR_BROWSER_DOWNLOAD_DIR=<path>`).
  - [ ] Start a new download and verify the file lands in the configured directory.

### Clear completed downloads (if implemented)

- [ ] If the UI exposes a “Clear” / “Clear completed” action:
  - [ ] Completed entries are removed from the list.
  - [ ] In-progress downloads remain visible (not cleared).

---

## 6) Find in page (open / close / next / prev / case sensitivity)

Use `about:test-scroll` and search for the text `scroll`.

### Open / close

- [ ] Open via Ctrl/Cmd+F (or Edit → Find in Page).
  - [ ] Find bar becomes visible.
  - [ ] The find input is focused (typing goes into it immediately).
- [ ] Close via:
  - [ ] Esc (when find bar is focused)
  - [ ] Find bar close button
  - [ ] Re-invoking Ctrl/Cmd+F toggles closed (if supported by current UI behaviour)
- [ ] Expected results:
  - [ ] Closing clears highlights and resets match counts.
  - [ ] Focus returns to a sensible place (typically the address bar or the page).

### Next / previous + match count

- [ ] Enter a query with multiple matches (`scroll` on `about:test-scroll`):
  - [ ] Match count updates (e.g. `1/1`, `1/3`, etc).
  - [ ] Active match is highlighted in the page view.
- [ ] Next / previous:
  - [ ] Next button (and Enter) advances to the next match.
  - [ ] Previous button goes to the previous match.
  - [ ] Wrap-around behaviour is sane (end wraps to start).

### Case sensitivity

- [ ] Toggle “case sensitive”:
  - [ ] Match count updates immediately.
  - [ ] Active match/highlight updates accordingly.

---

## 7) Menus + shortcuts (File / Edit / View / History / Bookmarks / Window / Help)

The browser supports an optional **in-window menu bar**:

- Default:
  - **macOS:** hidden (but can be shown)
  - **Windows/Linux:** shown
- Toggle: toolbar hamburger menu → Window → **Show menu bar**

For parity testing, it’s recommended to enable the menu bar on all platforms.

### Menu presence + basic interaction

- [ ] Menu bar contains: File, Edit, View, History, Bookmarks, Window, Help.
- [ ] Each menu opens via click and can be dismissed by clicking away or Esc.
- [ ] Menu items show correct enabled/disabled state (placeholders are visibly disabled).

### File

- [ ] New Tab (Ctrl/Cmd+T) works.
- [ ] Close Tab (Ctrl/Cmd+W) works.
- [ ] Save Page… is present but disabled (or shows a clear “not implemented” UX).
- [ ] Print… is present but disabled (or shows a clear “not implemented” UX).
- [ ] Quit exits cleanly (session writes `did_exit_cleanly=true`).

### Edit

- [ ] Cut/Copy/Paste/Select All work for chrome text inputs (address bar, find bar).
- [ ] Find in Page opens (Ctrl/Cmd+F).

### View

- [ ] Reload works (Ctrl/Cmd+R).
- [ ] Zoom in/out/reset work (Ctrl/Cmd +/-/0).
- [ ] Toggle Full Screen works:
  - Win/Linux: F11
  - macOS: Ctrl+Cmd+F

### History

- [ ] Back/Forward work.
- [ ] Reopen Closed Tab works (Ctrl/Cmd+Shift+T).
- [ ] History panel toggle works:
  - Win/Linux: Ctrl+H
  - macOS: Cmd+Y (and optionally Cmd+Shift+H, Firefox-style)
- [ ] History panel behaviour (when open):
  - [ ] Shows recent visits after navigating across a few non-`about:` pages.
  - [ ] Search filters results (and can be cleared).
  - [ ] “Clear browsing data…” opens the clear-data dialog.
  - [ ] Clicking an entry navigates (and focus returns to a sensible surface).

### Bookmarks

- [ ] Bookmark This Page toggle works:
  - Win/Linux: Ctrl+D
  - macOS: Cmd+D
- [ ] Bookmarks bar toggle works (Ctrl/Cmd+Shift+B).
- [ ] Bookmarks manager opens (Ctrl/Cmd+Shift+O).
- [ ] Bookmarks panel/manager behaviour (when open):
  - [ ] Search filters bookmarks.
  - [ ] Clicking a bookmark navigates (and can be opened in a new tab when offered).
  - [ ] Removing a bookmark updates the bookmarks bar / new tab page snapshot.

### Window

- [ ] New Window opens (Ctrl/Cmd+N).
- [ ] Show Downloads… opens downloads panel (Ctrl+J / Cmd+Shift+J).
- [ ] “Show menu bar” toggles the in-window menu bar (and persists across restart).

### Help

- [ ] Help opens `about:help` in a new tab.
- [ ] About opens `about:version` in a new tab.

---

## 8) Accessibility smoke (VoiceOver / Narrator / Orca)

FastRender exposes **chrome accessibility** via egui’s AccessKit integration.
The rendered page is currently an image, so screen readers should only be expected to traverse:
tabs, toolbar buttons, address bar, menus, panels, and popups.

See `docs/chrome_accessibility.md` for deeper debugging and the `dump_accesskit` tool.

### Basic cross-platform expectations

- [ ] Screen reader can traverse the toolbar and announce labels for:
  - [ ] Back / Forward / Reload / Stop / Home
  - [ ] Address bar (announced as an editable text field)
  - [ ] Tab strip entries + close buttons
  - [ ] Downloads button / panel
  - [ ] Find bar (when open)
- [ ] Focus changes are announced (e.g. Ctrl/Cmd+L announces “Address bar”).
- [ ] Disabled controls are exposed as disabled (e.g. Back when no history).

### macOS: VoiceOver (smoke)

- [ ] Enable VoiceOver (commonly Cmd+F5; may vary by system settings).
- [ ] Use VoiceOver navigation (Ctrl+Option+Arrow keys) to move through chrome controls.
- [ ] Verify labels/roles are spoken for address bar + toolbar buttons.

### Windows: Narrator (smoke)

- [ ] Enable Narrator (Win+Ctrl+Enter).
- [ ] Tab through chrome controls and ensure Narrator announces labels/states.

### Linux: Orca (smoke)

- [ ] Enable Orca (often Super+Alt+S on GNOME; may vary).
- [ ] Tab/arrow through chrome controls and ensure Orca announces labels/states.
