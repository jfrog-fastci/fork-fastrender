# DOM compatibility mode

FastRender parses HTML without executing author JavaScript. By default it also avoids applying
non-standard mutations to the parsed DOM.

Some pages rely on early “bootstrap JS” to:

- flip root classes (e.g. `no-js` → `js js-enabled`), or
- populate real `src`/`srcset` URLs from `data-*` lazy-load stashes.

Without JS execution those mutations never happen, which can cause content to stay hidden behind
CSS gates like `html.no-js …` or `img:not([src]) …`.

`DomCompatibilityMode::Compatibility` is an **opt-in**, **generic** set of post-parse DOM mutations
to mirror these common bootstrap steps.

Non-goal: this is **not** a home for hostname/page-specific hacks. Those belong behind
`CompatProfile::SiteCompatibility` / `--compat-profile site` (see
[`docs/notes/site-compat-hacks.md`](site-compat-hacks.md)).

Implementation source of truth: `src/dom.rs::apply_dom_compatibility_mutations`.

## Enabling compatibility mode

- High-level API: `FastRenderConfig::with_dom_compat_mode(DomCompatibilityMode::Compatibility)`
- Lower-level parsing: `DomParseOptions::compatibility()` (or set
  `DomParseOptions { compatibility_mode: DomCompatibilityMode::Compatibility, … }`)
- CLIs: pass `--dom-compat compat` (and optionally `--compat-profile site`) to
  `fetch_and_render`, `render_pages`, `pageset_progress` (run/worker), `bundle_page` (fetch/render),
  or `inspect_frag`.

## Current behavior

### 1) Root class bootstrap (class flips)

- `<html>` and `<body>`: if the class list contains the token `no-js`, remove it and add both `js`
  and `js-enabled` (if not already present).
  - Note: the flip is applied to whichever element contained `no-js`; it does not propagate to the
    other root element.
- `<html>` and `<body>`: ensure the class token `jsl10n-visible` is present.

No other class normalization is performed (for example, `lazyload`/`lazyloaded` are untouched).

### 2) Lazy-load URL lifting (`data-*` → real attributes)

Compatibility mode mirrors the “populate real attributes from `data-*` stashes” bootstrap step
commonly performed by lazy-load libraries.

These lifts are intentionally conservative:

- **Never overwrite** a non-empty, non-placeholder author-provided attribute.
- `src`/`poster` are only overwritten when the existing value is considered a placeholder (below).
- `srcset`/`sizes` are only overwritten when the existing value is empty/missing.

#### Placeholder detection (for `src`/`poster`)

`src`/`poster` values are treated as placeholders when, after trimming ASCII whitespace, they are:

- empty
- start with `#`
- `about:blank` (case-insensitive), optionally followed by `#…` or `?…`
- start with `javascript:`, `vbscript:`, or `mailto:` (case-insensitive)
- a `data:image/gif;base64,…` that decodes to a `1×1` GIF (payload length is capped to keep this
  check cheap)

These placeholder rules are reused anywhere compat mode decides whether to replace an existing
`src`-like attribute (`<img>`, `<iframe>`, `<video>`, `<audio>`, and `<video poster>`).

#### `<img>`

- `src`: if missing or placeholder, copy from the first non-empty candidate among:
  - `data-gl-src`
  - `data-src`
  - `data-lazy-src`
  - `data-original`
  - `data-original-src`
  - `data-url`
  - `data-actualsrc`
  - `data-img-src`
  - `data-hires`
  - `data-src-retina`
- `srcset`: if missing or empty, copy from the first non-empty candidate among:
  - `data-gl-srcset`
  - `data-srcset`
  - `data-lazy-srcset`
  - `data-original-srcset`
  - `data-original-set`
  - `data-actualsrcset`
- `sizes`: if missing or empty, copy from `data-sizes`.

#### `<picture><source>` (and `<source>` generally)

- `srcset`: if missing or empty, copy from the first non-empty candidate among:
  - `data-srcset`
  - `data-lazy-srcset`
  - `data-gl-srcset`
  - `data-original-srcset`
  - `data-original-set`
  - `data-actualsrcset`
- `sizes`: if missing or empty, copy from `data-sizes`.

#### `<iframe>`

- `src`: if missing or placeholder, copy from `data-src`.

#### `<video>`

- `src`: if missing or placeholder, copy from:
  - `data-video-urls` (if present): a comma-separated list (or JSON); prefers the first `.mp4`
    entry when multiple URLs are present, otherwise uses the first non-empty entry.
  - otherwise the first usable candidate from:
    - `data-video-src`
    - `data-video-url`
    - `data-src`
    - `data-src-url`
    - `data-url`
- `poster`: if missing or placeholder, copy from the first usable candidate among:
  - `data-poster`
  - `data-poster-url`
  - `data-posterimage`
  - `data-poster-image`
  - `data-poster-image-override`

For video `src`/`poster` candidates, compat mode also understands a small set of “JSON-ish” stashes
(values beginning with `{`, `[`, or `"`), attempting to parse and extract a URL-like string.
Unparseable JSON-ish values are ignored (not copied verbatim).

#### `<audio>`

- `src`: if missing or placeholder, copy from the first usable candidate among:
  - `data-audio-src`
  - `data-audio-url`
  - `data-src`
  - `data-url`

#### Wrapper propagation (`data-video-urls` / `data-poster-url`)

Some pages store video metadata on a wrapper element and have JS propagate it to a nested `<video>`
at runtime.

When compatibility mode sees a **non-`<video>`** element with a non-empty:

- `data-video-urls`, and/or
- `data-poster-url`

it finds the first descendant `<video>` element (depth-first) and populates its `src` and/or
`poster` using the same placeholder rules as above.

## Why this exists (and why it stays generic)

These mutations are a best-effort mirror of common JS bootstrap behavior to keep static rendering
useful for debugging and pageset regressions. They intentionally avoid hostname-specific logic so
the default pipeline remains spec-shaped and predictable.

Leaving compatibility mode at `DomCompatibilityMode::Standard` (the default) keeps the parsed DOM
free of these extra mutations.
