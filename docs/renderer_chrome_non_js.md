# Renderer-chrome without JavaScript: interaction roadmap (`chrome-action:`)

This document describes a **no-JavaScript** interaction model for “renderer chrome” (browser UI
rendered by FastRender) using only:

- Trusted **HTML/CSS** for chrome UI layout/styling.
- `chrome-action:` **URL navigations** and **form submissions** to trigger privileged actions in the
  browser process.

It is intended to make it explicit which chrome interactions we can deliver without JS, what engine
features we need to reach parity, and what likely still requires a JS bridge.

Related workstream overview: [`instructions/renderer_chrome.md`](../instructions/renderer_chrome.md).

---

## Core idea

The chrome frame is a **trusted** document (shipped with the binary or loaded from a trusted
location) rendered in the **browser process**.

Instead of running JS, chrome UI elements trigger actions by navigating to special URLs:

- Links/buttons: `href="chrome-action:..."`
- Forms: `action="chrome-action:..." method="get|post"`

The browser process intercepts these navigations/submissions and translates them into internal
actions (e.g. the existing `ui::chrome::ChromeAction` enum used by the egui UI).

### Why this is useful

- **Bootstrap** renderer-chrome interactions before the JS engine/DOM bridge is “browser-grade”.
- Keeps the chrome surface **small and trusted** (fewer moving parts).
- Forces the renderer to support the core HTML/CSS input model (forms, focus, etc).

---

## `chrome-action:` URL format (proposed)

Canonical format:

```
chrome-action:<action>[?<query>]
```

- `<action>` is lowercase `kebab-case` (e.g. `new-tab`, `activate-tab`).
- `<query>` uses `application/x-www-form-urlencoded` encoding (what HTML `<form method=get>`
  generates).

Examples:

- `chrome-action:back`
- `chrome-action:new-tab`
- `chrome-action:close-tab?tab=42`
- `chrome-action:navigate?url=https%3A%2F%2Fexample.com%2F`
- `chrome-action:navigate?url=cats+%26+dogs` (search term)

Note: the parser/formatter uses `url=` as the canonical parameter name for navigations, but accepts
`input=` as a legacy alias (useful if an older chrome HTML template already uses `name=input`).

Implementation note: the repo now has a small parser/formatter with a round-trip regression test:
`src/ui/chrome_action_url.rs`.

### Security / scope rules (non-negotiable)

- `chrome-action:` **must never be reachable from untrusted web content**.
  - If the content frame navigates to `chrome-action:*`, treat it as a blocked navigation.
- The chrome frame must be treated as a different security origin from normal pages (even if the
  renderer is shared).

---

## Feature matrix: egui chrome → HTML/CSS + `chrome-action:`

The table below maps current egui-driven chrome features (see `src/ui/chrome.rs`) to suggested HTML
markup patterns and `chrome-action:` URLs.

> The intent is that a worker can implement the chrome HTML template purely from this table plus
> existing UI state (`BrowserAppState`, `BrowserTabState`, `ChromeState`).

### Legend

- ✅: doable without JS given current/basic engine capabilities (links + form submission).
- ⚠️: doable without JS but needs **engine features** (listed later).
- ❌: likely needs a JS bridge for parity / reasonable UX.

### Back / Forward / Reload / Home

| Feature | Proposed HTML | `chrome-action:` | Notes |
|---|---|---|---|
| Back | `<a class=btn href="chrome-action:back" aria-disabled=...>` | `chrome-action:back` | ✅ Disabled state from `active_tab.can_go_back`. |
| Forward | `<a class=btn href="chrome-action:forward">` | `chrome-action:forward` | ✅ Disabled state from `active_tab.can_go_forward`. |
| Reload | `<a class=btn href="chrome-action:reload">` | `chrome-action:reload` | ✅ Often toggles to “Stop” while loading (see below). |
| Home | `<a class=btn href="chrome-action:home">` | `chrome-action:home` | ✅ Typically navigates to `about:newtab`. |
| Stop loading | `<a class=btn href="chrome-action:stop-loading">` | `chrome-action:stop-loading` | ✅ Mirrors the existing egui chrome “Stop loading” action (`ChromeAction::StopLoading`). |

### Tabs: new / close / activate

