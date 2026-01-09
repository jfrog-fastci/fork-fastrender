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

1. **CSS counters** are contained at the subtree boundary.
   - `counter-reset`, `counter-increment`, `counter-set`
   - Implicit counter instantiation when incrementing/setting an undefined counter
   - Built-in list item counter behavior (`list-item`, including `<ol reversed>` semantics)
   - The predefined paged-media `footnote` counter used by `float: footnote`

   Semantics: counter state is snapshotted on entry to the style-contained subtree; updates inside
   the subtree do not affect the counter state visible outside.

2. **Paged media running strings** are contained.
   - `string-set` assignments inside a style-contained subtree are ignored when computing the
     per-page running string values used by `string(...)` in `@page` margin boxes.

3. **Paged media running elements** are contained.
   - Running element occurrences inside a style-contained subtree are ignored when resolving
     `element(...)` in `@page` margin boxes.

## Not yet implemented

Style containment in the spec covers additional cross-subtree effects that FastRender either does
not implement yet, or does not currently model as a leaking global state. Notably, **quote depth**
(`open-quote`/`close-quote`) is not yet scoped by `contain: style` in FastRender.

