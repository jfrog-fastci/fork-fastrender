# SVG filters: `feTurbulence` + `feDisplacementMap` (implementation semantics)

FastRender’s SVG filter implementation lives in:

- [`src/paint/svg_filter.rs`](../../src/paint/svg_filter.rs)
- [`src/paint/svg_filter/turbulence.rs`](../../src/paint/svg_filter/turbulence.rs)

This note records the **exact semantics we implement today** for `feTurbulence` and
`feDisplacementMap`, especially where behavior is subtle and easy to regress:

- `color-interpolation-filters` (generator vs sampling primitives)
- `primitiveUnits` / objectBoundingBox scaling rules
- `filterRes` resampling (and interaction with DPR / anisotropic scaling)
- pixel sampling conventions and out-of-bounds behavior

Related notes:

- [SVG filters: `color-interpolation-filters`](svg_filters_color_interpolation_filters.md)
- [SVG `filterRes` mapping](svg_filter_filterres.md)
- [SVG filter percentage resolution](svg_filters_percentages.md)

## Coordinate system recap (FastRender filter engine)

At runtime we execute the filter graph over `tiny_skia::Pixmap` surfaces:

- All intermediate surfaces are **premultiplied RGBA8** (`PremultipliedColorU8`).
- The filter graph runs in **device pixel space** for the current working surface.
- `apply_svg_filter()` receives:
  - `scale`: device pixel ratio (DPR) used to interpret numeric values that are in CSS px
  - `bbox`: element bbox in device pixels (used for `filterUnits`/`primitiveUnits` resolution)

When `filterRes` is specified, `apply_svg_filter()` allocates a *working pixmap* of size
`filterRes` and resamples into/out of it (see `apply_svg_filter()` / `resize_pixmap()` /
`resample_pixmap_region()` in `src/paint/svg_filter.rs`).

Inside `apply_svg_filter_scaled()` the effective “CSS-to-filter-pixels” scales are:

- No `filterRes`: `scale_x = scale_y = DPR`
- With `filterRes`:
  - `scale_x = DPR * (filterRes_w / filter_region_w_device)`
  - `scale_y = DPR * (filterRes_h / filter_region_h_device)`

These `scale_x/scale_y` are passed into each primitive step and are the bridge between:

- **CSS/user units** (`primitiveUnits="userSpaceOnUse"`)
- **objectBoundingBox units** (`primitiveUnits="objectBoundingBox"`)
- **working filter pixels** (potentially `filterRes`-scaled)

## `feTurbulence`

### Parsing + defaults

Parsing happens in `parse_fe_turbulence()` (`src/paint/svg_filter.rs`):

- `baseFrequency`:
  - Parsed as a list of floats (`parse_number_list()`).
  - Defaults:
    - `fx = 0.05` when missing/empty.
    - `fy = fx` when the second value is missing.
  - Negative values clamp to `0.0` (`.max(0.0)`).
- `seed`:
  - Parsed as `f32`, default `0.0`.
  - Rounded (`.round()`) to the nearest integer.
  - Negative values clamp to `0`.
  - Stored as `u32`.
- `numOctaves`:
  - Parsed as `u32`, default `1`.
  - Clamped to `[1, MAX_TURBULENCE_OCTAVES]` (`MAX_TURBULENCE_OCTAVES = 8`).
- `stitchTiles`:
  - True if the attribute is `"stitch"`, `"true"`, or `"1"` (case-insensitive).
  - Otherwise false (default).
- `type`:
  - `"fractalNoise"` => `TurbulenceType::FractalNoise`
  - Anything else (including missing) => `TurbulenceType::Turbulence`

### Noise function + value mapping

Noise generation happens in `turbulence::render_turbulence()`:

- Base noise is classic 2D Perlin (`perlin()`), with a permutation table derived from `seed`
  (`build_permutation()`).
- For each octave:
  - Sample `perlin(x * freq_x, y * freq_y)`
  - If `type="turbulence"`, apply `abs()` **per octave** (`noise.abs()`).
  - Accumulate with amplitudes `1.0, 0.5, 0.25, …`.
- Normalize by the sum of amplitudes across octaves.
- Map into `[0, 1]` via:
  - `mapped = clamp01(normalized * 0.5 + 0.5)`

