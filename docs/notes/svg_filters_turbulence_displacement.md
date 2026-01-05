# SVG filters: `feTurbulence` + `feDisplacementMap` (Chrome-aligned semantics)

FastRender’s SVG filter implementation lives in:

- [`src/paint/svg_filter.rs`](../../src/paint/svg_filter.rs)
- [`src/paint/svg_filter/turbulence.rs`](../../src/paint/svg_filter/turbulence.rs)

This note records the **exact semantics we implement today** for `feTurbulence` and
`feDisplacementMap`, especially where behavior is subtle and easy to regress:

- `color-interpolation-filters` (generator vs sampling primitives)
- `primitiveUnits` / objectBoundingBox scaling rules
- `filterRes` resampling + its Chrome-compatibility exception for `feDisplacementMap`
- pixel sampling conventions and out-of-bounds behavior

Related notes:

- [SVG filters: `color-interpolation-filters`](svg_filters_color_interpolation_filters.md)
- [SVG `filterRes` mapping](svg_filter_filterres.md)
- [SVG filter percentage resolution](svg_filters_percentages.md)

## Coordinate system recap (FastRender filter engine)

At runtime we execute the filter graph over `tiny_skia::Pixmap` surfaces:

- All intermediate surfaces are **premultiplied RGBA8** (`PremultipliedColorU8`).
- The filter graph runs in **“working surface pixel space”** for the current filter execution.

`apply_svg_filter_with_cache()` (in `src/paint/svg_filter.rs`) computes per-render scales:

- `scale_x` / `scale_y`: **CSS/user units → working pixels** conversion factors passed into each
  primitive.
  - Without `filterRes`: `scale_x = scale_y = DPR`.
  - With `filterRes` (and no displacement-map exception):
    - `scale_region_x = filterRes_w / filter_region_w_device`
    - `scale_region_y = filterRes_h / filter_region_h_device`
    - `scale_x = DPR * scale_region_x`
    - `scale_y = DPR * scale_region_y`
    where `filter_region_*_device` is the resolved filter-region size in device pixels.
- `surface_origin_css`: the CSS-space origin of the working surface. This is needed when the filter
  engine allocates a working surface that is not aligned to `(0, 0)` in CSS space (e.g. offset
  filter regions, `filterRes` resampling, or when the resolved filter region extends outside the
  destination pixmap and we temporarily allocate a larger working surface). `feTurbulence` uses this
  to evaluate noise in a stable user-space coordinate system.

### `filterRes` handling

When `filterRes` is specified, `apply_svg_filter_with_cache()` normally allocates a working pixmap
of size `filterRes` and resamples into/out of it (see `resize_pixmap()` /
`resample_pixmap_region()`).

**Chrome compatibility exception:** Chrome ignores the SVG 1.1 `filterRes` attribute (it was removed
in SVG 2). For parity, FastRender treats `filterRes` as *unset* when the filter graph contains
`feDisplacementMap` (implemented as an early return in `apply_svg_filter_with_cache()` that bypasses
the `filterRes` path whenever any step matches `FilterPrimitive::DisplacementMap { .. }`).

Net effect:

- Filters *without* displacement maps: `filterRes` affects the working resolution (and thus
  `scale_x/scale_y`).
- Filters *with* displacement maps: `filterRes` is ignored and the graph runs at the default filter
  resolution (`scale_x = scale_y = DPR`).

## `feTurbulence`

Implementation is in `parse_fe_turbulence()` (`src/paint/svg_filter.rs`) and
`turbulence::render_turbulence()` (`src/paint/svg_filter/turbulence.rs`).

The turbulence generator is **ported from resvg’s** `filter::turbulence` implementation to match
Chromium/Skia behavior (see the top-of-file comment in `turbulence.rs`).

### Parsing + defaults

Parsing happens in `parse_fe_turbulence()`:

- `baseFrequency`:
  - Parsed as a list of floats (`parse_number_list()`).
  - Defaults:
    - Missing/empty attribute => `fx = 0.0`.
    - Missing second value => `fy = fx`.
  - Non-finite values become `0.0`.
  - Negative values clamp to `0.0`.
