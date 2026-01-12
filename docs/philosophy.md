# FastRender Philosophy & Culture

This document captures hard-won lessons, mindset principles, and operational wisdom accumulated over the project's history. **Read this twice.**

## The product (what we're building)

FastRender is building a **production-quality browser rendering engine**. Not a demo. Not a research project. A real engine that renders real pages correctly.

**Correct pixels are the product.** Everything else—performance, tooling, infrastructure—exists to help us ship correct pixels faster.

## Core philosophy

### 1. Accuracy is the product

- Our job is to make pages render *correct pixels* (layout, paint, text, images).
- Performance and data are tools to reach that goal faster, not goals we chase for their own sake.
- Wrong/blank/missing output is a bug. Treat "it's probably JS" as a hypothesis to disprove, not a conclusion.

### 2. Spec-first, always

- Implement CSS/HTML algorithms as written in the specs.
- "Incomplete but correct" beats "complete but wrong".
- No page-specific hacks, no hostname checks, no magic numbers for one site.
- No post-layout pixel nudging that contradicts the spec.

### 3. Performance is correctness

- A renderer that times out or loops is wrong.
- A renderer that crashes is wrong.
- Speed problems become correctness problems when they prevent rendering.

### 4. The 90/10 rule

- **90%** of effort: accuracy + capability (implementing missing spec features)
- **10%** of effort: performance + infra (only as needed to avoid timeouts/loops)

Perf is not the product. Correct pixels are the product.

## Culture & mindset

### No vanity work

Changes that don't improve pageset accuracy, eliminate a crash/timeout, or reduce uncertainty for an imminent fix are **not acceptable**. Instrumentation that never leads to a fix is waste.

### Ruthless triage

If you can't turn a symptom into a task with a measurable outcome quickly, **stop and split the work**. Don't spend days on vague problems.

### Accountability

Progress must be visible, comparable, and committed. Regressions must be obvious. We track the pageset in-repo (`progress/pages/*.json`) so everyone can see what's working and what's not.

### Worship data (in service of accuracy)

We don't "feel" performance or correctness—we measure it:
- `progress/pages/*.json` deltas
- Timing breakdowns
- Traces and logs
- Tests (unit/layout/paint/regressions)

Data is only valuable if it helps us ship more correct pageset renders.

### No shortcuts

The hard budgets (5s timeout, memory caps) are NOT permission to ship hacks, workarounds, TODOs, partial implementations, or "close enough" behavior.

They are pressure to **think deeply** and **grind** until the solution is both **correct and fast**. Fix root causes; don't paper over them.

### What does NOT count (guard against drift)

- **Tooling/infra-only work does not count** unless it is immediately used to ship an accuracy/capability/stability/termination win (same session or the very next commit).
- **Perf-only work does not count** unless it fixes a timeout/loop, prevents oversubscription, or is required to make an accuracy fix feasible.
- **Docs-only work does not count** unless it removes a real source of confusion that is actively blocking pageset fixes.

If you find yourself "improving the harness" without changing renderer behavior, **stop and implement the missing behavior**.

## The data-driven method (how we work)

The 90/10 rule tells you **what to prioritize**. This section tells you **how to work** on anything.

### The principle: instrument everything

Every pipeline stage, every algorithm, every code path should be observable:
- **Inject** instrumentation at key points (timers, counters, trace spans)
- **Trace** data flow through the system (input → output at each stage)
- **Collect** evidence continuously (not just when debugging)
- **Understand** what the data tells you (patterns, anomalies, regressions)
- **Systematize** the workflow (make investigation reproducible)

This isn't about "performance optimization" - it's about **understanding your system**. You can't fix what you can't see.

### Apply to accuracy work (90%)

When working on accuracy:
- Instrument **spec coverage** (which CSS properties work? which selectors?)
- Trace **value flow** (parsed → computed → used → actual)
- Collect **visual evidence** (fixtures, chrome diffs, WPT results)
- Understand **failure patterns** (which pages fail? which features are missing?)
- Systematize **the accuracy workflow** (docs/accuracy_workflow.md)

### Apply to performance work (10%)

When working on performance:
- Instrument **stage timings** (`stages_ms` in progress JSON)
- Trace **hotspots** (which functions dominate? which inputs are pathological?)
- Collect **profiles** (samply, perf, flamegraphs)
- Understand **complexity** (O(n)? O(n²)? data-dependent?)
- Systematize **the triage workflow** (docs/triage.md)

### The key insight: method serves goal

The pendulum swung from "perf focus" to "90/10 accuracy focus" because perf work was happening at the expense of accuracy.

The fix isn't to abandon the data-driven method. The fix is to **apply the method to the right goal**:

```
WRONG:  "We worship data" → optimize whatever data looks bad → lose sight of product
RIGHT:  "Accuracy is the product" → worship accuracy data → use perf data to unblock accuracy
```

