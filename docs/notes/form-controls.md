# Form control rendering

FastRender treats native form controls as replaced elements so they participate in intrinsic sizing and paint their own UA appearance.

Note: FastRender does not delegate to platform-native widgets; “native painting” here refers to FastRender’s built-in form-control renderer.

## Specs (background)

- HTML Living Standard: form controls (`<input>`, `<select>`, `<textarea>`, `<button>`) (`specs/whatwg-html/source`)
- CSS UI 4: `appearance` and the “fallback rendering model” for controls (`specs/csswg-drafts/css-ui-4/`)

## How it works

- `<input>`, `<select>`, and `<textarea>` generate `ReplacedType::FormControl` boxes (except `<input type="hidden">` and the usual suppression rules like `display: none`) **unless** the computed `appearance` is `none`.
- `<button>` is treated as a normal element (not a replaced form control) so its descendants participate in authored layout (e.g. inline-flex icon + label content).
- When computed `appearance` is `none` (including vendor aliases like `-webkit-appearance:none` / `-moz-appearance:none`), the element is **not** replaced. Instead it generates a normal element box that respects `display`, DOM children, and `::before`/`::after`.
- For controls that normally have no DOM children, FastRender synthesizes a small internal structure in `appearance:none` mode so authors can still style content:
  - Text-like `<input>` and `<textarea>`: value/placeholder is emitted as an inline text box. When placeholder text is shown, it uses a generated `::placeholder` box style.
  - `<input type=range>`: emits track/thumb pseudo-element boxes (`::-webkit-slider-runnable-track` / `::-webkit-slider-thumb`, normalized from vendor spellings) so custom sliders can be authored with normal layout/paint.
  - `<input type=file>`: emits a `::file-selector-button` box plus the selected file label text.
  - `<select>`: emits a text node with the selected option label (or `"Select"` when empty).
- Intrinsic sizing for form controls is handled in the replaced-element intrinsic sizing code and respects HTML defaults (e.g. ~`20ch` text inputs / `cols`+`rows` for `<textarea>`). It scales with the current font metrics so controls line up with surrounding text by default.
- Control kinds:
  - Text-like inputs cover `text/search/url/tel/email` (plus empty/missing `type`), password masking, `number` inputs (spinner affordance), and date-like inputs (`date`/`datetime-local`/`month`/`week`/`time`) with simple glyphs and default format placeholders.
  - Unknown `<input type=...>` falls back to `Unknown` and uses placeholder/value/type text as the label.
  - Checkboxes/radios draw marks when checked/indeterminate; selects render either a collapsed dropdown (label + caret) or a listbox when `multiple`/`size` > 1; ranges draw a track + thumb; color inputs render a swatch plus a hex label.
- Disabled, inert, focus, focus-visible, required, and invalid states are derived from element attributes + `data-fastr-*` hints during box generation and influence native painting (tinted overlays, accent changes, and caret painting for focused text controls). The `data-fastr-focus-visible` hint implies focus for native painting so standalone focus-visible markers are captured; `inert`/`data-fastr-inert=true` suppresses focus markers.
- `:user-valid` / `:user-invalid` are gated by HTML “user validity”, which only flips after user interaction. Since FastRender is static, you can opt in deterministically with `data-fastr-user-validity="true"` on the control itself or its form owner (to simulate a submission attempt).
- Some form-control pseudo-element styles are captured during the cascade (placeholder + range slider thumb/track + `::file-selector-button`). Vendor spellings and legacy single-colon forms (e.g. `::-webkit-input-placeholder`/`:-webkit-input-placeholder`, `::-moz-range-thumb`/`:-moz-range-thumb`, `::-ms-track`/`:-ms-track`, `::-webkit-file-upload-button`) are accepted and normalized internally.
- These pseudo styles are used in two places:
  - Native painters (default appearance) consume them via `FormControl::{placeholder_style, slider_thumb_style, slider_track_style, file_selector_button_style}`.
  - In `appearance:none` fallback mode they are applied to generated pseudo-element boxes in the box tree.
