# Internal `about:` pages

FastRender’s desktop `browser` binary exposes a small set of built-in, **offline** pages under the
`about:` scheme.

These pages are implemented in [`src/ui/about_pages.rs`](../src/ui/about_pages.rs) and rendered
through the normal UI worker pipeline (they are **not** `chrome://` renderer-chrome pages).

Implementation note: the HTML for these pages loads a shared stylesheet from
`chrome://styles/about.css` (source: [`assets/chrome/about.css`](../assets/chrome/about.css)). This is
a **small allowlisted** `chrome://` asset that is only permitted when the initiating document is an
`about:` page (see
[`src/ui/about_pages_fetcher.rs`](../src/ui/about_pages_fetcher.rs)). Untrusted web pages must not be
able to fetch arbitrary `chrome://...` resources.

If you add/remove an internal page, keep this document consistent with the `ABOUT_*` constants and
`ABOUT_PAGE_URLS` in `src/ui/about_pages.rs`.

## Opening `about:` pages

- Type an `about:` URL directly into the browser address bar (omnibox), e.g. `about:gpu`.
- Omnibox autocomplete suggests built-in `about:` pages when the user starts typing `about...` (it is
  backed by `ABOUT_PAGE_URLS`).
- Some pages support a `?q=` query parameter for filtering/search (notably `about:history`,
  `about:bookmarks`, and `about:processes`).

## Built-in pages (user-facing + debugging)

| URL | Purpose / expected content |
| --- | --- |
| `about:newtab` | Start page used for new tabs and first-run fallback. Shows a search box plus bookmarks + recently visited history **when snapshot data is available**. |
| `about:settings` | Offline settings summary for the chrome UI. Today this is mostly a debugging surface: it shows the *effective* appearance settings (e.g. accent color), key persisted paths (session/bookmarks/history/download directory), and points at the relevant env-var overrides. |
| `about:help` | Offline help page (usage notes + keyboard shortcuts). Should stay usable even when the network stack is broken. |
| `about:version` | Build/version info (crate version, git hash when available, build profile). Useful for bug reports. |
| `about:gpu` | wgpu adapter/backend selection used by the windowed UI. Best-effort: headless runs do not initialize wgpu so fields may be `"unknown"`. |
| `about:processes` | **Multiprocess/process-assignment debugging page** (still a placeholder for the final architecture). Today it shows a best-effort snapshot of currently open tabs (`AboutPageSnapshot.open_tabs`) including a derived **Site** column (and renderer process IDs when available), plus summary/grouping tables. |

### `about:processes` expectations (important)

`about:processes` is intended as a *chrome debugging* page:

- **Today (single-process):**
  - It should render an **open-tabs snapshot** (tab id + URL) when the front-end populates
    `AboutPageSnapshot.open_tabs`.
  - The **Site** column is derived from each tab URL (best-effort). When the front-end provides an
    explicit `site_key`, that should match the `SiteKey` derivation described in
    [`docs/site_isolation.md`](site_isolation.md); otherwise the page falls back to a simple
    URL-derived label.
  - Renderer/network process assignment is best-effort and may show as “unassigned” / “not
    implemented” in single-process builds.
- **Future (multiprocess):**
  - It should show real tab→process assignments (renderer + network) used by FastRender’s
    multiprocess architecture.

## Other internal `about:` pages (implemented today)

These also live in `src/ui/about_pages.rs` and are part of the `ABOUT_PAGE_URLS` registry:

| URL | Purpose |
| --- | --- |
| `about:blank` | Empty document used as a safe “base URL” for internal pages (prevents accidental relative-URL resolution against a previous network origin). |
| `about:error` | Deterministic error page used for navigation failures that render an error document. (Unsupported schemes are rejected earlier and do **not** render this page.) |
| `about:history` | Searchable global history view (offline). |
| `about:bookmarks` | Searchable bookmarks view (offline). |

## Test-only deterministic pages

These pages exist to make offline/manual repros and integration tests deterministic:

- `about:test-scroll` — a simple tall page for scroll/viewport behavior.
- `about:test-heavy` — a large DOM intended to make cancellation/timeout behavior observable.
- `about:test-layout-stress` — a **width-sensitive** layout stress fixture (auto-fit grid + wrapping
  text) used for resize/scroll responsiveness benchmarks.
- `about:test-form` — a minimal form for interaction/input testing.
