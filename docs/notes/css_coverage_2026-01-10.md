# CSS coverage: evidence-driven compatibility fixes (2026-01-10)

This repo has an evidence tool (`src/bin/css_coverage.rs`) that scans fixture pages in
`tests/pages/fixtures` and ranks the most frequent CSS that is either:

- unknown (property name not recognized), or
- known-but-rejected (property recognized, but `supports_declaration()` returns `false` for the
  sampled values).

This note records a small, high-leverage batch of fixes implemented based on that ranked output.

## Top missing items implemented

### Vendor-prefixed IE10 flexbox properties (`-ms-flex-*`)

These were among the highest-frequency *unknown* properties in fixture CSS. We now recognize and
map them onto existing modern flexbox computed style fields:

- `-ms-flex-pack` → `justify-content` mapping (`start/end/center/justify/distribute/stretch`)
- `-ms-flex-align` → `align-items`
- `-ms-flex-item-align` → `align-self` (`auto` clears)
- `-ms-flex-order` → `order`
- `-ms-flex-positive` → `flex-grow`
- `-ms-flex-negative` → `flex-shrink`
- `-ms-flex-preferred-size` → `flex-basis` (`auto/content/length/0`)

### Common value aliases / shorthands seen in fixtures

- `justify-content: left | right | stretch` (legacy aliases + stretch)
- `text-align: -webkit-match-parent` (alias → `match-parent`)
- `border-color` shorthands with 1–4 colors (validated as a list, not a single color token)
- `overflow` two-value shorthand (`overflow-x` + `overflow-y`)

All of the above were wired into `supports_declaration()` so `@supports` stays conservative and
only returns `true` when we actually implement the value/property.

## Evidence snapshot (css_coverage)

From a local run of `css_coverage --top 30` over `tests/pages/fixtures`:

- `known_style_properties`: **458 → 465** (+7)
- `unknown_style_properties`: **275 → 268** (-7)

After these changes, the sampled rejected values list shrank substantially; remaining common
rejections included:

- `align-items: row`
- `container-type: scroll-state`
- `visibility: "hidden"`

