# Logical background-position/size mapping

Logical `background-position-*` and `background-size-*` longhands map to physical axes per `writing-mode`, including sideways modes.

## Where this is implemented

- Parsing and mapping: `src/style/properties.rs` (`background-position-inline/block`, `background-size-inline/block`)
- Defaults: `BackgroundLayer` in `src/style/types.rs`

## Regression coverage

- Tests: `background_position_logical_*`
  - Preferred (unit tests under `src/style`): `bash scripts/cargo_agent.sh test -p fastrender --lib background_position_logical`
  - While the migration is in progress, the same test name may also be runnable via: `bash scripts/cargo_agent.sh test -p fastrender --test integration background_position_logical`
  - Covers inline/block position/size for `horizontal-tb`, `vertical-rl`, `sideways-lr`, `sideways-rl`
  - Also covers `background-position-x/y` longhands
  - `background-size-y` is deprecated in specs, but is still parsed/mapped and tested here