- The `:placeholder-shown` state pseudo-class is supported for `<input>`/`<textarea>` (including Firefox’s legacy `:-moz-placeholder-shown` alias). Real-world CSS sometimes uses legacy vendor placeholder selectors inside `:not()` (e.g. `:not(:-moz-placeholder)`, `:not(:-ms-input-placeholder)`) as fallbacks for `:placeholder-shown`; FastRender accepts these forms so selector lists don’t get invalidated.
- `appearance: none` switches the element to the CSS UI 4 “fallback rendering model”: it disables native control replacement and makes the element behave like a normal CSS box with children/pseudo-elements.
- Vendor-prefixed `-webkit-appearance` and other vendor-prefixed spellings (`-moz-appearance`, `-ms-appearance`, `-o-appearance`) are treated as aliases of `appearance` (for site compatibility), so those spellings can drive `Appearance::None` / keyword values through box generation and painting. (Task 94 tracks vendor-alias conformance; note: `@supports` intentionally only reports support for a small allowlist like `-webkit-appearance` to avoid inverting feature queries.)

## Key code paths

- Form control model: `src/tree/box_tree.rs::FormControl` (+ `FormControlKind`, `TextControlKind`)
- Box generation: `src/tree/box_generation.rs::create_form_control_replaced`
- Intrinsic sizing: `src/api.rs::resolve_intrinsic_for_replaced_for_media`
- Form-control pseudo-element styles (`::placeholder`, range slider thumb/track, `::file-selector-button`): `src/style/cascade.rs::compute_form_control_pseudo_styles`
  - Pseudo-element parsing/matching: `src/css/selectors.rs::PseudoElement::{Placeholder, SliderThumb, SliderTrack, FileSelectorButton}`
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

## `appearance:none` fallback rendering model

When `appearance:none` is computed, the control stops using the native form-control painters entirely (because it is no longer a replaced element). This enables:

- `::before`/`::after` (and any DOM children where applicable) to participate normally.
- Custom range/file controls authored through pseudo-element boxes (`::-webkit-slider-thumb`, `::-webkit-slider-runnable-track`, `::file-selector-button`).
- Placeholder/value text to be styled and laid out using normal CSS text/layout rules.

## Regression coverage

- Box-generation unit tests:
  - `src/tree/box_generation.rs::appearance_none_form_controls_generate_fallback_children`
  - `src/tree/box_generation.rs::appearance_none_disables_form_control_replacement_and_generates_placeholder_text`
  - `src/tree/box_generation.rs::webkit_appearance_none_disables_form_control_replacement`
  - `src/tree/box_generation.rs::moz_appearance_none_disables_form_control_replacement`
  - `tests/tree/form_controls_appearance_none_fallback.rs`
- Paint integration tests:
  - `tests/paint/form_control_appearance_none_affordances.rs` asserts `appearance:none` suppresses number/date affordance glyphs.
  - `tests/form_control_placeholder_opacity.rs` asserts `::placeholder` opacity is applied (both paint backends).
  - `tests/paint/range_track_pseudo_element.rs` asserts the range track pseudo-element paints under `appearance:none` (both paint backends).
  - `tests/paint/range_pseudo_opacity.rs` asserts range track/thumb pseudo-element `opacity` is applied (both paint backends).
  - `tests/paint/file_selector_button_pseudo_element.rs` asserts `::file-selector-button` paints under `appearance:none` (both paint backends).
- Offline page fixtures:
  - `tests/pages/fixtures/form_controls_appearance` includes `appearance:none` custom controls (including vendor slider pseudos like `::-webkit-slider-thumb` / `::-moz-range-thumb`).

The offline page regression suite includes form-heavy fixtures under `tests/pages/fixtures/form_controls*`
so we can catch large visual diffs caused by missing UA form control styling/painting. Regenerate
their goldens with:

```
UPDATE_PAGES_GOLDEN=1 \
  PAGES_FIXTURE_FILTER=form_controls,form_controls_appearance,form_controls_placeholder,form_controls_placeholder_pseudo,form_controls_range_select,form_controls_showcase,form_controls_states,form_controls_custom_vs_default,form_controls_comparison_panel,form_controls_lab \
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