- `seed`:
  - Parsed as `f32`, default `0.0`.
  - Coerced to an integer via Rust’s `as i32` cast (truncates toward zero, preserves sign).
  - Non-finite values become `0`.
  - Note: the internal noise tables then normalize the seed to a positive range (see
    `TurbulenceTables::new()` / `normalize_seed()` in `turbulence.rs`).
- `numOctaves`:
  - Parsed as `u32`, default `1`.
  - Clamped to `[1, MAX_TURBULENCE_OCTAVES]` (`MAX_TURBULENCE_OCTAVES = 8`).
- `stitchTiles`:
  - True if the attribute is `"stitch"`, `"true"`, or `"1"` (case-insensitive).
  - Otherwise false (default).
- `type`:
  - `"fractalNoise"` => `TurbulenceType::FractalNoise`
  - Anything else (including missing) => `TurbulenceType::Turbulence`

### Noise algorithm + value mapping

Noise generation happens in `turbulence::render_turbulence()`:

- The implementation uses classic gradient noise (Perlin-style), with:
  - a lattice selector table (`TurbulenceTables::lattice_selector`)
  - per-channel gradient vectors (`TurbulenceTables::gradient`)
- **Channels are independent:** noise is generated for `R`, `G`, `B`, and **`A`** separately
  (`CHANNELS = 4`).
- The main per-channel function is `turbulence()`:
  - Each octave samples `noise2(channel, x, y, tables, stitch_info)` (≈ `[-1, 1]`).
  - `type="fractalNoise"`: accumulate signed noise (no `abs()`):
    - `sum += noise / ratio`
  - `type="turbulence"`: accumulate absolute value (the `abs()` happens **per octave**):
    - `sum += abs(noise) / ratio`
  - `ratio` doubles each octave (`1, 2, 4, …`) and `x/y` double each octave (classic fractal
    Brownian motion setup).
  - There is **no normalization by the octave amplitude sum**; the final mapping relies on clamping
    (matching resvg/Chromium behavior).
- Mapping from `sum` to bytes in `render_turbulence()`:
  - `fractalNoise`: `((sum * 255.0) + 255.0) / 2.0` (i.e. `(sum + 1) / 2` in `[0, 1]`)
  - `turbulence`: `sum * 255.0`
  - Then clamp to `[0, 255]` and round by `+0.5` before truncation (matching resvg’s rounding).

**`baseFrequency=0` fast-path:** if both base frequencies are zero, we skip table generation and per
pixel sampling and fill a constant output that matches resvg/Chromium:

- `fractalNoise`: all channels (`R/G/B/A`) are constant `0.5` (including alpha).
  - These are **unpremultiplied** channel values. Since filter surfaces are stored premultiplied,
    the stored RGB bytes are multiplied by `A`:
    - `color-interpolation-filters="sRGB"`:
      - unpremultiplied `R=G=B=A=0.5` (`128`)
      - stored bytes are roughly `R=G=B≈64, A=128`
    - `color-interpolation-filters="linearRGB"`:
      - unpremultiplied `R=G=B=A=0.5` in linear space
      - RGB are encoded to sRGB (linear `0.5` ≈ sRGB byte `188`) before storage
      - stored bytes are roughly `R=G=B≈94, A=128`
- `turbulence`: all channels are `0` (fully transparent)

### Output channel policy

`feTurbulence` is a generator primitive that outputs **premultiplied RGBA8**:

- RGB channels are **not** forced to be equal (not monochrome).
- Alpha is **not** forced to `255`; it is generated as its own noise channel.
- If the generated alpha byte is `0`, the output pixel is fully transparent.

### `color-interpolation-filters` (generator primitive semantics)

For generator primitives, FastRender must decide how to encode computed numeric channel values into
the pixmap’s stored bytes (which are always sRGB-encoded bytes in our engine).

`render_turbulence()` treats the generated channel bytes as being in the step’s
`color-interpolation-filters` space:

