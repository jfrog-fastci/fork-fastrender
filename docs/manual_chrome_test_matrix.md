# Manual chrome test matrix (Linux / macOS / Windows)

FastRender’s desktop `browser` UI aims for keyboard/mouse parity across platforms, but many
chrome-level behaviours are hard to cover with automated tests (native windowing, fullscreen, OS
file manager integration, focus quirks).

This checklist is a **manual smoke matrix** for contributors working on browser chrome features
(tabs, omnibox, panels). Run it on **each target OS** before/after chrome changes and note any
platform-specific deltas.

Key notation:

- **Win/Linux** uses <kbd>Ctrl</kbd> and <kbd>Alt</kbd>.
- **macOS** uses <kbd>Cmd</kbd> and <kbd>Option</kbd>.
- If a row says “Ctrl/Cmd”, use <kbd>Ctrl</kbd> on Win/Linux and <kbd>Cmd</kbd> on macOS.

Recommended test pages:

- `about:test-form` (focus + text input)
- `about:test-scroll` (scroll + viewport)
- Any real site (e.g. `https://example.com/`) for navigation/history

For downloads, a deterministic option is to start a local HTTP server that serves a file:

```bash
mkdir -p /tmp/fastr-download-test && cd /tmp/fastr-download-test
python3 -m http.server 8000
# Put any file here (e.g. test.bin) and download it from http://localhost:8000/test.bin
```

Build/run instructions live in [browser_ui.md](browser_ui.md).

## Matrix

### Address bar (omnibox)

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| Focus address bar (select all) | Ctrl+L / Ctrl+K / F6 / Alt+D | Cmd+L / Cmd+K / F6 | Focus moves to address bar and existing text is selected. Typing replaces selection. |
| Navigate | Enter | Enter | Navigates to URL (or search). Address bar updates to the resolved/normalized URL after commit. |
| Open input in a new tab | Alt+Enter (while editing) | Option+Enter (while editing) | Creates a new foreground tab and navigates it to the resolved URL/query; current tab stays put. |
| Escape closes suggestions | Esc | Esc | If the omnibox dropdown is open (or a suggestion is selected), Esc closes the dropdown and restores the original input. |
| Escape cancels editing | Esc (when dropdown already closed) | Esc (when dropdown already closed) | Exits address bar editing and returns focus to the previous surface (typically the page). |
| Copy / paste in address bar | Ctrl+C / Ctrl+V (also Ctrl+X / Ctrl+A) | Cmd+C / Cmd+V (also Cmd+X / Cmd+A) | Uses the OS clipboard. Pasted URLs work; selection + editing shortcuts behave like a normal text field. |

### Navigation (buttons + shortcuts)

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| Back / Forward buttons | Click toolbar buttons | Click toolbar buttons | Disabled state matches history availability; click navigates correctly. |
| Back / Forward shortcuts | Alt+Left / Alt+Right | Cmd+[ / Cmd+] | Matches buttons; doesn’t require focusing the page first. |
| Reload | Ctrl+R or F5 | Cmd+R | Reloads current URL. |
| Stop loading | Esc (while loading) | Esc (while loading) | Cancels in-flight navigation; loading indicator stops updating. |
| Home page | Alt+Home | Cmd+Shift+H | Navigates to configured home page (if set). |

### Tabs (create/close/switch/reopen)

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| New tab | Ctrl+T | Cmd+T | New tab appears with correct initial URL (typically `about:newtab`). |
| Close tab | Ctrl+W (or middle-click tab) | Cmd+W (or middle-click tab) | Closes active tab. If only one tab exists, close is a no-op (app stays open). |
| Switch tabs (next/prev) | Ctrl+Tab / Ctrl+Shift+Tab (also Ctrl+PageDown/PageUp) | Cmd+Tab / Cmd+Shift+Tab (also Cmd+PageDown/PageUp) | Tab cycling is stable and wraps as expected; active tab highlight updates. |
| Switch tabs by number | Ctrl+1..9 | Cmd+1..9 | 1..8 activates that index; 9 activates the last tab. |
| Reopen closed tab | Ctrl+Shift+T | Cmd+Shift+T | Reopens the most recently closed tab (repeatable). |

