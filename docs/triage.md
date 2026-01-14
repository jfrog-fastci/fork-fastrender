# Triage & Operations

This document describes how to prioritize work, classify failures, and operate effectively as an agent on FastRender.

**See also**: [`docs/philosophy.md`](philosophy.md) for the data-driven methodology that underlies all triage work.

## Priority order (how to spend time)

When deciding what to work on, follow this order:

| Priority | Category | Description |
|----------|----------|-------------|
| **P0** | Panics / crashes | Any panic in production code is a P0 bug. No "it crashed" handwaving. |
| **P1** | Timeouts / loops | Must get under 5s hard budget. A renderer that doesn't finish is wrong. |
| **P2** | Accuracy failures | Missing content, wrong layout/paint, unreadable text on pageset pages. |
| **P3** | Big-stage hotspots | Cascade/layout/paint dominating when they block renders or iteration speed. |
| **P4** | Fidelity polish | Minor visual improvements that don't affect core functionality. |
| **P5** | Spec expansion | Only when it directly moves pageset accuracy/perf. |

**Rule**: Don't skip to P5 while P0-P2 issues exist. Fix crashes before polish.

## Failure classification (hotspot taxonomy)

When a pageset page fails or is slow, classify the failure to route to the right fix:

| Hotspot | Symptoms | Investigation |
|---------|----------|---------------|
| **fetch** | Page doesn't load, missing resources | Network logs, `FASTR_LOG_FETCH=1`, check URLs |
| **css** | Styles not applied, parse errors | CSS parse logs, check @import chains |
| **cascade** | Slow style computation, wrong specificity | `stages_ms.cascade`, selector complexity |
| **box_tree** | Wrong structure, missing elements | `inspect_frag`, display property issues |
| **layout** | Wrong positions/sizes, loops | `stages_ms.layout`, containing block issues |
| **paint** | Wrong stacking, missing backgrounds | Display list inspection, z-index issues |
| **decode** | Image/font decode slow or failing | Resource decode logs, format issues |
| **unknown** | Can't classify | Needs deeper investigation |

The `progress/pages/*.json` files include `hotspot` and `stages_ms` to help classify.

## The operating model

### Main planner: pageset-first triage loop

The main planner should **continuously**:

```
1. Fetch all pages (cache) using fetch_pages
2. Render all pages with hard timeout (5s) and panic containment
3. Record metrics per page (progress/pages/*.json)
4. Triage quickly, classify failures by hotspot
5. Spawn workers with tight scopes and measurable goals
```

**Critical**: Do NOT assign "one worker per page" long-term. Pages overlap in root causes. **Split by failure class / hotspot**, not by page.

Example: If 5 pages timeout in layout, assign one worker to fix the layout issue, not 5 workers to each page.

### Workers: definition of "done"

A worker task is only "done" if it produces at least one of:

| Outcome | Evidence |
|---------|----------|
| Page transitions **timeout → render** | `status` changes from "timeout" to "ok" in progress JSON |
| Page gets **materially faster** | Lower `total_ms` or dominant `stages_ms` bucket |
| **Panic/crash eliminated** | Regression test added, no more panic |
| **Correctness fix** | Observable improvement on pageset (ideally with fixture) |

**If you can't show a measurable delta, you are not done.** Stop, re-scope, or pick a different task.

### What workers should NOT do

- Spend time on "improving harnesses" without landing renderer changes
- Optimize code paths that aren't hotspots
- Add instrumentation that never leads to a fix
- Work on spec features not needed by pageset pages
- Polish when crashes/timeouts exist

## Progress artifacts (committed scoreboard)

We track the pageset in-repo so progress/regressions are visible and undeniable.

### Location

```
progress/pages/<stem>.json   # One file per page
```

The `<stem>` matches the cache filename (normalized: strip scheme + leading `www.`, sanitize).

### Schema (source of truth is code)

```json
{
  "url": "https://example.com/",
  "status": "ok|timeout|panic|error",
  "total_ms": 123.4,
  "stages_ms": {
    "fetch": 0.0,
    "css": 0.0,
    "cascade": 0.0,
    "box_tree": 0.0,
    "layout": 0.0,
    "paint": 0.0
  },
  "hotspot": "fetch|css|cascade|box_tree|layout|paint|unknown",
  "failure_stage": "dom_parse|css|cascade|box_tree|layout|paint|null",
  "timeout_stage": "dom_parse|css|cascade|box_tree|layout|paint|null",
  "notes": "short, durable explanation of current blocker",
  "auto_notes": "machine-generated diagnostics (overwritten each run)",
  "last_good_commit": "abcdef0",
  "last_regression_commit": "1234567"
}
```

### Rules