- `color-interpolation-filters="sRGB"`:
  - write the generated bytes as sRGB values
  - premultiply `R/G/B` by `A` (`multiply_alpha_u8`)
- `color-interpolation-filters="linearRGB"`:
  - treat the generated bytes as **linearRGB**
  - convert them to sRGB for storage using resvg’s `LINEAR_RGB_TO_SRGB_TABLE`
  - conversion is done in a way that matches resvg/Chromium (premultiply → demultiply →
    linear→sRGB → premultiply), so downstream primitives that decode to linearRGB see the intended
    numeric values.

Regression coverage:

- Unit test: `turbulence_encodes_channels_based_on_color_interpolation_filters`
  (`src/paint/svg_filter.rs`)
- Integration test: `turbulence_midgray_displacement_map_is_nearly_identity_in_linear_rgb`
  (`tests/svg_filter_turbulence.rs`)

### Coordinates: `primitiveUnits`, bbox translation, and `filterRes`/DPR

In `render_turbulence()` each output pixel coordinate `(x_px, y_px)` is converted into **user-space
coordinates** (CSS units) before sampling noise:

```
x_user = surface_origin_css.x + x_px / scale_x
y_user = surface_origin_css.y + y_px / scale_y
```

Important (Chrome-aligned) behavior:

- `feTurbulence` treats its noise coordinate system as user-space even when
  `primitiveUnits="objectBoundingBox"` (see the comment in `render_turbulence()`).
  - In other words: `primitiveUnits` does **not** change how `baseFrequency` or `(x, y)` are
    interpreted for turbulence.
- Because coordinates divide by `scale_x/scale_y`, the turbulence pattern is **scale-invariant in
  CSS units**:
  - increasing DPR or enabling `filterRes` increases sampling density
  - but does not change the underlying noise frequency in CSS/user space

Regression coverage:

- `turbulence_userspace_translation_changes_pattern` (`tests/svg_filter_turbulence.rs`)
- `turbulence_userspace_translation_changes_pattern_with_filter_res` (`tests/svg_filter_turbulence.rs`)

### `stitchTiles` (wrapping algorithm)

Stitching is implemented inside `turbulence()` / `noise2()`:

1. Convert the rendered pixel tile size back into **user-space units**:
   - `tile_width = region.width / scale_x`
   - `tile_height = region.height / scale_y`
2. Adjust base frequencies so the tile can be made periodic:
   - If `base_freq_x != 0`, choose between:
     - `lo_freq = floor(tile_width * base_freq_x) / tile_width`
     - `hi_freq = ceil(tile_width * base_freq_x) / tile_width`
     picking whichever is closer in relative error (same for `y`).
3. Compute `StitchInfo`:
   - `width = round(tile_width * base_freq_x)` (implemented as `+0.5` then cast to `i32`)
   - `wrap_x = ceil(tile_x * base_freq_x + PERLIN_N + width)` (same for `y`)
4. `noise2()` uses `wrap_x/wrap_y` to wrap lattice coordinates so noise repeats at the tile edges.
5. Each octave doubles the stitch period (`width/height`) and updates `wrap_x/wrap_y` by:
   - `wrap_x = 2*wrap_x - PERLIN_N` (same for `y`)

Regression coverage:

- `turbulence_stitches_edges` (`tests/svg_filter_turbulence.rs`)
- `turbulence_stitches_edges_with_offset_filter_region` (`tests/svg_filter_turbulence.rs`)

## `feDisplacementMap`

Implementation is in `parse_fe_displacement_map()` and the displacement branch of
`apply_primitive()` (`src/paint/svg_filter.rs`), plus:

- `apply_displacement_map()`
- `sample_nearest_premultiplied()`

### Parsing

`parse_fe_displacement_map()` parses:

- `in` / `in2` (case-insensitive attribute lookup)
- `scale` as a float (`parse_number()`; default `0.0`)
- `xChannelSelector` / `yChannelSelector` (`R|G|B|A`, default `A`)

### `scale` units + axis-specific scaling

