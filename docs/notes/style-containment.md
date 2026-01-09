# Style containment (`contain: style`)

FastRender parses and computes the style containment flag (`ComputedStyle::containment.style`) and
applies it (and the implied flag from `content-visibility:auto|hidden`) to a small but
spec-aligned set of **outward-leaking style effects**.

Spec references:

- CSS Containment Module Level 2 — style containment:
  <https://www.w3.org/TR/css-contain-2/#containment-style>
- CSS Lists and Counters Module Level 3 — counters:
  <https://www.w3.org/TR/css-lists-3/#counters>
- CSS Generated Content for Paged Media Level 3 — running strings / `string-set`:
  <https://www.w3.org/TR/css-gcpm-3/#running-strings>
- CSS Generated Content for Paged Media Level 3 — running elements:
  <https://www.w3.org/TR/css-gcpm-3/#running-elements>

## Implemented (current scope)

When a styled element has `containment.style == true`:

1. **CSS counters** are scoped at the subtree boundary.
   - `counter-increment`, `counter-set` (scoped per CSS Containment)
   - `counter-reset` (already creates a new counter instance on the element, so it does not leak)
   - Implicit counter instantiation when incrementing/setting an undefined counter
   - Built-in list item counter behavior (`list-item`, including `<ol reversed>` semantics)
   - The predefined paged-media `footnote` counter used by `float: footnote`

   Semantics: the style-contained element itself still participates in the surrounding counter
   state, but counter increments/sets inside its subtree create fresh counters that do not affect
   siblings outside the subtree (matching the “scoped to a sub-tree” definition in CSS Containment).

2. **Quote depth** (`open-quote`/`close-quote`) is scoped.
   - Quote depth changes inside a style-contained subtree do not affect quote nesting outside the
     subtree.

3. **Paged media running strings** are contained.
   - `string-set` assignments inside a style-contained subtree are ignored when computing the
     per-page running string values used by `string(...)` in `@page` margin boxes.

4. **Paged media running elements** are contained.
   - Running element occurrences inside a style-contained subtree are ignored when resolving
     `element(...)` in `@page` margin boxes.

## Not yet implemented

Style containment in the spec covers additional cross-subtree effects that FastRender either does
not implement yet, or does not currently model as a leaking global state. As more generated content
features land, additional style-scoped state may need to be modeled here.