- **Don't hand-author these files.** They are written by tooling (`pageset_progress`).
- If you must edit anything by hand, keep it to durable human fields like `notes` / `last_*`.
- Don't commit machine-local paths, traces, or blobs.
- Keep key ordering stable and formatting consistent.

### Using the scoreboard

```bash
# View top failures
timeout -k 10 60 bash scripts/cargo_agent.sh run --release --bin pageset_progress -- report --top 15

# View specific page
timeout -k 10 60 bash scripts/cargo_agent.sh run --release --bin pageset_progress -- report --pages example.com

# Update scoreboard (run full pageset)
timeout -k 10 900 bash scripts/cargo_agent.sh xtask pageset
```

## Performance investigation workflow

When a page is slow (but not timing out):

### 1. Identify the hotspot

```bash
# Check which stage dominates
timeout -k 10 60 bash scripts/cargo_agent.sh run --release --bin pageset_progress -- report --pages example.com
```

Look at `stages_ms` to see where time is spent.

### 2. Profile the slow stage

```bash
# Profile with samply (writes profile file + prints summary)
scripts/profile_samply.sh example.com --timeout 10

# Or with perf on Linux
scripts/profile_perf.sh example.com --timeout 10
```

### 3. Drill into the hotspot

| Hotspot | Next steps |
|---------|------------|
| **cascade** | Check selector complexity, specificity calculations, inheritance chains |
| **layout** | Check for layout loops, expensive intrinsic sizing, deep flex/grid nesting |
| **paint** | Check display list size, excessive repaints, large images |
| **fetch** | Check network parallelism, blocked resources, redirect chains |

### 4. Fix and verify

```bash
# Re-run pageset after fix
timeout -k 10 900 bash scripts/cargo_agent.sh xtask pageset

# Compare before/after
git diff progress/pages/example.com.json
```

## Timeout investigation workflow

When a page times out:

### 1. Identify where it's stuck

Check `timeout_stage` in the progress JSON. If null, the timeout happened before stage tracking kicked in.

### 2. Run with verbose logging

```bash
FASTR_LOG_LAYOUT=1 timeout -k 10 30 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- \
  https://example.com out.png --timeout 30
```

### 3. Look for loops

Common timeout causes:
- **Layout loops**: Flex/grid constraints that don't converge
- **Infinite recursion**: Deep nesting or circular references
- **Resource stalls**: Waiting on network that never completes
- **Exponential blowup**: Algorithm with bad complexity on pathological input

### 4. Create minimal repro

```bash
# Capture a deterministic offline bundle (crawl mode avoids renderer crashes/timeouts)
timeout -k 10 300 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin bundle_page -- \
  fetch --no-render https://example.com --out /tmp/timeout_repro_bundle.tar

# Import it as an offline fixture for minimization/regression work:
timeout -k 10 300 bash scripts/cargo_agent.sh xtask import-page-fixture /tmp/timeout_repro_bundle.tar timeout_repro --overwrite

# Note: media sources (`<video src>`, `<audio src>`, `<source src>`, `<track src>`) are rewritten to
# deterministic empty placeholder files by default so fixtures stay small. If you need **playable**
# media in the offline fixture (e.g. browser UI testing), add `--include-media` (subject to size
# budgets; see `xtask import-page-fixture --help`).
```

Then minimize the fixture until you find the smallest case that triggers the timeout.

## Panic investigation workflow

When a page panics:

### 1. Get the backtrace

```bash
RUST_BACKTRACE=1 timeout -k 10 60 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- \
  https://example.com out.png
```

### 2. Identify the panic location

Look for the first frame in `src/` (not stdlib or dependencies).

### 3. Create minimal repro

Same as timeout workflow - bundle and minimize.

### 4. Fix with regression

Add a test that would have caught the panic, then fix the code.

## Environment variables for debugging

| Variable | Purpose |
|----------|---------|
| `RUST_BACKTRACE=1` | Show backtraces on panic |
| `FASTR_LOG_FETCH=1` | Log resource fetching |
| `FASTR_LOG_CSS=1` | Log CSS parsing |
| `FASTR_LOG_LAYOUT=1` | Log layout calculations |
| `FASTR_LOG_PAINT=1` | Log paint operations |
| `FASTR_RENDER_TIMINGS=1` | Show stage timing breakdown |

See `docs/env-vars.md` for full list.

## Tooling reference

| Tool | Purpose |
|------|---------|
| `pageset_progress report` | View scoreboard |
| `scripts/pageset.sh` | Run full pageset loop |
| `scripts/profile_samply.sh` | Profile with samply |
| `scripts/profile_perf.sh` | Profile with perf (Linux) |
| `inspect_frag` | Inspect fragment tree |
| `render_fixtures` | Render offline fixtures |
| `diff_renders` | Compare render outputs |

See `docs/cli.md` for full tooling documentation.
