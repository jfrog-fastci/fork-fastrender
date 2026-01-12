# Logical background-position/size mapping

Logical `background-position-*` and `background-size-*` longhands map to physical axes per `writing-mode`, including sideways modes.

## Where this is implemented

- Parsing and mapping: `src/style/properties.rs` (`background-position-inline/block`, `background-size-inline/block`)
- Defaults: `BackgroundLayer` in `src/style/types.rs`

## Regression coverage

- Integration tests: `tests/style/background_position_logical_test.rs` (run via `--test style_tests`)
  - Covers inline/block position/size for `horizontal-tb`, `vertical-rl`, `sideways-lr`, `sideways-rl`
  - Also covers `background-position-x/y` longhands
  - `background-size-y` is deprecated in specs, but is still parsed/mapped and tested here

## Verification

- `bash scripts/cargo_agent.sh test -p fastrender --test style_tests background_position_logical`