### Find in page

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| Open find bar | Ctrl+F | Cmd+F | Find UI becomes visible and focused; typing immediately updates match count. |
| Next / previous match | Enter / Shift+Enter (in find bar) | Enter / Shift+Enter (in find bar) | Advances active match; wraps at ends; match index updates (e.g. `2/10`). |
| Close find bar | Esc (or close button) | Esc (or close button) | Find UI closes and match highlights are cleared. Focus returns to a sensible surface. |

### Downloads panel

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| Toggle downloads panel | Ctrl+J (or toolbar downloads icon) | Cmd+Shift+J (or toolbar downloads icon) | Panel opens/closes; focus doesn’t get “stuck” (Esc/close returns focus to page). |
| Start a download | Use a direct file URL or page context menu **Download link/image** | Same | Download appears in the panel with “Downloading…” status and a progress indicator (when total size is known). |
| Cancel in-progress download | Click **Cancel** | Click **Cancel** | Download transitions to “Cancelled” with a **Retry** affordance. Partial `*.part` files should not replace the final filename. |
| Retry cancelled/failed download | Click **Retry** | Click **Retry** | Restarts the download and updates status correctly. |
| Open completed download | Click **Open** | Click **Open** | Launches the file with the OS default handler (Explorer/Finder associated app). |
| Reveal completed download | Click **Show in Folder** | Click **Show in Folder** | Reveals the file in the OS file manager (Explorer/Finder/file manager). |
| Show downloads folder | Click **Show downloads folder** (panel header) | Same | Opens the configured downloads directory in the OS file manager. |
| Change downloads folder | (N/A in UI; use `browser --download-dir …` / `FASTR_BROWSER_DOWNLOAD_DIR=…`) | Same | Verify downloads land in the configured directory. |
| Clear completed downloads | (Only if implemented) | (Only if implemented) | If/when a “Clear” affordance exists, verify it removes completed entries without affecting in-progress downloads. |

### History + bookmarks panels

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| Toggle history panel | Ctrl+H | Cmd+Y | Side panel opens with recent visits; clicking an entry navigates. |
| Create a history trail | Navigate across 2–3 pages, then open history panel | Same | Recent entries are present (excluding `about:` pages). |
| Bookmark current page | Ctrl+D | Cmd+D | Star/indicator updates; bookmark appears in the bookmarks bar and/or manager. |
| Toggle bookmarks panel | Menu bar: **Bookmarks → Bookmarks panel** | Same (system menu bar by default) | Side panel shows bookmarks; clicking navigates; focus/scroll behaves. |
| Open bookmarks manager | Ctrl+Shift+O | Cmd+Shift+O | Manager UI opens; remove bookmark updates other surfaces (bar/panel/newtab). |

### Fullscreen

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| Toggle fullscreen | F11 | Ctrl+Cmd+F | Window enters/exits fullscreen; chrome remains usable; focus rings/keyboard shortcuts still work. |

### Multi-window

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| New window | Ctrl+N | Cmd+N | A second window opens with its own tab strip; both windows remain responsive. |
| Detach tab into a new window | Drag a tab out of the tab strip | Same | Tab becomes a new window; tab state (URL/title) preserved. |

### Accessibility (spot checks)

These checks are intentionally lightweight. For deeper debugging, see
[chrome_accessibility.md](chrome_accessibility.md).

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| Keyboard focus order | Use Tab/Shift+Tab across toolbar + address bar + tabs | Same | Focus order follows the **visual** left-to-right order; focused controls show a clear indicator. |
| Screen reader labels | Orca/Narrator: traverse toolbar/address bar | VoiceOver: traverse toolbar/address bar | Controls announce meaningful labels (e.g. “Back”, “Address bar”, “Downloads”). No “unknown”/empty names. |
| Find bar / panel a11y | Open find/downloads/history panels and traverse controls | Same | Panel buttons and rows have labels; focus does not get trapped; Esc closes expected surfaces. |

