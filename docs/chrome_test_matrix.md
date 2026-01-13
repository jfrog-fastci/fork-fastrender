# Chrome manual test matrix (quick, cross-platform)

This is a **quick manual smoke matrix** for browser chrome UX parity across **Linux / macOS /
Windows** (keyboard shortcuts, focus behaviour, OS integration). It’s intended for contributors
making chrome changes where automated tests can’t fully cover native windowing behaviour (fullscreen,
multi-window, OS file manager reveal/open).

For a deeper, step-by-step checklist (session restore, menus, crash paths, etc), see:
[`docs/browser_chrome_manual_test_matrix.md`](browser_chrome_manual_test_matrix.md).

Key notation:

- **Win/Linux** uses <kbd>Ctrl</kbd> and <kbd>Alt</kbd>.
- **macOS** uses <kbd>Cmd</kbd> and <kbd>Option</kbd>.

Note (macOS): <kbd>Cmd</kbd>+<kbd>Tab</kbd> is generally reserved by the OS (app switcher), so tab
cycling should be tested with <kbd>Ctrl</kbd>+<kbd>Tab</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Tab</kbd>.

## Address bar (omnibox)

| Test | Win/Linux shortcut | macOS shortcut | Expected / parity notes |
|---|---|---|---|
| Focus address bar (select all) | Ctrl+L / Ctrl+K / F6 / Alt+D (or click) | Cmd+L / Cmd+K / F6 (or click) | Focus moves to address bar and current text is selected. Typing replaces selection. |
| Navigate | Enter | Enter | Navigates to URL (or search). Address bar updates after commit. |
| Cancel suggestion dropdown | Esc | Esc | If dropdown open (or a suggestion is selected), Esc closes it and restores original input. |
| Cancel editing / blur | Esc (when dropdown already closed) | Esc (when dropdown already closed) | Exits editing and restores the active tab URL. |
| Open input in new tab | Alt+Enter | Option+Enter | Opens resolved navigation in a new foreground tab; current tab remains unchanged. |
| Clipboard in address bar | Ctrl+C/X/V/A | Cmd+C/X/V/A | Uses the OS clipboard; behaves like a normal text field. |

## Navigation (buttons + shortcuts)

| Test | Win/Linux shortcut | macOS shortcut | Expected / parity notes |
|---|---|---|---|
| Back / Forward buttons | Click toolbar buttons | Click toolbar buttons | Enabled/disabled state matches history availability. |
| Back / Forward shortcuts | Alt+Left / Alt+Right | Cmd+[ / Cmd+] | Matches toolbar buttons. |
| Reload | Click reload button, or Ctrl+R / F5 | Click reload button, or Cmd+R (F5 optional) | Reloads current URL. |
| Stop loading | Click stop button (while loading), or Esc | Click stop button (while loading), or Esc | Cancels in-flight navigation/load. |
| Home | Click home button, or Alt+Home | Click home button, or Cmd+Shift+H | Navigates to configured home page. |

## Tabs (create/close/switch/reopen)

| Test | Win/Linux shortcut | macOS shortcut | Expected / parity notes |
|---|---|---|---|
| New tab | Ctrl+T (or “+” button) | Cmd+T (or “+” button) | Creates a new foreground tab (typically `about:newtab`). |
| Close tab | Ctrl+W (or tab close “×” / middle-click tab) | Cmd+W (or tab close “×” / middle-click tab) | Closes active tab. If only one tab exists, close is a no-op. |
| Switch tabs (next/prev) | Ctrl+Tab / Ctrl+Shift+Tab | Ctrl+Tab / Ctrl+Shift+Tab | Tab cycling is stable and updates active highlight + address bar. |
| Switch tabs by number | Ctrl+1..9 | Cmd+1..9 | 1..8 activates that index; 9 activates the last tab. |
| Reopen closed tab | Ctrl+Shift+T | Cmd+Shift+T | Restores the most recently closed tab (repeatable). |

## Find in page

| Test | Win/Linux shortcut | macOS shortcut | Expected / parity notes |
|---|---|---|---|
| Open find bar | Ctrl+F | Cmd+F | Find UI opens and is focused; typing updates match count. |
| Next / previous match | Enter / Shift+Enter (in find bar) | Enter / Shift+Enter (in find bar) | Advances active match and wraps at ends. |
| Close find bar | Esc | Esc | Find UI closes; highlights are cleared; focus returns sensibly. |

## Downloads panel

| Test | Win/Linux shortcut | macOS shortcut | Expected / parity notes |
|---|---|---|---|
| Toggle downloads panel | Ctrl+J (or toolbar icon) | Cmd+Shift+J (or toolbar icon) | Panel opens/closes; focus does not get “stuck”. |
| Start download + progress | (Use direct file URL / “Download link/image”) | Same | Entry appears; progress updates. |
| Cancel + retry | Click Cancel / Retry | Same | Cancel stops; Retry restarts. |
| Open completed | Click Open | Same | Opens with OS default application. |
| Reveal completed | Click Show in Folder | Same | Reveals in OS file manager (Explorer/Finder/xdg-open). |
| Open downloads folder | Click “Show downloads folder” (panel header) | Same | Opens the current download directory in the OS file manager. |
| Change downloads folder | CLI/env | CLI/env | Use `browser --download-dir <path>` or `FASTR_BROWSER_DOWNLOAD_DIR=<path>` and verify downloads land there. |
| Clear completed downloads | (If implemented) | (If implemented) | Removes completed entries without affecting in-progress downloads. |

## History + bookmarks panels

| Test | Win/Linux shortcut | macOS shortcut | Expected / parity notes |
|---|---|---|---|
| Toggle history panel | Ctrl+H | Cmd+Y | Panel opens; search works; clicking an entry navigates. |
| Toggle bookmarks panel | Menu bar: Bookmarks → Bookmarks panel (or page context menu) | Same | Side panel opens/closes; search and navigation behave; focus doesn’t get trapped. |
| Bookmark current page | Ctrl+D | Cmd+D | Bookmark state updates; bookmark appears in bar/manager. |
| Open bookmarks manager | Ctrl+Shift+O | Cmd+Shift+O | Manager opens; search + remove/edit works. |

## Fullscreen

| Test | Win/Linux shortcut | macOS shortcut | Expected / parity notes |
|---|---|---|---|
| Toggle fullscreen | F11 | Ctrl+Cmd+F | Enters/exits fullscreen; shortcuts still work. |

## Multi-window

| Test | Win/Linux shortcut | macOS shortcut | Expected / parity notes |
|---|---|---|---|
| New window | Ctrl+N | Cmd+N | Second window opens with independent tabs. |
| Detach a tab to new window | Drag tab out of strip | Same | Dragging a tab out creates a new window for that tab. |

## Accessibility (spot checks)

| Test | Win/Linux | macOS | Expected / parity notes |
|---|---|---|---|
| Focus order | Tab/Shift+Tab through chrome controls | Same | Focus order matches visual order; focus indicator is visible. |
| Screen reader labels | Narrator/Orca can traverse chrome | VoiceOver can traverse chrome | Controls announce meaningful labels (“Back”, “Address bar”, “Downloads”, etc). |
