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

### Cargo builds/tests (avoid stampedes)

Many agents running Cargo concurrently can spawn thousands of `rustc`/linker processes and blow up RAM.

- **Do not run `cargo …` directly**; use `scripts/cargo_agent.sh` to throttle concurrent cargo invocations.
- **Do not run unscoped `cargo test`**; always scope the target(s) you need.

```bash
scripts/cargo_agent.sh build --release
scripts/cargo_agent.sh test --quiet --lib
```

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
