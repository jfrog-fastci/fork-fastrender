# Variable font decoration metrics

- `ttf-parser` 0.25 marks underline/strikeout metrics as variation-aware; calling
  `Face::set_variation` applies MVAR deltas before reading `underline_metrics` /
  `strikeout_metrics`.
- Decoration metric extraction now forwards run variation coordinates when present,
  and the fallback path derives the same synthesized variation set from style data
  before querying font metrics.
- Skip-ink decoration carving now queries per-glyph bounds via `FontInstance::glyph_bounds`
  (skrifa-backed) instead of `ttf-parser`'s `glyph_bounding_box`, and keys the cache with the
  instance variation hash. This keeps underline gaps deterministic and variation-aware for variable
  fonts.
- Tests include the Roboto Flex variable font fixture (`tests/fonts/RobotoFlex-VF.ttf`,
  OFL-1.1) to exercise variation handling. Its MVAR table adjusts cap height/caret/x-height
  but not decoration metrics, so the assertions cover determinism rather than a visual delta;
  replace the fixture with one that varies underline/strike metrics to strengthen coverage when
  available.
