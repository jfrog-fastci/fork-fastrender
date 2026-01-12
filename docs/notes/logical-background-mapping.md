# Logical background-position/size mapping

Logical `background-position-*` and `background-size-*` longhands map to physical axes per `writing-mode`, including sideways modes.

## Where this is implemented

- Parsing and mapping: `src/style/properties.rs` (`background-position-inline/block`, `background-size-inline/block`)
- Defaults: `BackgroundLayer` in `src/style/types.rs`

## Regression coverage

- Tests: `background_position_logical_*` (currently in `tests/style/background_position_logical_test.rs`; may migrate into `src/style/` as unit tests).
  - Covers inline/block position/size for `horizontal-tb`, `vertical-rl`, `sideways-lr`, `sideways-rl`
  - Also covers `background-position-x/y` longhands
  - `background-size-y` is deprecated in specs, but is still parsed/mapped and tested here

## Verification

- If tests have been migrated into `src/`: `bash scripts/cargo_agent.sh test -p fastrender --lib background_position_logical`
- If tests are still integration tests: `bash scripts/cargo_agent.sh test -p fastrender --test integration background_position_logical`
