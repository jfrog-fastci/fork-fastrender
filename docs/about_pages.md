# Internal `about:` pages

FastRender’s desktop `browser` binary exposes a small set of built-in, **offline** pages under the
`about:` scheme.

These pages are implemented in [`src/ui/about_pages.rs`](../src/ui/about_pages.rs) and rendered
through the normal UI worker pipeline (they are **not** `chrome://` renderer-chrome pages).

If you add/remove an internal page, keep this document consistent with the `ABOUT_*` constants and
`ABOUT_PAGE_URLS` in `src/ui/about_pages.rs`.

## Built-in pages (user-facing + debugging)

| URL | Purpose / expected content |
| --- | --- |
| `about:newtab` | Start page used for new tabs and first-run fallback. Shows a search box plus bookmarks + recently visited history **when snapshot data is available**. |
| `about:settings` | Offline settings summary for the chrome UI. Today this is mostly a debugging surface: it shows the *effective* appearance settings (e.g. accent color) and points at the relevant env-var overrides. |
| `about:help` | Offline help page (usage notes + keyboard shortcuts). Should stay usable even when the network stack is broken. |
| `about:version` | Build/version info (crate version, git hash when available, build profile). Useful for bug reports. |
| `about:gpu` | wgpu adapter/backend selection used by the windowed UI. Best-effort: headless runs do not initialize wgpu so fields may be `"unknown"`. |
| `about:processes` | **Multiprocess placeholder**. Today it shows a best-effort snapshot of currently open tabs (`AboutPageSnapshot.open_tabs`) and (once implemented) a derived **Site** column computed from each tab URL. The “Renderer” / “Network” columns are placeholders for future process-per-site work. |

### `about:processes` expectations (important)

`about:processes` is intended as a *chrome debugging* page:

- **Today (single-process):**
  - It should render an **open-tabs snapshot** (tab id + URL) when the front-end populates
    `AboutPageSnapshot.open_tabs`.
  - The **Site** column is intended to be derived from each tab URL using the same site isolation
    rules as the multiprocess model (i.e. the `SiteKey` derivation described in
    [`docs/site_isolation.md`](site_isolation.md)). This is currently not fully implemented.
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
- `about:test-form` — a minimal form for interaction/input testing.