Important: with `baseFrequency="0 0"` (or clamped-to-zero), Perlin is skipped and the primitive
produces `normalized = 0` => `mapped = 0.5` everywhere.

### Output channels

Current channel policy (in `render_turbulence()`):

- RGB are **not independent**: `R == G == B` (monochrome noise).
- Alpha is always fully opaque: `A = 1.0` (byte `255`).

### `color-interpolation-filters` for a generator primitive

`feTurbulence` is a **generator**: it does not sample any input surface, it synthesizes pixels.

FastRender treats the computed `mapped` value as being in the step’s
`color-interpolation-filters` space:

- `color-interpolation-filters="sRGB"`: write `mapped` directly as an sRGB byte.
- `color-interpolation-filters="linearRGB"`:
  - treat `mapped` as a **linear** value
  - encode it to sRGB bytes via `linear_to_srgb()` before storing into the RGBA8 pixmap

This is required so that downstream primitives that *decode* into linearRGB recover the intended
numeric `mapped` value (see the regression test `turbulence_midgray_displacement_map_is_nearly_identity_in_linear_rgb`
in `tests/svg_filter_turbulence.rs`).

### Coordinate system (`primitiveUnits`) + `filterRes`/DPR scaling

Implementation detail: `render_turbulence()` operates in **working-pixmap pixel coordinates** and
does not currently consult `SvgFilter::primitive_units` or the element bbox.

Concretely:

- Per-pixel sampling uses `(x, y)` as **integer pixel indices** in the working pixmap.
- `baseFrequency` is interpreted as “cycles per working pixel”.
- As a result, the *visible* frequency in CSS/user units scales with the effective `scale_x/scale_y`
  chosen by the filter engine:
  - higher DPR and/or higher `filterRes` => higher apparent frequency in CSS pixels
  - lower `filterRes` => lower apparent frequency (noise stretches when resampled back)

### `stitchTiles` (wrapping algorithm)

When `stitchTiles` is enabled, we adjust frequencies so the noise value matches on the primitive
subregion edges:

- In `render_turbulence()` we compute:
  - `stitch_width = region.width - 1` (clamped to at least 1)
  - `stitch_height = region.height - 1` (clamped to at least 1)
  - The `-1` means “make the first and last pixel identical”, matching how the subregion is
    rasterized as discrete pixels.
- For each octave and each axis (`adjust_frequency()`):
  - `wrap = round(freq * extent)` (forced to be non-zero if `freq != 0`)
  - `adjusted_freq = wrap / extent`
  - Pass `wrap` into the Perlin hash as a period (see `wrap_index()`).
- This forces the Perlin lattice indices to repeat with period `wrap`, and by choosing
  `adjusted_freq` such that `extent * adjusted_freq == wrap`, the coordinate at the far edge lands
  on the same lattice phase as the origin.

The edge stitch behavior is regression-tested by `turbulence_stitches_edges` in
`tests/svg_filter_turbulence.rs`.

## `feDisplacementMap`

### Parsing

`parse_fe_displacement_map()` in `src/paint/svg_filter.rs` parses:

- `in` / `in2` (case-insensitive attribute lookup)
- `scale` as a float (`parse_number()`; default `0.0`)
- `xChannelSelector` / `yChannelSelector` (`R|G|B|A`, default `A`)

### Scale resolution (`primitiveUnits` and `filterRes` anisotropy)

In `apply_primitive()` the `scale` attribute is converted into a displacement magnitude in **working
pixels**:

1. Resolve the raw `scale` number through `primitiveUnits`:
   - `SvgFilter::resolve_primitive_scalar(scale, css_bbox)`
   - `primitiveUnits="userSpaceOnUse"`: scalar is used as-is (CSS/user units).
   - `primitiveUnits="objectBoundingBox"`: scalar is multiplied by the **average** bbox dimension:
     `scale * 0.5 * (bbox.width + bbox.height)`.
2. Multiply by the average pixel scale:
   - `scale_avg = 0.5 * (scale_x + scale_y)`
   - This makes displacement **isotropic** even when `filterRes` introduces anisotropic scaling.

Net effect: `dx` and `dy` are scaled by the **same** scalar, and that scalar changes when `filterRes`
changes (because `scale_x/scale_y` change).

### Displacement math + sampling