| Feature | Proposed HTML | `chrome-action:` | Notes |
|---|---|---|---|
| New tab | `<a class=newtab href="chrome-action:new-tab">+</a>` | `chrome-action:new-tab` | ✅ |
| Activate tab | `<a class=tab href="chrome-action:activate-tab?tab=…">…</a>` | `chrome-action:activate-tab?tab=<TabId>` | ✅ `tab` values come from `BrowserTabState.id`. |
| Close tab | `<form action="chrome-action:close-tab" method=get><input type=hidden name=tab value=…><button>×</button></form>` | `chrome-action:close-tab?tab=<TabId>` | ✅ Avoid nested `<a>` (invalid HTML) by using a form/button. |

### Address bar submit

| Feature | Proposed HTML | `chrome-action:` | Notes |
|---|---|---|---|
| Navigate to typed input | `<form class=omnibox action="chrome-action:navigate" method=get> <input name=url value="…"> </form>` | `chrome-action:navigate?url=...` | ✅ The browser process should run the same resolution as `BrowserTabState::navigate_typed` (URL vs search). |
| Open typed input in new tab | `<button formaction="chrome-action:open-url-in-new-tab" formmethod=get>` | `chrome-action:open-url-in-new-tab?url=...` | ✅ Useful for middle-click / Ctrl+Enter parity; can also be a separate button. |

### Omnibox suggestions (mouse + keyboard)

Omnibox suggestions are a mix of:

- Open tabs (`BrowserAppState.tabs`)
- History/visited (`VisitedUrlStore`, `GlobalHistoryStore`)
- Bookmarks (`BookmarkStore`)
- Remote typeahead (optional; currently fetched outside the worker and cached in `ChromeState.remote_search_cache`)

**Mouse (click) suggestions** are straightforward without JS:

```html
<ul class="omnibox-suggestions" role="listbox">
  <li role="option">
    <a href="chrome-action:navigate?url=https%3A%2F%2Fexample.com%2F">
      <span class="title">Example Domain</span>
      <span class="url">example.com</span>
    </a>
  </li>
</ul>
```

| Feature | Proposed HTML | `chrome-action:` | Notes |
|---|---|---|---|
| Suggestion click → navigate | `<a href="chrome-action:navigate?url=…">` | `chrome-action:navigate?url=…` | ✅ Input can be a URL or a raw search query. |
| Suggestion click → open in new tab | `<a href="chrome-action:open-url-in-new-tab?url=…">` | `chrome-action:open-url-in-new-tab?url=…` | ✅ |
| Keyboard up/down selection | (custom listbox UI) | (same actions) | ❌ Without JS, arrow-key-driven “roving selection” is hard to implement with semantic HTML alone. See gaps/JS section. |
| `datalist`-based suggestions | `<input list=…><datalist>…</datalist>` | submit via `chrome-action:navigate` | ⚠️ Possible no-JS approach, but requires engine support for `<datalist>` suggestion UI (not currently implemented). Styling is also very limited. |
| `<select size>` listbox fallback | `<select size=8 name=url>…</select>` | submit via `chrome-action:navigate` | ⚠️ This gives keyboard navigation “for free”, but options cannot be richly styled and cannot embed icons/secondary text. |

### Context menu / tooltip / status bubble

| Feature | Proposed HTML/CSS | `chrome-action:` | Notes |
|---|---|---|---|
| Tooltip (simple) | `title="Back"` on buttons/links | none | ✅ Depends on engine tooltip support (or accept no-tooltip initially). |
| Tooltip (custom styled) | `:hover::after { content: attr(data-tooltip); … }` | none | ⚠️ Requires generated content + fixed positioning + proper hit testing. |
| Tab context menu (button-driven) | A visible “⋮” button per tab that opens a `<details>` menu | actions as links/forms | ✅ Not a true right-click menu, but works without JS. |
| True right-click context menu | Open at cursor pos on `contextmenu` | actions as links/forms | ❌ Likely needs JS (or an engine-level contextmenu event + popover positioning). |
| Status bubble showing hovered link URL | `a:hover::after { content: attr(href); position: fixed; … }` | none | ⚠️ Works only for links in the same document; cross-frame status (content → chrome) needs host plumbing. |

### Tab reorder / detach

