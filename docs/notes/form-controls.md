# Form control rendering

FastRender treats native form controls as replaced elements so they participate in intrinsic sizing and paint their own UA appearance.

Note: FastRender does not delegate to platform-native widgets; “native painting” here refers to FastRender’s built-in form-control renderer.

## Specs (background)

- HTML Living Standard: form controls (`<input>`, `<select>`, `<textarea>`, `<button>`) (`specs/whatwg-html/source`)
- CSS UI 4: `appearance` and the “fallback rendering model” for controls (`specs/csswg-drafts/css-ui-4/`)

## How it works

- `<input>`, `<select>`, `<textarea>`, and `<button>` generate `ReplacedType::FormControl` boxes (except `<input type="hidden">` and the usual suppression rules like `display: none`).
- Intrinsic sizing for form controls is handled in the replaced-element intrinsic sizing code and respects HTML defaults (e.g. ~`20ch` text inputs / `cols`+`rows` for `<textarea>`). It scales with the current font metrics so controls line up with surrounding text by default.
- Control kinds:
  - Text-like inputs cover `text/search/url/tel/email` (plus empty/missing `type`), password masking, `number` inputs (spinner affordance), and date-like inputs (`date`/`datetime-local`/`month`/`week`/`time`) with simple glyphs and default format placeholders.
  - Unknown `<input type=...>` falls back to `Unknown` and uses placeholder/value/type text as the label.
  - Checkboxes/radios draw marks when checked/indeterminate; selects render either a collapsed dropdown (label + caret) or a listbox when `multiple`/`size` > 1; ranges draw a track + thumb; color inputs render a swatch plus a hex label.
- Disabled, inert, focus, focus-visible, required, and invalid states are derived from element attributes + `data-fastr-*` hints during box generation and influence native painting (tinted overlays, accent changes, and caret painting for focused text controls). The `data-fastr-focus-visible` hint implies focus for native painting so standalone focus-visible markers are captured; `inert`/`data-fastr-inert=true` suppresses focus markers.
- Some form-control pseudo-element styles are captured during the cascade (placeholder + range slider thumb/track) and passed into the painters via `FormControl::{placeholder_style, slider_thumb_style, slider_track_style}`. Vendor spellings and legacy single-colon forms (e.g. `::-webkit-input-placeholder`/`:-webkit-input-placeholder`, `::-moz-range-thumb`/`:-moz-range-thumb`, `::-ms-track`/`:-ms-track`) are accepted and normalized internally.
- The `:placeholder-shown` state pseudo-class is supported for `<input>`/`<textarea>` (including Firefox’s legacy `:-moz-placeholder-shown` alias). Real-world CSS sometimes uses legacy vendor placeholder selectors inside `:not()` (e.g. `:not(:-moz-placeholder)`, `:not(:-ms-input-placeholder)`) as fallbacks for `:placeholder-shown`; FastRender accepts these forms so selector lists don’t get invalidated.
- `appearance: none` affects **native painting** (suppresses some UA chrome) but does **not** currently change box generation: the element is still a `ReplacedType::FormControl` and keeps form-control intrinsic sizing. (Non-`none` keywords are preserved as `Appearance::Keyword(...)`, but painters currently only special-case `Appearance::None`.)
- Vendor-prefixed `-webkit-appearance` and other vendor-prefixed spellings (`-moz-appearance`, `-ms-appearance`, `-o-appearance`) are treated as aliases of `appearance` (for site compatibility), so those spellings can drive `Appearance::None` / keyword values through box generation and painting. (Task 94 tracks vendor-alias conformance; note: `@supports` intentionally only reports support for a small allowlist like `-webkit-appearance` to avoid inverting feature queries.)

## Key code paths

- Form control model: `src/tree/box_tree.rs::FormControl` (+ `FormControlKind`, `TextControlKind`)
- Box generation: `src/tree/box_generation.rs::create_form_control_replaced`
- Intrinsic sizing: `src/api.rs::resolve_intrinsic_for_replaced_for_media`
- Form-control pseudo-element styles (`::placeholder` and range slider thumb/track pseudo-elements): `src/style/cascade.rs::compute_form_control_pseudo_styles`
  - Pseudo-element parsing/matching: `src/css/selectors.rs::PseudoElement::{Placeholder, SliderThumb, SliderTrack}`
- Vendor aliasing:
  - `-webkit-appearance` → `appearance` during style application: `src/style/properties.rs`
  - `-moz-appearance` (and other vendor prefixes) canonicalized during CSS parsing when possible:
    - `src/css/parser.rs::lookup_known_property`
    - `src/css/properties.rs::vendor_prefixed_property_alias`
  - `@supports` handling for vendor properties (intentionally conservative to avoid inverting feature queries):
    - `src/css/supports.rs::supports_declaration` (tests: `tests/css_supports_vendor_properties.rs`)
- Painting:
  - Display list: `src/paint/display_list_builder.rs::emit_form_control`
  - Immediate painter: `src/paint/painter.rs::paint_form_control`
- UA defaults:
  - Baseline UA stylesheet: `src/user_agent.css` (loaded in `src/style/cascade.rs`)
  - Additional dynamic UA defaults (e.g. link state overrides): `src/style/cascade.rs::ua_default_rules`

## What `appearance:none` enables today

