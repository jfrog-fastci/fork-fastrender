# Form control rendering

FastRender treats native form controls as replaced elements so they participate in intrinsic sizing and paint their own UA appearance.

## How it works

- `<input>`, `<select>`, `<textarea>`, and `<button>` generate `ReplacedType::FormControl` boxes (except `<input type="hidden">`).
- Intrinsic sizing for form controls is handled in the replaced-element intrinsic sizing code and respects HTML defaults (e.g. ~`20ch` text inputs / `cols`+`rows` for `<textarea>`). It scales with the current font metrics so controls line up with surrounding text by default.
- The painters draw a simplified UA-like control surface (value/placeholder text + a small set of affordances) inside the element’s content box.
- `appearance: none` affects **native painting** (suppresses some UA chrome) but does **not** currently change box generation: the element is still a `ReplacedType::FormControl` and keeps form-control intrinsic sizing.
- `-webkit-appearance` is intended to be treated as an alias of `appearance` once Task 94 lands (today it is only recognized for `@supports` feature queries).

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

The pageset regression suite includes form-heavy fixtures under `tests/pages/fixtures/form_controls*`
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