| Feature | Proposed HTML | `chrome-action:` | Notes |
|---|---|---|---|
| Detach tab (menu item) | `<a href="chrome-action:detach-tab?tab=…">Detach</a>` | `chrome-action:detach-tab?tab=<TabId>` | ✅ |
| Reorder tab (buttons) | “Move left/right” buttons | `chrome-action:move-tab?tab=…&delta=±1` | ✅ Low UX, but fully no-JS. |
| Reorder tab (drag) | Drag tabs in strip | `chrome-action:reorder-tabs?...` | ❌ Requires drag events + pointer capture + a way to compute target index. JS is the obvious route. |
| Detach tab (drag out of strip) | Drag outside strip triggers detach | `chrome-action:detach-tab?tab=…` | ❌ Same drag/pointer capture limitations. |

### Downloads / History panels

| Feature | Proposed HTML | `chrome-action:` | Notes |
|---|---|---|---|
| Toggle History panel | Toolbar button | `chrome-action:toggle-history-panel` | ✅ Uses `ChromeState.history_panel_open`. |
| Toggle Downloads panel | Toolbar button | `chrome-action:toggle-downloads-panel` | ✅ Uses `ChromeState.downloads_panel_open`. |
| History entry click → navigate | `<a href="chrome-action:navigate?url=…">` | `chrome-action:navigate?url=…` | ✅ |
| Clear history range | `<form action="chrome-action:clear-history" method=post>…</form>` | `chrome-action:clear-history` | ⚠️ Requires POST form submission plumbing and a defined payload shape. |
| Download item cancel/retry | Buttons per item | `chrome-action:cancel-download?id=…` / `chrome-action:retry-download?id=…` | ⚠️ Requires defining IDs and wiring to the download manager. |
| “Open file” / “Show in folder” | `<a href="chrome-action:open-download?id=…">` | `chrome-action:open-download?...` | ⚠️ Needs OS integration in browser process; action triggering can still be no-JS. |

---

## Engine/interaction gaps for “no-JS parity”

These gaps block a “pure HTML/CSS + chrome-action” chrome from reaching parity with the current egui
chrome UX.

### Input + focus model

- **Robust text editing** for `<input type=text>`:
  - selection, word movement, undo/redo, clipboard, IME composition.
  - (Some of this exists; this is a parity checklist for chrome.)
- **Programmatic focus control** from the browser process:
  - e.g. `Ctrl/Cmd+L` → focus + select-all in address bar.
  - implies an API to set focus to a specific element (by id) in the chrome document.
- **Deterministic focus traversal**:
  - correct Tab/Shift+Tab order across chrome widgets.
  - `:focus-visible` / focus ring behavior consistent with accessibility expectations.

### Event plumbing (browser process ↔ chrome document)

To stay “no JS” while still being interactive, the browser process needs a small event bridge:

- Intercept `chrome-action:` **link activations** and **form submissions**, including:
  - the fully resolved URL (including query)
  - the “submitter” button (for forms with multiple submit buttons)
  - pointer/keyboard modifiers (Cmd/Ctrl/middle click) for expected browser behaviors
- Optional: expose **input change events** (address bar typing) so the browser process can re-render
  suggestions live without JS.

### Drag/pointer capture

Required for “native-feeling” tab strip interactions:

- **Pointer capture** (continue receiving move/up events after leaving element bounds).
- **Drag start / drag move / drag end** events with stable element identity.
- Hit-testing while dragging (drop targets, drag ghost).

### Popups / overlays

- A stable way to implement overlays without JS:
  - `<details>` can work but is limited.
  - The HTML Popover API (`popover`, `popovertarget`) would be ideal, but requires engine support.
- Correct stacking contexts + clipping + `position: fixed` behavior for dropdowns/tooltips.

### `<datalist>` / rich listbox behavior

- If we want no-JS omnibox suggestions with keyboard selection:
  - implement `<datalist>` UI for `<input list=...>` **or**
  - improve `<select size>` styling/appearance to support rich omnibox rows.

---

## What likely requires a JS bridge (for parity)

Even with engine improvements, some interactions are significantly simpler with JS:

- **Rich omnibox suggestion UX**:
  - arrow-key selection with highlight + scroll-into-view
  - mouse hover preview without stealing focus
  - mixed-row layouts (icons, secondary text, action buttons)
  - incremental filtering as the user types
- **Tab drag reorder/detach**:
  - computing target indices
  - animated drag ghosts / drop indicators
  - cross-window drag/drop
- **True context menus**:
  - open on right-click at cursor position, close on click-away/Escape

The “no-JS” roadmap can still deliver a usable chrome (especially for P0/P1 feasibility), but the JS
bridge is the realistic route to match mature browser UX in these areas.
