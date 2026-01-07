# Fuzzing FastRender

This directory hosts libFuzzer targets driven by `cargo-fuzz`. Targets focus on
CSS parsing, selector parsing/matching, SVG filter execution, and custom
property/animation resolution.

## Setup

```
cargo install cargo-fuzz
```

`cargo-fuzz` will automatically use a nightly toolchain for the fuzz build.

## Targets

- `css_parser`: Feeds random bytes/unicode into stylesheet and declaration
  parsing.
- `selectors`: Parses selectors and matches them against randomized DOM trees.
- `vars_and_calc`: Exercises custom property resolution and calc parsing.
- `svg_filters`: Generates small SVGs with `<filter>` graphs and runs them
  through the filter parser/executor.
- `animation_properties`: Builds CSS animation/transition/keyframe snippets and
  samples them against a styled DOM tree.
- `color_fonts`: Builds fonts from arbitrary bytes and exercises color glyph
  rendering (bitmaps, SVG-in-OT, COLR).
- `render_pipeline`: Runs the full HTML+CSS → pixels pipeline (DOM parse →
  cascade → box tree → layout → paint) under strict timeouts with network
  fetching disabled.
- `html_scanners`: Feeds random HTML into lightweight string-based scanners
  (template stripping, client redirect inference, and HTML asset discovery).

## Running

Quick smoke runs:

```
cargo fuzz run css_parser -- -runs=1000
cargo fuzz run selectors tests/fuzz_corpus -- -max_total_time=10
cargo fuzz run render_pipeline tests/fuzz_corpus -- -runs=1000
cargo fuzz run html_scanners -- -runs=1000
```

You can point any target at additional corpora (e.g. `tests/fuzz_corpus/` which
contains curated real-world CSS animation/filter samples) to improve coverage.

Note: corpora live under `fuzz/corpus/<target>/` once you start fuzzing; these
directories are intentionally not checked in.
