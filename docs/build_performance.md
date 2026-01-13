# Build Performance Guide

This document explains FastRender's build system, why compilation takes so long, and how to minimize it.

**TL;DR for agents:**
- Use `cargo check` for validation, not `cargo build`
- Always use `--bin <name>` when building specific binaries
- `--release` is fast and suitable for most work (no LTO)
- Use `--profile release-max` only for final distribution (full LTO, very slow)
- Incremental compilation is enabled by default—don't disable it

## Why Builds Are Slow

FastRender has fundamental characteristics that make compilation expensive:

### 1. Mega-Crate Architecture

The entire renderer lives in a single crate (`fastrender`) with **1.38 million lines** of Rust:

| Module | Lines | Description |
|--------|-------|-------------|
| `src/layout/` | 282K | Layout algorithms (block, inline, flex, grid, table) |
| `src/js/` | 213K | JavaScript integration |
| `src/paint/` | 208K | Painting and rasterization |
| `src/style/` | 157K | CSS parsing, cascade, computed styles |
| `src/text/` | 54K | Text shaping, fonts, line breaking |
| `src/ui/` | 46K | Browser UI components |
| ... | ... | ... |

**Why this matters:**
- rustc processes the entire crate as one compilation unit
- Changes to core modules (`layout/`, `style/`, `paint/`) invalidate large portions
- No crate-level parallelism for the main code

**Incremental behavior:**
- Touch a leaf file (e.g., `src/js/vmjs/window_console.rs`): ~0.2s rebuild
- Touch a core file (e.g., `src/layout/mod.rs`): ~11s rebuild

### 2. Numerous Targets

| Type | Count | Impact |
|------|-------|--------|
| Binaries (`src/bin/`) | 20+ | Each links against `libfastrender` |
| Benchmarks (`benches/`) | 27 | Each is a separate binary |
| Integration tests | 2 | Allocation-failure tests need custom allocator |

Building "all targets" means 50+ binaries, each with its own link stage.

### 3. Large Dependency Tree

```
$ cargo tree | wc -l
897
```

The full dependency graph includes 897 crates (fonts, crypto, networking, etc.).

## Build Profiles

### Development (`cargo build` or `--profile dev`)

```toml
[profile.dev]
opt-level = 1  # Light optimization

# Critical deps are fully optimized for usable dev experience
[profile.dev.package.taffy]
opt-level = 3
# ... etc
```

**When to use:** Day-to-day iteration, debugging, `cargo check`.

### Release (`cargo build --release`)

```toml
[profile.release]
opt-level = 3
lto = false           # No LTO for fast compilation
codegen-units = 32    # Parallel codegen
debug = 1             # Basic debug info for backtraces
```

**When to use:** Performance testing, running page fixtures, most release work. This is fast to compile while still being well-optimized.

### Release Max (`--profile release-max`)

```toml
[profile.release-max]
lto = true            # Full link-time optimization
codegen-units = 1     # Maximum optimization (sequential)
strip = true          # Remove all debug info
```

**When to use:** Final distribution binaries only. **Extremely slow to compile** (30-60 min for all targets). Never use for iteration.

### Benchmark (`--profile bench`)

```toml
[profile.bench]
opt-level = 2
lto = false
codegen-units = 64
```

**When to use:** Running benchmarks. Fast to compile, representative performance.

## Best Practices for Agents

### Use `cargo check` for Validation

`cargo check` is **much faster** than `cargo build`:
- No code generation, no linking
- Only type-checking and borrow-checking
- Typically 10-50x faster than build

```bash
# GOOD - Fast validation
timeout -k 10 120 bash scripts/cargo_agent.sh check -p fastrender

# BAD - Unnecessarily slow
timeout -k 10 600 bash scripts/cargo_agent.sh build
```

### Always Scope Your Builds

Never build "everything"—always specify exactly what you need:

```bash
# GOOD - Build specific binary
timeout -k 10 300 bash scripts/cargo_agent.sh build --release --bin fetch_and_render

# BAD - Builds ALL 20+ binaries
timeout -k 10 600 bash scripts/cargo_agent.sh build --release
```

### Release vs Release-Max

```bash
# GOOD - Fast release build for testing (no LTO)
timeout -k 10 180 bash scripts/cargo_agent.sh build --release --bin fetch_and_render

# ONLY for final distribution - extremely slow
timeout -k 10 1800 bash scripts/cargo_agent.sh build --profile release-max --bin fetch_and_render
```

### Test Scoping

Always scope test runs to avoid compiling unnecessary targets:

