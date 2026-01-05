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
when `primitiveUnits` is `userSpaceOnUse`. resvg interprets the **numeric**
`x/y/z` attributes on `<fePointLight>` / `<feSpotLight>`
in user space regardless of `primitiveUnits`, so FastRender resolves those
numeric coordinates without object-bbox scaling (see `resolve_light_point`).

Paired values that come from a single input (e.g.
`stdDeviation="2"` or `kernelUnitLength="1"`) are still resolved per-axis so
`objectBoundingBox` units respect the bbox width for X and height for Y rather
than averaging the two dimensions.