The pixel kernel is implemented in `apply_displacement_map()` (`src/paint/svg_filter.rs`):

For each output pixel `(x, y)`:

1. Sample the displacement-map input (`in2`) at `(x, y)` using bilinear sampling in premultiplied
   RGBA (`sample_premultiplied()`).
2. Convert that sample to **unpremultiplied** RGBA floats (`unpremultiply_sample()`).
3. Extract channels from the unpremultiplied sample:
   - `dx = (channel(map, xChannelSelector) - 0.5) * scale`
   - `dy = (channel(map, yChannelSelector) - 0.5) * scale`
4. Sample the primary input (`in1`) at `(x + dx, y + dy)` using the same bilinear premultiplied
   sampler.
5. Store the sampled premultiplied values back into an RGBA8 pixmap.

### Pixel coordinate convention (centers vs corners)

FastRender treats integer coordinates as the sample positions:

- A sample at `(x = 0.0, y = 0.0)` reads exactly pixel `(0, 0)`.
- Fractional positions bilinearly interpolate between the four neighboring pixels.

This convention is encoded by `sample_premultiplied()` using:

- `x0 = floor(x)`, `tx = x - x0` (similarly for `y`)

### Out-of-bounds sampling

Out-of-bounds sampling is treated as **transparent black**:

- If any of the bilinear taps fall outside the pixmap, that tap contributes 0.
- We do **not** clamp coordinates to the edge.

This is done in `sample_premultiplied()` by skipping contributions when `sx/sy` are out of range.

### `color-interpolation-filters` for a sampling primitive

`feDisplacementMap` is a **sampling** primitive: it reads two input surfaces and interpolates them.

When the step’s `color-interpolation-filters` is `linearRGB`, `apply_displacement_map()`:

- clones both input pixmaps
- re-encodes them in-place from sRGB bytes to linearRGB (`reencode_pixmap_to_linear_rgb()`)
- performs displacement-map channel selection + bilinear interpolation in that space
- re-encodes the output back to sRGB (`reencode_pixmap_to_srgb()`) for storage as RGBA8

When the step uses `sRGB`, no conversion is performed.

## Cross-cutting: filter-region clipping + `filterRes`

Two engine-level details matter for both primitives:

1. **Filter-region clipping happens before the graph runs.**
   `apply_svg_filter_scaled()` calls `clip_to_region(pixmap, filter_region)` so pixels outside the
   resolved filter region do not leak into sampling primitives.

2. **`filterRes` resamples the entire filter region into a working surface.**
   Primitives see the filter graph in the working surface’s pixel grid; generator primitives (like
   `feTurbulence`) will generate at that resolution, and sampling primitives (like
   `feDisplacementMap`) will sample in that resolution.

See [svg_filter_filterres.md](svg_filter_filterres.md) for the exact filterRes mapping when the
filter region is offset/clipped.

## Regression coverage (tests + fixtures)

### Unit tests (parsing + math)

- `src/paint/svg_filter.rs`:
  - `turbulence_base_frequency_parses_pair`
  - `turbulence_base_frequency_clamps_negative_to_zero`
  - `turbulence_negative_base_frequency_clamps_to_zero`
- `tests/svg_filter_turbulence.rs`:
  - `turbulence_is_deterministic`
  - `turbulence_seed_changes_output`
  - `turbulence_stitches_edges`
  - `turbulence_midgray_displacement_map_is_nearly_identity_in_linear_rgb`

### Integration fixtures

- Golden fixture exercising `feDisplacementMap` under both `linearRGB` and `sRGB`:
  - Fixture: `tests/fixtures/html/svg_filter_color_interpolation_filters.html`
    (`filter id="cif-displacement-*"`).
  - Golden test: `tests/paint/svg_filter_color_interpolation_golden.rs`
  - Golden image: `tests/fixtures/golden/svg_filter_color_interpolation_filters.png`
- Fuzz corpus sample using both primitives (useful as a minimized repro input):
  - `tests/fuzz_corpus/svg_filters.svg` (`filter id="wavy"`)
- Real-world pageset fixture using `feTurbulence` with `stitchTiles="stitch"`:
  - `tests/pages/fixtures/foxnews.com/assets/795bf940a1c9a2a4e880a25b9b697ad7.svg`
