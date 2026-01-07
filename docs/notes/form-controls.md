# Form control rendering

FastRender treats native form controls as replaced elements so they participate in intrinsic sizing and paint their own UA appearance.

## How it works

- `<input>`, `<select>`, `<textarea>`, and `<button>` generate `ReplacedType::FormControl` boxes (except `<input type="hidden">`).
- Intrinsic sizing for form controls is handled in the replaced-element intrinsic sizing code and respects HTML defaults (e.g. ~`20ch` text inputs / `cols`+`rows` for `<textarea>`). It scales with the current font metrics so controls line up with surrounding text by default.
- Control kinds:
  - Text-like inputs cover `text/search/url/tel/email` (plus empty/missing `type`), password masking, `number` inputs (spinner affordance), and date-like inputs (`date`/`datetime-local`/`month`/`week`/`time`) with simple glyphs and default format placeholders.
  - Unknown `<input type=...>` falls back to `Unknown` and uses placeholder/value/type text as the label.
  - Checkboxes/radios draw marks when checked/indeterminate; selects render a text value plus a caret; ranges draw a track + thumb; color inputs render a swatch plus a hex label.
- Disabled, focus, focus-visible, required, and invalid states are derived from element attributes + `data-fastr-focus*` hints during box generation and influence native painting (tinted overlays, accent changes). The `data-fastr-focus-visible` hint implies focus for native painting so standalone focus-visible markers are captured.
- `appearance: none` affects **native painting** (suppresses some UA chrome) but does **not** currently change box generation: the element is still a `ReplacedType::FormControl` and keeps form-control intrinsic sizing. (Non-`none` keywords are preserved as `Appearance::Keyword(...)`, but painters currently only special-case `Appearance::None`.)
- `-webkit-appearance` is parsed/accepted today (so it can participate in `@supports`), but it does not currently affect computed styles; only the unprefixed `appearance` property is applied. Task 94 intends to treat `-webkit-appearance` as an alias of `appearance`.

## Key code paths

- Box generation: `src/tree/box_generation.rs::create_form_control_replaced`
- Intrinsic sizing: `src/api.rs::resolve_intrinsic_for_replaced_for_media`
- Painting:
  - Display list: `src/paint/display_list_builder.rs::emit_form_control`
  - Immediate painter: `src/paint/painter.rs::paint_form_control`
- UA defaults: `src/user_agent.css` (embedded in the cascade; if Task 127 moves UA defaults into `ua_default_rules`, update this pointer)

## What `appearance:none` enables today

- Author `background`/`border`/`padding` styling applies normally (the element is still a normal CSS box; only the *inside* is painted by the form-control code).
- Native chrome suppression is currently selective and implemented directly in the painters:
  - Select caret (“▾”) is skipped when `control.appearance == Appearance::None` (`emit_form_control` / `paint_form_control`).
  - Checkbox/radio marks are skipped when `control.appearance == Appearance::None` (`emit_form_control` / `paint_form_control`).
- Current limitations:
  - `appearance:none` does **not** yet disable all affordances (e.g. number/date glyphs and the range track/thumb are still painted today).
  - Vendor pseudo-elements like `::-webkit-slider-thumb`, `::-webkit-slider-runnable-track`, `::-moz-range-thumb`, etc. are not implemented yet, so fully custom range styling isn’t available.

The offline page regression suite includes form-heavy fixtures under `tests/pages/fixtures/form_controls*`
so we can catch large visual diffs caused by missing UA form control styling/painting. Regenerate
their goldens with:

```
UPDATE_PAGES_GOLDEN=1 \
  PAGES_FIXTURE_FILTER=form_controls,form_controls_appearance,form_controls_range_select,form_controls_showcase,form_controls_states,form_controls_custom_vs_default,form_controls_comparison_panel,form_controls_lab \
  cargo test pages_regression
```

The reference fixture at `tests/ref/fixtures/form_controls` exercises common control types and
states (including size/rows/cols hints, invalid and disabled colors, unknown types, date/time
variants, and focus-visible highlights). Regenerate the reference golden with:

```
UPDATE_GOLDEN=1 cargo test form_controls_reference_image_matches_golden
```
