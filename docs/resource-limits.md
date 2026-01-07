# Resource limits (RAM / CPU / time) for agents

FastRender work involves hostile inputs (real pages) and complex algorithms. **Any run can go pathological**.
We want safe defaults so no benchmark or CLI can eat the machine.

This doc describes a two-layer strategy:

1. **OS-level hard caps** (always available, works for *any* command)
2. **In-process guardrails** (for our own binaries, better error messages + staged attribution)

## 1) OS-level caps (recommended default for agents)

### Convenience wrapper (recommended encouraging default)

Use the repo helper which prefers `prlimit` and falls back to `ulimit`:

```bash
scripts/run_limited.sh --as 12G --cpu 60 -- \
  cargo bench --bench selector_bloom_bench
```

If `cargo` itself fails early with an `out of memory` message, bump `--as` (the Rust toolchain can
reserve a surprisingly large amount of virtual address space even when RSS is low).

You can also set defaults via environment variables:

```bash
LIMIT_AS=12G LIMIT_CPU=60 scripts/run_limited.sh -- cargo run --release --bin pageset_progress -- run --timeout 5
```

### A. `prlimit` (best general-purpose tool)

If `prlimit` is available (usually via `util-linux`), it can cap address-space and CPU:

```bash
# Note: `prlimit` expects raw byte counts; size suffixes (e.g. 12G) are not universally supported.
prlimit --as=$((12 * 1024 * 1024 * 1024)) --rss=$((12 * 1024 * 1024 * 1024)) --cpu=30 -- \
  cargo run --release --bin pageset_progress -- run --timeout 5
```

Notes:
- `--as` (virtual address space) is the most reliable “hard memory ceiling”.
- `--rss` is not reliably enforced on all kernels; treat it as advisory.
- Cap `cargo` itself if you are running “cargo run”; `cargo` spawns child processes and inherits limits.
  - Some `prlimit` builds do not accept human suffixes reliably (e.g. `--as=12G`). Prefer raw byte
    counts or use `scripts/run_limited.sh`, which converts suffixes to bytes automatically.

### B. `ulimit` (portable shell-level fallback)

In bash/zsh you can cap virtual memory and stack:

```bash
ulimit -v $((12 * 1024 * 1024))  # KiB
ulimit -s $((64 * 1024))         # KiB
```

Then run your command in the same shell.

### C. cgroups / systemd (best isolation)

If systemd is available:

```bash
systemd-run --user -p MemoryMax=12G -p CPUQuota=200% -- \
  cargo run --release --bin pageset_progress -- run --timeout 5
```

This is the most robust approach for multi-agent environments.

## 2) In-process caps (for FastRender binaries)

OS caps are blunt: they stop the process, but don’t tell us *why*. For FastRender CLIs, prefer:

- **Hard wall-clock timeout** (already exists in `pageset_progress` and `render_pages`)
- **Cooperative per-stage deadlines** (so timeouts are attributed to a stage)
- **Bounded caches** (LRU with explicit byte/item caps)

### Available guardrails

The render-driving CLIs (`pageset_progress`, `render_pages`, `fetch_and_render`) support:

- `--mem-limit-mb <N>`: **hard process memory ceiling** in MiB (`0` disables).
  - Linux: enforced via `setrlimit(RLIMIT_AS, …)` early in startup (including worker subcommands).
  - Non-Linux: the flag is accepted but ignored with a warning.
- `--stage-mem-budget-mb <N>`: **best-effort per-stage RSS budget** in MiB (`0` disables).
  - RSS is sampled at the start/end of each pipeline stage (DomParse/Css/Cascade/BoxTree/Layout/Paint).
  - When the sampled RSS exceeds the configured budget, the render aborts with a structured
    `RenderError::StageMemoryBudgetExceeded { stage, rss_bytes, budget_bytes }`.

Examples:

```bash
# 8 GiB hard ceiling via RLIMIT_AS, abort render stages that grow beyond 2 GiB RSS
cargo run --release --bin pageset_progress -- run \
  --mem-limit-mb 8192 \
  --stage-mem-budget-mb 2048
```

### Diagnostics

When diagnostics/stats output is enabled (e.g. `pageset_progress --diagnostics basic` or
`render_pages --diagnostics-json`), JSON diagnostics include per-stage RSS samples:

- `diagnostics.stats.memory.dom_parse.rss_start_bytes`
- `diagnostics.stats.memory.dom_parse.rss_end_bytes`
- …and the same fields for `css`, `cascade`, `box_tree`, `layout`, `paint`.

### What to implement next (repo plan)

- **Add per-stage “allocation budget” counters** for known hotspots (images, CSS parse, display list build).
  - Goal: fail with a diagnostic like “paint rasterize exceeded 512MB budget” instead of OOM.
- **Make every unbounded cache bounded** (items and/or bytes) with explicit configuration knobs.
- **Bench safety**: benches must never allocate unboundedly by default (see below).

## Operational guidance

- For pageset runs, set an OS memory cap by default (cgroups or prlimit).
- When a cap is hit, treat it as a **bug**: either an algorithmic explosion or an unbounded cache.
- The “correct fix” is almost always: reduce asymptotic work, add early exits, and bound caches—**not** “skip rendering”.

## Bench safety

Agents occasionally run `cargo bench`. A single pathological benchmark can OOM the host.
Benchmarks must be **safe-by-default**: bounded memory/CPU even when inputs (env vars, fixture
files) are hostile.

### Standard env vars (benches)

Set `FASTR_BENCH_VERBOSE=1` to print the *effective* caps once at bench startup.

Common caps (with conservative defaults):

- `FASTR_BENCH_MAX_FIXTURE_BYTES` (default: `8MiB`)
  - Max bytes a benchmark will read from any on-disk fixture.
  - Oversized fixtures are skipped with a clear message (or truncated deterministically when
    appropriate for the bench).
- `FASTR_BENCH_MAX_THREADS` (default: `8`)
  - Caps rayon threadpools / parallel paint/layout benchmarks.
- `FASTR_BENCH_MAX_DOM_NODES` (default: `100_000`)
  - Caps synthetic DOM generators (depth, fan-out, list size env knobs).
- `FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS` (default: `200_000`)
  - Caps synthetic display-list / background-tiling workloads.
- `FASTR_BENCH_MAX_DEPTH` (default: `256`)
  - Caps recursion depth for synthetic tree builders.

Some benches also accept legacy, bench-specific knobs. For example,
`benches/selector_bloom_bench.rs` still accepts `FASTR_BLOOM_BENCH_MAX_ELEMS` as an override for the
DOM node cap.

### Always use OS-level caps too

In-process caps prevent accidental runaway allocations, but the most reliable protection is still
an OS-enforced ceiling. Prefer:

```bash
FASTR_BENCH_VERBOSE=1 scripts/run_limited.sh --as 12G --cpu 60 -- \
  cargo bench --bench selector_bloom_bench -- --noplot
```
