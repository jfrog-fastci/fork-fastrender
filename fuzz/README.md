# Fuzzing FastRender

This directory hosts libFuzzer targets driven by `cargo-fuzz`. Targets focus on
CSS parsing, selector parsing/matching, SVG filter execution, and custom
property/animation resolution.

## Setup

```
bash scripts/cargo_agent.sh install cargo-fuzz
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
- `text_shaping`: Exercises the text shaping pipeline (bidi analysis, script
  itemization, font fallback, HarfBuzz shaping) with randomized font-related
  `ComputedStyle` inputs. Uses bundled fonts only for determinism.
- `text_line_break`: Exercises Unicode line breaking (UAX#14), hyphenation, and
  justification routines on bounded UTF-8 text.
- `render_pipeline`: Runs the full HTML+CSS → pixels pipeline (DOM parse →
  cascade → box tree → layout → paint) under strict timeouts with network
  fetching disabled.
- `accessibility_tree`: Generates an accessibility tree for HTML (DOM parse →
  cascade → ARIA/name/state traversal) under strict timeouts with network
  fetching disabled.
- `html_scanners`: Feeds random HTML into lightweight string-based scanners
  (template stripping, client redirect inference, and HTML asset discovery).
- `image_decoding`: Feeds arbitrary bytes into the image probing + decode
  pipeline with tight decode limits (dimensions/pixels) to prevent OOM.
- `ipc_decode`: Feeds arbitrary bytes into IPC message deserialization using the
  production bincode options + size limits (panic-free, bounded allocations).

## Running

Quick smoke runs:

```
# `cargo-fuzz` defaults to AddressSanitizer, which reserves a very large virtual address space for
# shadow memory. `scripts/cargo_agent.sh` automatically bumps RLIMIT_AS for `fuzz` subcommands; set
# `FASTR_FUZZ_LIMIT_AS` / `FASTR_CARGO_LIMIT_AS` if you need to override it.

bash scripts/cargo_agent.sh fuzz run css_parser -- -runs=1000
bash scripts/cargo_agent.sh fuzz run selectors fuzz/corpus/selectors tests/fuzz_corpus -- -max_total_time=10
bash scripts/cargo_agent.sh fuzz run render_pipeline fuzz/corpus/render_pipeline tests/fuzz_corpus -- -runs=1000
bash scripts/cargo_agent.sh fuzz run text_shaping -- -runs=1000
bash scripts/cargo_agent.sh fuzz run text_line_break -- -runs=1000
bash scripts/cargo_agent.sh fuzz run accessibility_tree fuzz/corpus/accessibility_tree tests/fuzz_corpus -- -runs=1000
bash scripts/cargo_agent.sh fuzz run html_scanners -- -runs=1000
bash scripts/cargo_agent.sh fuzz run image_decoding -- -runs=1000
bash scripts/cargo_agent.sh fuzz run ipc_decode -- -runs=1000
```

You can point any target at additional corpora (e.g. `tests/fuzz_corpus/` which
contains curated real-world CSS animation/filter samples) to improve coverage.

## Corpus replay smoke test (no cargo-fuzz)

The checked-in corpora under `tests/fuzz_corpus/` are also replayed in the normal
Rust test suite. This provides lightweight, deterministic coverage in CI without
requiring `cargo-fuzz` (the goal is termination + no panics under strict
timeouts/offline resource policies, not pixel-perfect output comparison).

Run it scoped:

```
bash scripts/cargo_agent.sh test -p fastrender --test integration tooling::fuzz_corpus_smoke::fuzz_corpus_smoke_test
```

Debug builds skip the heaviest corpus cases (currently `render_pipeline_stress.html`)
unless you opt in:

```
FUZZ_CORPUS_SMOKE_IN_DEBUG=1 bash scripts/cargo_agent.sh test -p fastrender --test integration tooling::fuzz_corpus_smoke::fuzz_corpus_smoke_test
```

Note: corpora live under `fuzz/corpus/<target>/` once you start fuzzing; these
directories are intentionally not checked in.