```bash
# GOOD - Run library tests only
timeout -k 10 300 bash scripts/cargo_agent.sh test --lib

# GOOD - Run specific integration test
timeout -k 10 300 bash scripts/cargo_agent.sh test --test integration -- layout::flex

# BAD - Compiles all test targets
timeout -k 10 600 bash scripts/cargo_agent.sh test
```

### Incremental Compilation

Incremental compilation is **enabled by default** in `cargo_agent.sh`. Don't disable it unless you have a specific reason:

```bash
# Default behavior: CARGO_INCREMENTAL=1 (enabled)
bash scripts/cargo_agent.sh build --bin fetch_and_render

# If you must force a clean build:
CARGO_INCREMENTAL=0 bash scripts/cargo_agent.sh build --bin fetch_and_render
```

## sccache (Shared Compilation Cache)

For multi-agent environments, sccache can dramatically reduce redundant compilation by sharing cached artifacts. Currently **disabled by default** due to reliability concerns with daemon state.

### Enabling sccache

```bash
# Enable sccache for this command
FASTR_CARGO_USE_SCCACHE=1 bash scripts/cargo_agent.sh build --bin fetch_and_render
```

### When to Use sccache

- **Multi-agent hosts**: When many agents work on the same repo, sccache prevents duplicate compilation
- **CI pipelines**: Share cache across builds
- **Clean builds**: When you know you'll be rebuilding from scratch

### When NOT to Use sccache

- **Incremental builds**: sccache and incremental compilation don't work well together
- **Unhealthy daemon**: If sccache daemon is down, builds will fail

## Profiling Builds

To understand where time goes:

```bash
# Time each crate's compilation
CARGO_LOG=cargo::core::compiler::job_queue=debug bash scripts/cargo_agent.sh build --bin fetch_and_render 2>&1 | head -100

# Use cargo's built-in timing
bash scripts/cargo_agent.sh build --bin fetch_and_render --timings

# Rust self-profiling (nightly only)
RUSTFLAGS="-Zself-profile" bash scripts/cargo_agent.sh +nightly build --bin fetch_and_render
```

## Future Work: Crate Splitting

The mega-crate architecture is a significant bottleneck. However, splitting requires careful analysis due to circular dependencies.

### Dependency Analysis Results

```
Module (lines)        Depends on (import count)
-------------------------------------------------
layout/  (282K)  →    style (1525), tree (482), geometry (129), text (69)
paint/   (208K)  →    style (432), text (196), tree (108), geometry (127)
style/   (157K)  →    css (304), dom (192), geometry (14), tree (13)
text/    (54K)   →    style (81), paint (20), css (19)
js/      (213K)  →    dom (29), error (21), resource (19), web (12)
tree/              →    style (66), geometry (8), dom (8)
```

### Circular Dependencies (blocking clean splits)

| A | B | A→B | B→A | Notes |
|---|---|-----|-----|-------|
| layout | tree | 482 | 3 | Box tree lives in tree, layout produces fragments |
| paint | text | 196 | 20 | Paint renders text, text uses paint for glyph caching |

### Why Not Split Now?

1. **High risk**: Interface changes across ~50 files per module
2. **Circular breaks require traits**: Adding trait abstractions adds complexity
3. **Build parallelism gains diminished by cycles**: rust compiler can't parallelize cyclic crate graphs
4. **Incremental compilation already helps**: With incremental enabled, leaf file changes are ~1s

**Recommendation**: Prioritize other improvements first. Crate splitting is high effort, moderate reward given existing incremental compilation. Revisit if/when the codebase doubles in size or incremental compilation stops being effective.

## Quick Reference

| Task | Command | Cold | Incremental (leaf) |
|------|---------|------|-------------------|
| Type check | `cargo check -p fastrender` | 10-30s | ~1s |
| Build one binary (dev) | `cargo build --bin <name>` | 30-60s | ~1s |
| Build one binary (release) | `cargo build --release --bin <name>` | ~90s | ~1s |
| Build one binary (release-max) | `cargo build --profile release-max --bin <name>` | 5-10min | 1-2min |
| Run lib tests | `cargo test --lib` | 30-60s | ~1s |
| Run integration tests | `cargo test --test integration` | 60-120s | varies |
| **Build ALL release-max** | `cargo build --profile release-max` | **30-60min** | ⚠️ |

**Key insight**: Incremental compilation makes leaf-file changes nearly instant (~1s). Core module changes (`layout/mod.rs`, `style/mod.rs`) still take 1-2 minutes even with incremental because they invalidate large dependency graphs.

All commands should be run via `scripts/cargo_agent.sh` with appropriate `timeout -k` wrapping.
