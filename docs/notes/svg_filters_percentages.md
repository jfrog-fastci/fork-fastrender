# SVG filter percentage resolution

When `filterUnits` or `primitiveUnits` are `userSpaceOnUse`, percentage values on
`x/y/width/height` are resolved against the filtered element's bounding box
(width for `x`/`width`, height for `y`/`height`). In CSS `filter:url(...)`
contexts there is no SVG viewport to anchor user-space percentages, so the
element bbox is the only consistent base.

This rule is shared across filter regions and primitive subregions via
`SvgFilterRegion::resolve` and the `SvgFilter::resolve_primitive_*` helpers in
`svg_filter.rs` to avoid diverging behavior.

Light source coordinates for lighting primitives are also parsed as `SvgLength`
so percentage inputs continue to track the filtered element's bounding box even
when `primitiveUnits` is `userSpaceOnUse`.

For `<fePointLight>` / `<feSpotLight>`, FastRender resolves **numeric**
`x/y/z` (and `pointsAt*`) in the coordinate system established by
`primitiveUnits`, matching Chromium:

- `primitiveUnits="objectBoundingBox"`: numeric values are scaled against the
  element bbox (width for X, height for Y, and average dimension for Z).
- `primitiveUnits="userSpaceOnUse"`: numeric values are treated as offsets from
  the element bbox origin, consistent with our CSS `filter:url(...)` baseline
  documented in `docs/notes/svg_filters_userspace_numbers.md`.

Note: resvg currently diverges here by treating **numeric** light coordinates
as user space even when `primitiveUnits="objectBoundingBox"`.

Paired values that come from a single input (e.g.
`stdDeviation="2"` or `kernelUnitLength="1"`) are still resolved per-axis so
`objectBoundingBox` units respect the bbox width for X and height for Y rather
than averaging the two dimensions.