Performance work is justified when:
1. A page **times out** (can't measure accuracy if it doesn't render)
2. Iteration speed **blocks accuracy work** (can't fix fast if pageset takes 10 min)
3. A performance fix **enables** an accuracy fix (e.g., render finishes so you can diff)

Otherwise, stay on accuracy.

## Tried patterns (lessons learned the hard way)

### ❌ Failed approaches

These approaches have been tried and don't work:

1. **CSS-only solutions** — Can't solve layout engine limitations with CSS tricks
2. **Pre-layout width/height setting** — Layout engines override during calculation phase
3. **Complex selector matching for special cases** — Simple class-based detection is more reliable
4. **Flex basis forcing in constrained containers** — Layout engines ignore it
5. **Assuming flex containers collect inline text** — They don't (tables are different)
6. **Batching fixes without validation** — Visual differences compound; validate each change

### ✅ Successful patterns

These approaches work:

1. **Post-layout modification** — Override after the layout engine calculates when necessary
2. **Content injection during extraction** — Force text at layout tree building time
3. **Exception-based painting** — Allow special elements to bypass filters when needed
4. **Recursive content detection** — Walk tree to find elements that need special handling
5. **Debug output at every stage** — Track data through transformations
6. **One fix at a time** — Validate each change immediately against reference

### Repeated lessons (memorize these)

1. **Layout engine constraints are absolute** — Cannot negotiate with calculation phases
2. **Text flows differently in different contexts** — Inline children don't auto-collect in flex
3. **Paint order matters** — Background before text, layering per spec
4. **Debug output is essential** — Visual debugging insufficient for layout issues
5. **Force content early** — Easier than fixing missing content later
6. **Absolute coordinates work** — When relative positioning fails
7. **Simple detection beats complex selectors** — Class checks more reliable than CSS selector matching

## The development loop (non-negotiable)

**ESSENTIAL**: Continuously compare output against reference render. Never stop until pixel-perfect.

```
1. Run render
2. Compare visually (output vs reference)
3. Identify differences (missing elements, wrong positioning, incorrect colors)
4. Add debug output (track specific failing elements through pipeline)
5. Implement fix (target the exact constraint causing the issue)
6. Repeat immediately (don't batch fixes, validate each change)
```

This loop is non-negotiable. Visual differences indicate system failures that compound. Each iteration should bring closer to pixel-perfect match.

## Problem-solving approach

When something renders wrong:

1. **Isolate the pipeline stage** — Where does data get lost/corrupted?
   - DOM → styled tree: is the element present? are attributes parsed? are styles applied?
   - Styled → box tree: are anonymous wrappers correct? `display: contents`? pseudo-elements?
   - Box tree → layout: are containing blocks correct? percent bases? intrinsic sizing?
   - Layout → fragments: are positions/bounds correct? line boxes? floats?
   - Fragments → display list: is painting order correct? clips/transforms?

2. **Add debug output** — Track data through transformations

3. **Identify constraint** — Which system is preventing correct behavior?

4. **Override at lowest level** — Closest to final output for maximum control

5. **Test incrementally** — One fix at a time, verify each step

If you can't explain the failure in terms of a spec rule + code path, you are not done investigating.

## Evidence requirements

"Looks better" without artifacts is **not evidence**. For accuracy work, evidence should be:

### Preferred: Offline repro + golden/regression
- A minimized fixture
- An imported page fixture
- A WPT-style reftest with expected image

### Acceptable: Chrome-vs-FastRender diff on fixtures
- Deterministic (offline, no network)
- Uses bundled fonts (stable across machines)
- Written report under `target/`

### Best-effort: Chrome-vs-FastRender diff on cached HTML
- May be non-deterministic due to live subresources
- Treat as rapid triage only

## Hard budgets (boundaries, not strategy)

- **Hard timeout**: Every pageset render must finish in **< 5 seconds**. If it doesn't, it's a bug.
- **Target**: Pages should render in **< 100ms** (longer-term goal).
- **Panic boundaries**: Panics are P0 bugs, not "it crashed" handwaving.
- **Memory caps**: OS-level limits (64GB default) to prevent one bad case from freezing the host.

**Budgets are boundaries, not a strategy.** Do not "meet" the budget by skipping work, clamping away correctness, or degrading output. Meet the budget by fixing algorithms, data structures, and invariants.

## Code organization principles

- **Separate concerns** — Parse, style, layout, paint are distinct phases
- **Data transformation** — Each stage produces input for next stage
- **Error boundaries** — Handle failures at stage boundaries
- **Debugging hooks** — Consistent logging patterns across modules
- **No global state** — Keep state explicit and local
- **Prefer immutable** — Build styled tree, don't mutate original DOM

## Memory management

- **DOM tree** — Keep original for reference
- **Styled tree** — Intermediate representation with computed styles
- **Layout tree** — Internal representation (Taffy for flex/grid, native for tables/block/inline)
- **Fragment tree** — Final positioned elements
- **Display list** — Paint operations for rasterization
- **Clean separation** — Each stage owns its data structures

## Error handling strategy

- **Parse errors** — Continue with partial data when possible
- **Layout failures** — Fallback to minimal valid layout
- **Paint errors** — Skip problematic elements rather than crash
- **Network errors** — Clear error messages for debugging
- **Never panic** — Return errors cleanly and bound work
