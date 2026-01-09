# Pageset page loop (fix pages one-by-one)

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

AGENTS.md is the law. These rules are not suggestions. Violating them destroys host machines, wastes hours of compute, and blocks other agents. Non-compliance is unacceptable.

**MANDATORY (no exceptions):**
- Use `scripts/cargo_agent.sh` for ALL cargo commands (build, test, check, clippy)
- Use `scripts/run_limited.sh --as 64G` when executing ANY renderer binary
- Scope ALL test runs (`-p <crate>`, `--test <name>`, `--lib`) — NEVER run unscoped tests

**FORBIDDEN — will destroy the host:**
- `cargo build` / `cargo test` / `cargo check` without wrapper scripts
- `cargo test --all-features` or `cargo check --all-features --tests`
- Unscoped `cargo test` (compiles 300+ test binaries and blows RAM)

If you do not understand these rules, re-read AGENTS.md. There are no exceptions. Ignorance is not an excuse.

---

This document defines the **page-by-page** work mode: pick a pageset page, turn it inside out, and keep fixing it until it renders at **really good quality**.

This is intentionally **not** “work by subsystem” (style/layout/paint/etc.). We parallelize by **page** to avoid the coordination bottleneck of feature ownership.

## Scope

- **Source of truth pages**: `src/pageset.rs` (the official pageset list).
- **Goal**: each page becomes “good” (visually correct enough that remaining differences are clearly attributable to *known, explicit missing features*, not pervasive broken primitives).
- **How to measure**: you do **not** need Chrome baselines, pixel diffs, or stats dashboards. Use judgment, specs, and tight regressions.

## Rules (non-negotiable)

- **No page-specific hacks**: no hard-coded hostnames, selectors, magic numbers, special-case branches for one site.
- **No deviating-spec behavior**: implement the spec behavior the page depends on, even if partial/incomplete elsewhere.
- **Fix root causes, not symptoms**: if something looks wrong, find the primitive that is wrong (parsing/cascade/layout/paint/text/resources) and correct it.
- **Add regressions**: every meaningful fix must land with a regression (unit/layout/paint fixture). The page is the motivation; the regression is the permanent guardrail.

## Parallelism policy (by page)

- One agent owns one page at a time.
- You may touch any subsystem needed (style/layout/paint/text/resources), but **the page is the organizing unit**.
- If multiple agents must touch the same primitive, coordinate by *landing minimal regressions first*, then implement fixes.

## The loop (do this repeatedly for the page)

### 1) Choose and “freeze” the page

- Pick a pageset URL from `src/pageset.rs`.
- Prefer working from cached inputs when possible (stable HTML/CSS/resources) so “the target” doesn’t move every run.

### 2) Create a concrete “brokenness inventory”

Do not hand-wave “layout is wrong.” Write down **specific observable failures**, e.g.:

- “Header overlaps hero image”
- “Links are stacked vertically instead of horizontal nav”
- “Floats intrude into line boxes; text wraps through image”
- “Form controls have wrong intrinsic size / baseline”
- “Stacking context order is wrong; overlay appears behind content”

For each item, identify the likely subsystem:

- **Style/cascade**: wrong computed value, wrong inheritance, wrong percent base, `var()`/shorthand resolution, selector matching.
- **Layout**: formatting context, intrinsic sizing, shrink-to-fit, min/max constraints, float avoidance, fragmentation.
- **Paint**: stacking context ordering, clipping, transforms, border radius, blend modes, text decorations.
- **Text**: font fallback, metrics, line-height, shaping, bidi, wrapping.
- **Resources**: base URL, redirects, caching, content-type sniffing, decoding, image sizing, SVG.

### 3) “Turn the guts inside out” until you can explain the failure

Use the renderer’s inspection/debug tooling to locate the first wrong decision:

- DOM → styled tree: is the element present? are attributes parsed? are styles applied?
- Styled → box tree: are anonymous wrappers correct? `display: contents`? pseudo-elements?
- Box tree → layout: are containing blocks correct? percent bases? intrinsic sizing?
- Layout → fragments: are positions/bounds correct? line boxes? floats?
- Fragments → display list: is painting order correct? clips/transforms?

If you can’t explain the failure in terms of a spec rule + code path, you are not done investigating.

### 4) Implement the missing primitive (spec-first)

- Read the relevant spec algorithm (CSS2.1 / Selectors / CSS Values / Positioning / Flexbox / Grid / Painting).
- Implement the smallest correct slice that fixes the page **without** hacks.
- Keep the change local and reviewable; avoid broad refactors unless absolutely necessary.

### 5) Add the regression

Prefer, in order:

1. **Unit test** (parser / cascade / computed value).
2. **Layout test** under `tests/layout/`.
3. **Paint test** under `tests/paint/`.
4. **Tiny fixture** (offline HTML/CSS) only when the behavior can’t be expressed otherwise.

The regression should encode the *primitive* (not the whole live page).

### 6) Repeat until the page is “good”

“Good” means:

- major structure is correct (no global collapse into one column, no massive overlap),
- text flows in the correct regions (no pervasive float/linebox failures),
- images/replaced elements size and place correctly in the main layout,
- stacking/clipping is mostly correct for the visible chrome/overlays,
- remaining issues are attributable to **explicit missing features** you can name.

## What counts as “done” for a page

You can declare a page “good” when:

- you can enumerate the remaining diffs as a short list of missing features (with at least one tracked regression per new primitive you implemented), and
- there are no “mystery” failures you cannot trace to a spec rule + code path.

If you reach diminishing returns, stop and pick the next page; the goal is a steady march toward “most pages are good.”