In `apply_primitive()` we resolve `scale` to **pixel displacements**:

1. Resolve the raw scalar in the current `primitiveUnits` coordinate system:
   - `base_scale = SvgFilter::resolve_primitive_x(scale, css_bbox)`
   - `primitiveUnits="userSpaceOnUse"`: `base_scale = scale`
   - `primitiveUnits="objectBoundingBox"`: `base_scale = scale * bbox.width`

   Note: this intentionally resolves object-bounding-box scalars against the bbox **width** (not an
   average dimension) to match Chrome/Skia (see the comment at the displacement-map site in
   `apply_primitive()`).
2. Convert to working pixels separately for x/y:
   - `scale_x_px = base_scale * scale_x`
   - `scale_y_px = base_scale * scale_y`

In practice, `filterRes` is ignored for graphs containing `feDisplacementMap`, so the common case is
`scale_x == scale_y == DPR` and thus `scale_x_px == scale_y_px`.

`scale` is the **full displacement range** (no extra `*2`):

- If the selected map channel is `1.0` and `scale=2`, then `dx = (1.0 - 0.5) * 2 = +1px`.
- If the selected map channel is `0.0` and `scale=2`, then `dx = (0.0 - 0.5) * 2 = -1px`.

Regression coverage:

- `displacement_map_object_bounding_box_scale_is_resolved_against_bbox_width`
  (`tests/paint/svg_filter_test.rs`)
- `displacement_map_applies_scale_without_extra_multiplier`
  (`tests/paint/svg_filter_test.rs`)

### Displacement math + sampling (Chrome behavior)

`apply_displacement_map()` applies a per-pixel kernel:

For each output pixel `(x, y)` (integer pixel indices):

1. **Sample the displacement map (`in2`) at `(x, y)` with clamping to the map’s primitive subregion.**
   - We clamp `x/y` into `map_region` (the `FilterResult::region` of the map input), then sample
     `map.pixel(map_x, map_y)`.
   - This models Chrome’s behavior: pixels outside the map’s primitive subregion behave like the
     nearest edge pixel, not transparent black (see the comment in `apply_displacement_map()`).
   - Sampling is **nearest neighbor** (no bilinear interpolation).
2. Convert that map pixel from premultiplied RGBA8 into **unpremultiplied floats** using
   `to_unpremultiplied()`.
3. Select channels and compute displacements:
   - `dx = (channel(map, xChannelSelector) - 0.5) * scale_x_px`
   - `dy = (channel(map, yChannelSelector) - 0.5) * scale_y_px`
4. Sample the primary input (`in1`) at `(x + dx, y + dy)` using `sample_nearest_premultiplied()`.
5. Store the sampled premultiplied pixel.

### Pixel coordinate convention (nearest pixel centers)

Primary sampling uses `sample_nearest_premultiplied()`:

- Out-of-bounds coordinates return transparent (`PremultipliedColorU8::TRANSPARENT`).
- Otherwise we round to the nearest pixel “center” with ties rounding toward negative infinity:

```
ix = ceil(x - 0.5)
iy = ceil(y - 0.5)
```

So:

- `x = 0.0` => `ix = 0` (reads pixel 0)
- `x = 0.5` => `ix = 0` (tie goes to 0)
- `x = 0.50001` => `ix = 1`

This tie-breaking is intentional to match Chrome/Skia semantics.

### Out-of-bounds behavior

- Sampling the **primary input** out-of-bounds yields transparent black.
- Sampling the **map input** never goes out-of-bounds in practice because coordinates are clamped to
  the map’s region and then to pixmap bounds.

### Output region expansion

After applying the displacement kernel, `apply_primitive()` conservatively expands the output region
based on the displacement magnitude:

- `margin_x = 0.5 * abs(scale_x_px)`
- `margin_y = 0.5 * abs(scale_y_px)`
- `region = inflate_rect_xy(primary.region, margin_x, margin_y)` (clipped to the primitive region)

This affects how later primitives decide which pixels are “valid” and is easy to regress when
touching region bookkeeping.

### `color-interpolation-filters` (sampling primitive semantics)

