# CSS Values 5: `if()`, `calc-size()`, and typed `attr()`

FastRender implements a small but high-impact subset of three CSS Values Level 5 value functions.
These functions increasingly appear in framework-generated CSS and in modern authoring patterns.

This note documents what is supported today (and what is intentionally *not*).

## `if()`

### Supported syntax

FastRender parses the CSS Values 5 form:

```css
if(<condition> : <value> ; <condition> : <value> ; <else-value>)
```

Notes:
- Branches are separated with `;`.
- Each conditional branch uses a single `:` separating the condition from the branch value.
- The final branch is the else branch (no `:`) and must be last.

### Supported condition grammar

- Boolean operators: `not`, `and`, `or`
- Parentheses for grouping
- Test functions:
  - `media(<media-query-list>)`
  - `supports(<supports-condition>)`

All other test functions evaluate to `false`.

### Evaluation semantics

`if()` is evaluated during computed-value resolution (in the same phase as `var()` substitution).
Branch selection is **lazy**: FastRender only resolves substitution functions (`var()`, nested
`if()`, typed `attr()`) in the selected branch. This avoids accidentally forcing evaluation of
unselected branches that may contain missing/invalid variables.

Limitations:
- Container queries, style queries, and other proposed `if()` test functions are not implemented.

## `calc-size()`

FastRender supports `calc-size(<basis>, <calc-sum>)` as an intrinsic sizing keyword for size
properties such as:
- `width` / `height`
- `min-width` / `min-height`
- `max-width` / `max-height`
- logical size properties (via their physical counterparts)

### Supported `<basis>`

- `auto`
- `min-content`
- `max-content`
- `stretch` / `fill-available` (mapped to `fill-available`)
- `fit-content`
- `fit-content(<length-percentage>)`
- `<length-percentage>` (including `calc(...)`)

### Evaluation model

FastRender stores the `<calc-sum>` expression with an interned identifier.
During layout, it computes the basis size and substitutes `size` in the expression with that basis
value (in the same coordinate system as the property’s specified size; `box-sizing` is respected).
The resulting expression is evaluated as a `calc(<sum>)` length-percentage.

Limitations:
- Some layout contexts treat `calc-size()` conservatively (e.g. as `auto`) when a full intrinsic
  basis cannot be computed cheaply/safely.

## Typed `attr()`

FastRender supports the CSS Values 5 typed `attr()` function for property values, e.g.:

```css
width: attr(data-w px, 10px);
color: attr(data-color color, red);
z-index: attr(data-z integer, 0);
```

### Supported types

Conservative subset:
- `length` / `length-percentage` (and unit-form like `px`, `rem`, etc.)
- `number`
- `integer`
- `color`
- `url`

### Fallback behavior

The fallback (second argument) is used when:
- The attribute is missing
- The attribute is present but fails to parse as the requested type
- The requested type is unsupported

Limitations:
- Other complex typed `attr()` categories are currently unsupported (fallback is used).
