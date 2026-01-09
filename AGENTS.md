# FastRender (agent instructions)

This file contains **repo-wide** rules shared by all workstreams.

## Workstreams

Pick one workstream and follow its specific doc:

- **Capability buildout (spec-first primitives)**: `instructions/capability_buildout.md`
- **Pageset page loop (fix pages one-by-one)**: `instructions/pageset_page_loop.md`
- **Browser UI / chrome (tabs, address bar, inputs)**: `instructions/browser_ui.md`
- **JavaScript support (full JS + web APIs)**: `instructions/javascript_support.md`

Supporting docs:

- `ecma-rs` submodule workflow: `instructions/ecma_rs.md`

## Non-negotiables

- **No page-specific hacks** (no hostname/selector special-cases, no magic numbers for one site).
- **No deviating-spec behavior** as a “compat shortcut”. Implement the spec behavior the page depends on (incomplete is OK; wrong is not).
- **No post-layout pixel nudging**; keep the pipeline staged (parse → style → box tree → layout → paint).
- **No panics** in production code. Return errors cleanly and bound work.
- **Keep Taffy vendored** (`vendor/taffy/`) and only use it for flex/grid; do not update it via Cargo.
- **JavaScript execution must be bounded**: the JS engine must support interrupts/timeouts and avoid unbounded host allocations.

## What counts

A change counts if it lands at least one of:

- **New capability** (feature/algorithm implemented) with a regression.
- **Bugfix** (behavior corrected) with a regression.
- **Stability** (crash/panic eliminated) with a regression.
- **Termination** (timeout/loop eliminated) with a regression.
- **Conformance** (meaningful new WPT/fixture coverage).

## System resources (RAM / time / disk) — mandatory safety

FastRender runs on hostile inputs. Any run can go pathological. **Always enforce RAM ceilings** so one bad case can’t freeze the host.

### Running renderer binaries (always cap memory)

For anything that executes the renderer (pages, fixtures, benches, fuzz, etc.), run under OS caps:

```bash
scripts/run_limited.sh --as 64G -- cargo run --release --bin fetch_and_render -- <args...>
```

Prefer in-process guardrails when a CLI supports them (e.g. `--mem-limit-mb`, stage budgets), but **OS caps are still required**.

See: `docs/resource-limits.md`

### Cargo builds/tests — NEVER run unscoped

**This is critical. Violating these rules spawns hundreds of rustc/linker processes, exhausts RAM, and kills the host.**

**FORBIDDEN (no exceptions):**
- `cargo build` / `cargo test` / `cargo check` without wrapper scripts
- `cargo test` without `-p <crate>`, `--test <name>`, or `--lib`
- `cargo test --all-features` or `cargo check --all-features --tests`
- `cargo build --all-targets` or `cargo test --all-targets`
- ANY command that compiles all 100+ targets

**MANDATORY:**
- **Always use `scripts/cargo_agent.sh`** for all cargo commands
- **Always scope test runs**: `-p <crate>`, `--test <name>`, `--lib`, or `--bin <name>`

```bash
# CORRECT:
scripts/cargo_agent.sh build --release
scripts/cargo_agent.sh test --quiet --lib
scripts/cargo_agent.sh test --test layout_tests
scripts/cargo_agent.sh check -p fastrender

# If your checkout lost executable bits (e.g. zip/tarball downloads), invoking via `bash` is fine:
bash scripts/cargo_agent.sh check -p fastrender

# WRONG — WILL DESTROY HOST:
cargo test
cargo build --all-targets
cargo check --all-features --tests
```

If you run unscoped cargo commands, you will compile 100+ binaries with LTO, spawn hundreds of parallel rustc/mold processes, exhaust all RAM, and render the machine unusable. **There are no exceptions.**

### Disk hygiene (`target/`)

`target/` grows without bound. Before loops, check size and clean when over budget:

```bash
TARGET_MAX_GB="${TARGET_MAX_GB:-2000}"
TARGET_MAX_BYTES=$((TARGET_MAX_GB * 1024 * 1024 * 1024))
du -xsh target 2>/dev/null || true
if [[ -d target ]]; then
  size_kib="$(du -sk target 2>/dev/null | cut -f1 || echo 0)"
  size_bytes=$((size_kib * 1024))
  if [[ "${size_bytes}" -ge "${TARGET_MAX_BYTES}" ]]; then
    echo "target/ exceeds ${TARGET_MAX_GB}GB budget; running cargo clean..." >&2
    cargo clean
    du -xsh target 2>/dev/null || true
  fi
fi
```

## Regression philosophy (required)

Live pages motivate fixes, but regressions keep them fixed:

- Prefer **unit tests** for parsing/cascade/value computation.
- Use **`tests/layout/`** / **`tests/paint/`** when feasible.
- Use a **tiny offline fixture** only when necessary to reproduce real-world interactions.

When uncertain, add the regression first, then implement the fix.

## Test organization (mandatory)

**NEVER create loose `tests/*.rs` files.** Each `.rs` file directly in `tests/` becomes a separate binary that must be compiled and linked. With 300+ test files, this destroys build times.

**Always add tests to an existing harness subdirectory:**

```
tests/
├── layout_tests.rs      ← harness (auto-discovered, compiles as ONE binary)
├── layout/              ← subdirectory (NOT auto-discovered)
│   ├── mod.rs
│   └── your_new_test.rs ← ADD YOUR TEST HERE
├── paint_tests.rs
├── paint/
├── style_tests.rs
├── style/
├── regression_tests.rs
├── regression/
└── ...
```

**To add a new test:**

1. Find the appropriate category (`layout/`, `paint/`, `style/`, `regression/`, etc.)
2. Create your test file in that subdirectory
3. Add `mod your_new_test;` to the subdirectory's `mod.rs`

**If no category fits**, add to `tests/misc/` and update `tests/misc/mod.rs`.

**NEVER** create a new top-level `tests/foo.rs` file unless you are creating a new harness (requires approval).