`feDisplacementMap` is a **sampling** primitive: it reads two inputs and interprets channel values.

When the step’s `color-interpolation-filters` is `linearRGB`, `apply_displacement_map()`:

- clones both input pixmaps
- re-encodes them from sRGB bytes to linearRGB values (`reencode_pixmap_to_linear_rgb()`)
- performs channel selection + displacement math in that space
- re-encodes the output back to sRGB bytes for storage (`reencode_pixmap_to_srgb()`)

When the step uses `sRGB`, no conversion is performed.

Regression coverage:

- `displacement_map_interprets_map_channels_as_unpremultiplied`
  (`tests/paint/svg_filter_test.rs`)
- `displacement_map_interprets_map_channels_in_color_interpolation_space`
  (`tests/paint/svg_filter_test.rs`)

### `filterRes` interaction

For Chrome parity, `filterRes` is ignored whenever the filter graph contains `feDisplacementMap`
(see `apply_svg_filter_with_cache()`).

Regression coverage:

- `displacement_map_ignores_filter_res` (`tests/paint/svg_filter_test.rs`)

## Regression coverage (tests + fixtures)

### `feTurbulence` unit tests

- `src/paint/svg_filter.rs` (parsing/CIF encoding):
  - `turbulence_base_frequency_parses_pair`
  - `turbulence_base_frequency_clamps_negative_to_zero`
  - `turbulence_negative_base_frequency_clamps_to_zero`
  - `turbulence_seed_defaults_to_zero_when_missing`
  - `turbulence_seed_preserves_negative_values`
  - `turbulence_seed_truncates_fractional_values`
  - `turbulence_encodes_channels_based_on_color_interpolation_filters`
- `src/paint/svg_filter/turbulence.rs` (determinism under rayon):
  - `turbulence_render_is_byte_identical_for_same_seed`
  - `turbulence_raster_is_deterministic_across_thread_counts`

### `feTurbulence` integration tests

- `tests/svg_filter_turbulence.rs`:
  - `turbulence_is_deterministic`
  - `turbulence_seed_changes_output`
  - `turbulence_stitches_edges`
  - `turbulence_output_is_rgba_noise`
  - `turbulence_generates_independent_rgb_channels`
  - `turbulence_userspace_translation_changes_pattern`
  - `turbulence_userspace_translation_changes_pattern_with_filter_res`
  - `turbulence_missing_basefrequency_defaults_to_zero` (compares against resvg)

### `feDisplacementMap` tests + fixtures

- `tests/paint/svg_filter_test.rs`:
  - `displacement_map_applies_scale_without_extra_multiplier`
  - `displacement_map_interprets_map_channels_as_unpremultiplied`
  - `displacement_map_object_bounding_box_scale_is_resolved_against_bbox_width`
  - `displacement_map_ignores_filter_res`
  - `displacement_map_interprets_map_channels_in_color_interpolation_space`
- Displacement-map semantics golden (validated against Chrome):
  - Fixture: `tests/fixtures/html/svg_filter_displacement_map_semantics.html`
  - Golden test: `tests/paint/svg_filter_displacement_map_semantics_golden.rs`
  - Golden image: `tests/fixtures/golden/svg_filter_displacement_map_semantics.png`
- CIF golden fixture (includes `feDisplacementMap` under both CIF modes):
  - Fixture: `tests/fixtures/html/svg_filter_color_interpolation_filters.html`
    (`filter id="cif-displacement-*"`).
  - Golden test: `tests/paint/svg_filter_color_interpolation_golden.rs`
  - Golden image: `tests/fixtures/golden/svg_filter_color_interpolation_filters.png`

### Additional repro inputs

- Fuzz corpus sample using both primitives:
  - `tests/fuzz_corpus/svg_filters.svg` (`filter id="wavy"`)
- Real-world pageset fixture using `feTurbulence` with `stitchTiles="stitch"`:
  - `tests/pages/fixtures/foxnews.com/assets/795bf940a1c9a2a4e880a25b9b697ad7.svg`
