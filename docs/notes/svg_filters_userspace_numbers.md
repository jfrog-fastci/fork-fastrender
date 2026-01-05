# SVG filters: `userSpaceOnUse` numeric `x/y` for CSS `filter:url(...)`

FastRender applies SVG `<filter>` elements referenced from CSS `filter:url(...)` via
`src/paint/svg_filter.rs`.

This note records how we interpret **numeric** `x/y` values when
`filterUnits="userSpaceOnUse"` (and `primitiveUnits="userSpaceOnUse"`).

## Decision

For CSS-applied SVG filters, FastRender treats **user-space numeric `x/y` as offsets from the
filtered element’s bounding box origin** (the `bbox.min_x/min_y` passed into the filter executor),
*not* as coordinates in a global page/canvas space.

- `x/y` are positioned relative to the element bbox origin.
- `width/height` remain lengths in the same user-space (i.e. they are not scaled by the bbox).

This matches the existing rule for **percentages** documented in
`docs/notes/svg_filters_percentages.md` and keeps filter-region resolution consistent between:

- outset computation (`filter.resolve_region(bbox)` in CSS-space), and
- filter execution (`apply_svg_filter*` with a possibly offset bbox inside a larger layer/pixmap).

## Chromium baseline

Chromium (tested with Chromium 143) keeps the filtered output aligned with the element when
`filterUnits="userSpaceOnUse"` and numeric `x/y` are used.

Minimal repro:

```html
<div id="box"></div>
<style>
  body { margin: 0; background: black; }
  #box {
    position: absolute;
    left: 100px;
    top: 50px;
    width: 20px;
    height: 20px;
    background: red;
    filter: url("data:image/svg+xml,...#f");
  }
</style>
```

Filter document:

```xml
<filter id="f" filterUnits="userSpaceOnUse" x="0" y="0" width="20" height="20">
  <feOffset dx="0" dy="0"/>
</filter>
```

Headless screenshot command (adapted from `docs/notes/svg_filter_filterres.md`):

```bash
cat > /root/snap/chromium/current/userspace_numbers.html <<'HTML'
<!doctype html>
<meta charset="utf-8" />
<style>
  html, body { margin: 0; width: 200px; height: 150px; background: black; }
  #box {
    position: absolute;
    left: 100px;
    top: 50px;
    width: 20px;
    height: 20px;
    background: red;
    filter: url("data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%3E%3Cfilter%20id='f'%20filterUnits='userSpaceOnUse'%20x='0'%20y='0'%20width='20'%20height='20'%3E%3CfeOffset%20dx='0'%20dy='0'/%3E%3C/filter%3E%3C/svg%3E#f");
  }
</style>
<div id="box"></div>
HTML

/snap/chromium/current/usr/lib/chromium-browser/chrome --no-sandbox --headless=new --disable-gpu \
  --run-all-compositor-stages-before-draw \
  --user-data-dir=/root/snap/chromium/current/tmp-profile \
  --window-size=200,150 --force-device-scale-factor=1 \
  --virtual-time-budget=1500 \
  --screenshot=/root/snap/chromium/current/userspace_numbers.png \
  file:///root/snap/chromium/current/userspace_numbers.html
```

The captured screenshot contains the red box at the expected location (e.g. the pixel at
`(110,60)` is red), which would not happen if numeric `x/y` were interpreted relative to the page
origin.