- Author `background`/`border`/`padding` styling applies normally (the element is still a normal CSS box; only the *inside* is painted by the form-control code). This applies whether you set `appearance:none` or common vendor spellings like `-webkit-appearance:none` / `-moz-appearance:none` / `-ms-appearance:none`.
- Native chrome suppression is currently selective and implemented directly in the painters:
  - Select caret (“▾”) is skipped when `control.appearance == Appearance::None`:
    - `src/paint/display_list_builder.rs::emit_form_control` (`FormControlKind::Select`)
    - `src/paint/painter.rs::paint_form_control` (`FormControlKind::Select`)
  - Checkbox/radio marks are skipped when `control.appearance == Appearance::None`:
    - `src/paint/display_list_builder.rs::emit_form_control` (`FormControlKind::Checkbox`)
    - `src/paint/painter.rs::paint_form_control` (`FormControlKind::Checkbox`)
  - Text-control affordances are skipped when `control.appearance == Appearance::None`:
    - Number spinners (▲/▼) are only painted when `appearance != none` (`TextControlKind::Number`)
    - Date-like dropdown caret (▾) is only painted when `appearance != none` (`TextControlKind::Date`)
    - See `src/paint/display_list_builder.rs::emit_form_control` and `src/paint/painter.rs::paint_form_control`
      (`FormControlKind::Text`)
- Range controls treat `appearance:none` as “custom range” mode:
  - UA accent fill painting is skipped when `control.appearance == Appearance::None`, but author
    track pseudo-element styling can still paint a custom track (over the element background).
  - The thumb is still painted; in `Appearance::None` mode it can be styled via
    `slider_thumb_style` (captured from `::-webkit-slider-thumb` and vendor/legacy aliases like
    `::-moz-range-thumb`/`:-moz-range-thumb`/`::-ms-thumb`).
  - `slider_track_style` is captured (e.g. `::-webkit-slider-runnable-track`, `::-moz-range-track`,
    `:-ms-track`) and is used by the painters to draw the track when present (including under
    `appearance:none`), but the accent fill segment is only painted when `appearance != none`.
  - See `src/paint/display_list_builder.rs::emit_form_control` and
    `src/paint/painter.rs::paint_form_control` (`FormControlKind::Range`).
- Task 80 tracks further broadening of the suppressed affordance set for `appearance:none` (beyond the current select/checkbox/range/number/date hooks).
- Current limitations:
  - `appearance:none` does **not** turn the element into a normal container: the control is still a `ReplacedType::FormControl`, so its DOM children are not laid out (e.g. `<button><svg>…</svg>Label</button>` collapses to a plain text label).
  - `appearance:none` does **not** yet disable all affordances (e.g. `<input type=color>` still paints a swatch + hex label; `FormControlKind::Color` does not currently special-case `Appearance::None`).
- Range pseudo-element selectors are normalized internally (WebKit/Mozilla/MS spellings are accepted), but painters still only consume a subset of the style hooks:
  - `slider_thumb_style` is used by both painters to style the thumb (including in `appearance:none` mode for custom sliders).
  - `slider_track_style` is used by both painters to style the track; in `appearance:none` mode the
    track paints but the UA accent fill segment is suppressed.

## Intended direction (fallback rendering model)

The CSS UI “fallback rendering model” for form controls is that `appearance:none` should allow author styling to fully take over, including the ability to build controls out of normal box-tree/pseudo-element mechanics.

FastRender does **not** implement that model yet (see limitations above). Implementing it would likely involve:

- Changing box generation so `appearance:none` (and vendor aliases like `-webkit-appearance:none`) no longer forces `ReplacedType::FormControl`, allowing children/pseudo-elements to lay out normally.
- Exposing vendor pseudo-elements used for custom controls as real pseudo-element boxes and/or broadening the existing style-capture/paint hooks (today we capture placeholder + range slider thumb/track styles, but painters still only consume a subset of those hooks).

This work is tracked in the capability map under `alg.forms.appearance-none`.

## Regression coverage

- Box-generation unit tests:
  - `src/tree/box_generation.rs::appearance_none_form_controls_still_generate_replaced_boxes`
  - `src/tree/box_generation.rs::appearance_none_does_not_disable_form_control_replacement`
  - `src/tree/box_generation.rs::webkit_appearance_none_propagates_to_form_control`
  - `src/tree/box_generation.rs::moz_appearance_none_propagates_to_form_control`
- Paint integration tests:
  - `tests/paint/form_control_appearance_none_affordances.rs` asserts `appearance:none` suppresses number/date affordance glyphs.
  - `tests/form_control_placeholder_opacity.rs` asserts `::placeholder` opacity is applied (both paint backends).
  - `tests/paint/range_track_pseudo_element.rs` asserts the range track pseudo-element paints under `appearance:none` (both paint backends).
- Offline page fixtures:
  - `tests/pages/fixtures/form_controls_appearance` includes `appearance:none` custom controls (including vendor slider pseudos like `::-webkit-slider-thumb` / `::-moz-range-thumb`).

The offline page regression suite includes form-heavy fixtures under `tests/pages/fixtures/form_controls*`
so we can catch large visual diffs caused by missing UA form control styling/painting. Regenerate
their goldens with:

```
UPDATE_PAGES_GOLDEN=1 \
  PAGES_FIXTURE_FILTER=form_controls,form_controls_appearance,form_controls_placeholder,form_controls_range_select,form_controls_showcase,form_controls_states,form_controls_custom_vs_default,form_controls_comparison_panel,form_controls_lab \
  cargo test pages_regression
```

Note: `PAGES_FIXTURE_FILTER` expects a comma-separated list of **exact** fixture names (it is not a prefix/regex match).

Tip: to refresh a single page fixture, you can also use `PAGES_FIXTURE=<name>` instead of `PAGES_FIXTURE_FILTER`.

The reference fixture at `tests/ref/fixtures/form_controls` exercises common control types and
states (including size/rows/cols hints, invalid and disabled colors, unknown types, date/time
variants, and focus-visible highlights). Regenerate the reference golden with:

```
UPDATE_GOLDEN=1 \
  cargo test form_controls_reference_image_matches_golden
```
