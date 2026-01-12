# Workstream: Pageset Page Loop (fix pages one-by-one)

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

You're rendering real web pages — hostile inputs by definition. **Any render can hang, explode memory, or timeout.** A single degenerate CSS rule can trigger infinite layout. A malformed image can decode to gigabytes. A script can loop forever.

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL (SIGTERM alone can be ignored)
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

If something exceeds limits, that's a **bug to investigate**, not a limit to raise.

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries
- Scope ALL test runs (`-p <crate>`, `--test <name>`, `--lib`) — NEVER run unscoped tests

**FORBIDDEN — will destroy the host:**
- ANY command without `timeout -k` (can hang forever)
- `cargo build` / `cargo test` / `cargo check` without wrapper scripts
- `cargo test --all-features` or unscoped `cargo test`

---

This workstream owns **fixing pages end-to-end**: pick a pageset page, diagnose issues, fix root causes, and make it render correctly.

## Relationship to other workstreams

This workstream is **vertically integrated** — you touch whatever subsystem is broken:

- **Capability buildout** (`capability_buildout.md`) — Generic primitives land there with regressions
- **Browser chrome** (`browser_chrome.md`) — Page navigation/loading issues
- **JS workstreams** (`js_*.md`) — Script execution issues

When you fix a page, the **primitive** gets a generic regression in the appropriate workstream. The page just validates the fix works end-to-end.

## The job

Pick a pageset page, turn it inside out, and keep fixing it until it renders at **really good quality**. Then move to the next page.

This is intentionally **not** "work by subsystem" (style/layout/paint/etc.). We parallelize by **page** to avoid the coordination bottleneck of feature ownership.

## What counts

A change counts if it lands at least one of:

- **Page improvement**: A specific page renders better (with regression for the fixed primitive).
- **Root cause fix**: A bug affecting multiple pages is fixed (with regression).
- **Page declared "good"**: A page reaches "good" quality with documented remaining gaps.

## Scope

- **Source of truth pages**: `src/pageset.rs` (the official pageset list, ~170 pages).
- **Progress tracking**: `progress/pages/<stem>.json` files.
- **Goal**: each page becomes "good" (visually correct enough that remaining differences are clearly attributable to *known, explicit missing features*, not pervasive broken primitives).
- **How to measure**: you do **not** need Chrome baselines, pixel diffs, or stats dashboards. Use judgment, specs, and tight regressions.

## Rules (non-negotiable)

- **No page-specific hacks**: no hard-coded hostnames, selectors, magic numbers, special-case branches for one site.
- **No deviating-spec behavior**: implement the spec behavior the page depends on, even if partial/incomplete elsewhere.
- **Fix root causes, not symptoms**: if something looks wrong, find the primitive that is wrong (parsing/cascade/layout/paint/text/resources) and correct it.
- **Add regressions**: every meaningful fix must land with a regression (prefer a unit test next to the code; use an integration test only when exercising public APIs/fixtures/WPT; use a tiny offline fixture only when needed). The page is the motivation; the regression is the permanent guardrail.

## Supporting documentation

- **Accuracy workflow**: [`docs/accuracy_workflow.md`](../docs/accuracy_workflow.md) — Detailed step-by-step guide for fixing accuracy issues
- **Philosophy**: [`docs/philosophy.md`](../docs/philosophy.md) — Hard-won lessons and mindset principles

## The loop (do this repeatedly for the page)

### 1) Choose and "freeze" the page

- Pick a pageset URL from `src/pageset.rs`.
- Prefer working from cached inputs when possible (stable HTML/CSS/resources) so "the target" doesn't move every run.
- Check `progress/pages/<stem>.json` for current status.

### 2) Create a concrete "brokenness inventory"

Do not hand-wave "layout is wrong." Write down **specific observable failures**, e.g.:

- "Header overlaps hero image"
- "Links are stacked vertically instead of horizontal nav"
- "Floats intrude into line boxes; text wraps through image"
- "Form controls have wrong intrinsic size / baseline"
- "Stacking context order is wrong; overlay appears behind content"

For each item, identify the likely subsystem:

- **Style/cascade**: wrong computed value, wrong inheritance, wrong percent base, `var()`/shorthand resolution, selector matching.
- **Layout**: formatting context, intrinsic sizing, shrink-to-fit, min/max constraints, float avoidance, fragmentation.
- **Paint**: stacking context ordering, clipping, transforms, border radius, blend modes, text decorations.
- **Text**: font fallback, metrics, line-height, shaping, bidi, wrapping.
- **Resources**: base URL, redirects, caching, content-type sniffing, decoding, image sizing, SVG.

### 3) "Turn the guts inside out" until you can explain the failure

Use the renderer's inspection/debug tooling to locate the first wrong decision:

- DOM → styled tree: is the element present? are attributes parsed? are styles applied?
- Styled → box tree: are anonymous wrappers correct? `display: contents`? pseudo-elements?
- Box tree → layout: are containing blocks correct? percent bases? intrinsic sizing?
- Layout → fragments: are positions/bounds correct? line boxes? floats?
- Fragments → display list: is painting order correct? clips/transforms?

If you can't explain the failure in terms of a spec rule + code path, you are not done investigating.

### 4) Implement the missing primitive (spec-first)

- Read the relevant spec algorithm (CSS2.1 / Selectors / CSS Values / Positioning / Flexbox / Grid / Painting).
- Implement the smallest correct slice that fixes the page **without** hacks.
- Keep the change local and reviewable; avoid broad refactors unless absolutely necessary.

### 5) Add the regression

Prefer, in order:

1. **Unit tests in `src/`** next to the code you changed (parser / cascade / computed value).
   - Layout primitives: unit tests under `src/layout/**` (or `src/layout/tests/**`, depending on the final layout test structure).
   - Paint primitives: unit tests under `src/paint/**` (or `src/paint/tests/**`, depending on the final paint test structure).
2. **Integration tests under `tests/`** (via `tests/integration.rs` modules) only when you need to exercise the public API, offline fixture runners, or WPT harnesses.
3. **Tiny offline fixture** (minimal HTML/CSS/assets) only when the behavior can't be expressed cleanly as a unit test.

The regression should encode the *primitive* (not the whole live page).

How to run the relevant tests (always wrap with `timeout -k` per the safety rules at the top of this doc):

```bash
# Unit tests (in `src/`):
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib <filter>

# Integration tests (in `tests/integration.rs`):
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration <filter>
```

### 6) Repeat until the page is "good"

"Good" means:

- major structure is correct (no global collapse into one column, no massive overlap),
- text flows in the correct regions (no pervasive float/linebox failures),
- images/replaced elements size and place correctly in the main layout,
- stacking/clipping is mostly correct for the visible chrome/overlays,
- remaining issues are attributable to **explicit missing features** you can name.

## What counts as "done" for a page

You can declare a page "good" when:

- you can enumerate the remaining diffs as a short list of missing features (with at least one tracked regression per new primitive you implemented), and
- there are no "mystery" failures you cannot trace to a spec rule + code path.

If you reach diminishing returns, stop and pick the next page; the goal is a steady march toward "most pages are good."

## Success criteria

The pageset workstream is **done** when:

- >90% of pageset pages are "good" quality
- Remaining issues on each page are documented with known root causes
- No page has "mystery" failures that can't be explained
- New pages can be added and quickly brought to "good" status
