# Test Architecture Cleanup

This document describes the complete restructuring of FastRender's test organization from the current 60+ binary mess to the correct Rust test architecture.

**This is not optional cleanup. This is fixing a fundamental architectural mistake.**

---

## Current State (broken)

- **60 top-level `tests/*.rs` files** → 60 separate test binaries
- **2-3 minutes link time** on clean build (60 × 2-3s per binary)
- **Cargo-cult `#[path = ...]` shims** creating duplicate binaries for "isolation"
- **Unit tests masquerading as integration tests** in `tests/` instead of `src/`
- **Large test files at wrong level** (e.g., `animation_tests.rs` with 2600 lines at top level)
- **Inconsistent naming** (`_tests.rs`, `_test.rs`, neither)

---

## Target State (correct)

```
src/
├── lib.rs
├── layout/
│   ├── mod.rs
│   ├── flex.rs
│   │   └── #[cfg(test)] mod tests { ... }   ← unit tests HERE
│   ├── grid.rs
│   │   └── #[cfg(test)] mod tests { ... }
│   └── ...
├── paint/
│   └── ... (same pattern)
├── style/
│   └── ...
└── ...

tests/
├── integration.rs          ← ONE main integration test binary
├── allocation_failure.rs   ← special: custom global allocator
├── common/                 ← shared test utilities (NOT a binary)
│   └── mod.rs
├── api/                    ← public API tests
│   ├── mod.rs
│   └── ...
├── fixtures/               ← data-driven fixture tests
│   ├── mod.rs
│   ├── runner.rs
│   └── html/
│       └── *.html
└── wpt/                    ← web platform tests
    ├── mod.rs
    └── ...
```

**Result: 2 test binaries instead of 60. Seconds to link instead of minutes.**

---

## The Rules

### Rule 1: Unit tests go in `src/`, not `tests/`

If a test:
- Tests internal/private logic
- Imports internal modules (`use crate::layout::*`)
- Could benefit from accessing private functions
- Tests a specific function or module's behavior

Then it is a **unit test** and belongs in `src/` next to the code:

```rust
// src/layout/flex.rs
pub(crate) fn compute_flex_basis(...) -> ... { ... }

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn flex_basis_auto() { ... }
}
```

**DO NOT** put unit tests in `tests/`. This is the primary mistake being fixed.

### Rule 2: Integration tests are for the public API only

`tests/` is **exclusively** for:
- Testing the public `FastRender` API as an external consumer would
- Data-driven tests that run external fixtures (HTML files, WPT)
- Tests requiring special harnesses (custom allocator, process isolation)

If you find yourself importing internal modules in `tests/`, **stop** — that test belongs in `src/`.

### Rule 3: One integration test binary (with exceptions)

All integration tests go in `tests/integration.rs` which includes modules:

```rust
// tests/integration.rs
mod common;
mod api;
mod fixtures;
mod wpt;
```

**Exceptions** (separate binaries) are ONLY for:
- `allocation_failure.rs` — needs `#[global_allocator]`
- Tests that modify process-global state irreversibly

Every other test goes in `integration.rs`. No exceptions.

### Rule 4: No `#[path = ...]` shims

The pattern:
```rust
// tests/some_specific_test.rs
#[path = "layout/some_specific_test.rs"]
mod some_specific_test;
```

**DELETE ALL OF THESE.** They exist because someone thought you need a separate binary to run one test in isolation. You don't:

```bash
# This already works:
bash scripts/cargo_agent.sh test --test integration layout::some_specific_test
```

The `#[path = ...]` pattern creates a second binary for zero benefit.

### Rule 5: No top-level test code in harness files

Files like `animation_tests.rs` with 2600 lines of actual test code are **wrong**. The harness file should be:

```rust
// tests/integration.rs (or old: tests/animation_tests.rs)
mod animation;  // That's it. 1 line.
```

All test code goes in the subdirectory (`tests/animation/` or `src/animation/`).

---

## Phase 1: Inventory and Classification

Before moving anything, classify every test:

### 1.1 Identify unit tests (move to `src/`)

A test is a unit test if it:
- Tests a specific function or module
- Imports internal crate modules
- Would benefit from accessing private items
- Doesn't need external fixtures

**These constitute ~90% of tests in `tests/layout/`, `tests/style/`, `tests/paint/`, etc.**

### 1.2 Identify true integration tests (keep in `tests/`)

A test is an integration test if it:
- Tests the public `FastRender` API end-to-end
- Runs external HTML fixtures through the renderer
- Verifies behavior from a consumer's perspective

### 1.3 Identify special-harness tests (separate binary)

Tests that need:
- Custom `#[global_allocator]`
- Process forking / exec
- Global state that can't be reset

### 1.4 Document the classification

Create a tracking file or spreadsheet:

| File | Type | Destination | Notes |
|------|------|-------------|-------|
| `tests/layout/flex_wrap.rs` | unit | `src/layout/flex.rs` | Tests internal flex logic |
| `tests/fixtures_test.rs` | integration | `tests/fixtures/` | Fixture runner |
| `tests/allocation_failure/` | special | keep | Custom allocator harness module for `tests/allocation_failure.rs` |

---

## Phase 2: Prepare `src/` for unit tests

### 2.1 Add `#[cfg(test)]` modules to source files

For each source file that will receive tests:

```rust
// src/layout/flex.rs

// ... existing code ...

#[cfg(test)]
mod tests {
    use super::*;
    
    // Tests will go here
}
```

### 2.2 Expose necessary items for testing

Some items may need visibility changes:
- `pub(crate)` for cross-module test access
- Keep truly private things private — test them indirectly or through the module's `tests` submodule

**DO NOT** make things `pub` just for testing. Use `pub(crate)` or test from within the module.

### 2.3 Create test utilities in `src/`

```rust
// src/test_utils.rs (or src/testing/mod.rs)
#![cfg(test)]

pub fn create_test_dom(html: &str) -> Dom { ... }
pub fn assert_layout_matches(node: &BoxNode, expected: &str) { ... }
```

Import with `#[cfg(test)] use crate::test_utils::*;`

---

## Phase 3: Move unit tests to `src/`

### 3.1 Migration pattern

For each test file in `tests/`:

**Before:**
```rust
// tests/layout/flex_wrap.rs
use fastrender::layout::flex::*;
use fastrender::style::*;

#[test]
fn flex_wrap_reverse() {
    // test code
}
```

**After:**
```rust
// src/layout/flex.rs
pub fn compute_flex_layout(...) { ... }

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn flex_wrap_reverse() {
        // same test code, now with access to privates
    }
}
```

### 3.2 Handle cross-module tests

Some tests exercise multiple modules together. Options:

1. **Put in the "primary" module** — the one most central to what's being tested
2. **Create a `src/integration_tests.rs`** module for cross-cutting unit tests:
   ```rust
   // src/lib.rs
   #[cfg(test)]
   mod integration_tests;
   
   // src/integration_tests.rs
   use crate::layout::*;
   use crate::style::*;
   
   #[test]
   fn style_affects_layout() { ... }
   ```

### 3.3 Preserve test names and structure

Keep test function names identical during migration. This preserves git blame and makes verification easier.

### 3.4 Verify each migration

After moving tests from a module:

```bash
# Before: tests ran via
bash scripts/cargo_agent.sh test --test integration layout::flex_wrap

# After: tests run via
bash scripts/cargo_agent.sh test --lib flex_wrap

# Verify same tests exist
bash scripts/cargo_agent.sh test --lib -- --list | grep flex_wrap
```

---

## Phase 4: Consolidate `tests/` to single binary

### 4.1 Create the unified integration test

```rust
// tests/integration.rs
//! FastRender integration tests.
//!
//! These tests exercise the public API and external fixtures.
//! Unit tests belong in src/, not here.

mod common;
mod api;
mod fixtures;
mod wpt;
```

### 4.2 Create module structure

```
tests/
├── integration.rs
├── common/
│   ├── mod.rs
│   ├── assertions.rs
│   └── fixtures.rs
├── api/
│   ├── mod.rs
│   ├── render.rs
│   └── options.rs
├── fixtures/
│   ├── mod.rs
│   ├── runner.rs
│   └── html/
│       └── *.html
└── wpt/
    ├── mod.rs
    └── runner.rs
```

### 4.3 Move remaining integration tests

For tests that legitimately belong in `tests/`:

1. Identify which module they belong in (`api/`, `fixtures/`, etc.)
2. Move the test code to appropriate file in that module
3. Update the module's `mod.rs` to include it

### 4.4 Delete old harness files

Once all tests are moved:

```bash
# Delete all the old top-level test files
rm -f tests/layout_tests.rs
rm -f tests/paint_tests.rs
rm -f tests/style_tests.rs
# ... etc
```

**Verify nothing broke:**
```bash
bash scripts/cargo_agent.sh test --test integration
bash scripts/cargo_agent.sh test --lib
```

---

## Phase 5: Delete the `#[path = ...]` shims

### 5.1 Identify all shims

```bash
# Any `#[path = "..."]` attribute under tests/ is a shim.
rg -n '^\s*#\[\s*path\s*=\s*"' tests/
```

### 5.2 Delete them

These files serve no purpose. The tests they reference are already in subdirectories and can be run with:

```bash
bash scripts/cargo_agent.sh test --test integration specific_test_name
```

**Just delete the shim files.** No migration needed — the actual tests remain in their subdirectories.

---

## Phase 6: Handle special cases

### 6.1 Allocation failure tests

Keep `tests/allocation_failure.rs` as a separate binary:

```rust
// tests/allocation_failure.rs
#![cfg(not(miri))]  // if needed
// Note: `mod allocation_failure;` is ambiguous here because it would match both:
// - this crate root (`tests/allocation_failure.rs`), and
// - the harness module directory (`tests/allocation_failure/mod.rs`).
//
// We avoid `#[path = ...]` shims by using `include!` instead.
mod allocation_failure {
    include!("allocation_failure/mod.rs");
}
```

### 6.2 Fixture tests

The fixture runner stays in `tests/` because it:
- Reads external HTML files
- Tests the public rendering API
- Is legitimately an integration test

```rust
// tests/fixtures/runner.rs
use fastrender::FastRender;
use std::fs;

#[test]
fn run_all_fixtures() {
    for entry in fs::read_dir("tests/fixtures/html").unwrap() {
        let path = entry.unwrap().path();
        run_fixture(&path);
    }
}
```

Consider using `datatest-stable` or `libtest-mimic` for parallel fixture execution with individual test reporting.

### 6.3 WPT (Web Platform Tests)

Similar to fixtures — stays in `tests/` as integration tests:

```rust
// tests/wpt/mod.rs
mod runner;
mod expectations;
```

---

## Phase 7: Update documentation and CI

### 7.1 Update AGENTS.md

Replace the current "Test organization" section with:

```markdown
## Test organization

### Unit tests: `src/`

Unit tests go in `src/` alongside the code they test:

\`\`\`rust
// src/layout/flex.rs
pub fn compute_flex(...) { ... }

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_flex_wrap() { ... }
}
\`\`\`

Run with: `cargo test --lib`

### Integration tests: `tests/`

Integration tests in `tests/` are ONLY for:
- Public API tests (testing `FastRender` as a consumer would)
- Fixture runners (HTML files, WPT)
- Special harnesses (custom allocator)

There are exactly 2 test binaries:
- `tests/integration.rs` — all normal integration tests
- `tests/allocation_failure.rs` — custom allocator tests

**NEVER create new `tests/*.rs` files.** Add to existing modules.

Run with: `cargo test --test integration`
```

### 7.2 Add CI check

Add a CI step that fails if `tests/*.rs` count exceeds threshold:

```bash
#!/bin/bash
 MAX_TEST_BINARIES=2
COUNT=$(ls tests/*.rs 2>/dev/null | wc -l)
if [ "$COUNT" -gt "$MAX_TEST_BINARIES" ]; then
    echo "ERROR: Found $COUNT test binaries, max is $MAX_TEST_BINARIES"
    echo "Unit tests go in src/, not tests/"
    exit 1
fi
```

### 7.3 Update test commands in scripts

Update `scripts/cargo_agent.sh` and any CI scripts:

```bash
# Old (broken)
bash scripts/cargo_agent.sh test --test <suite_specific_test_binary>

# New (correct)
bash scripts/cargo_agent.sh test --lib           # unit tests
bash scripts/cargo_agent.sh test --test integration  # integration tests
```

---

## What NOT To Do

### DO NOT create new `tests/*.rs` files

Every new file is a new binary. There is no reason to ever add one.

### DO NOT use `#[path = ...]` for "isolation"

You can already run specific tests:
```bash
bash scripts/cargo_agent.sh test --test integration specific::test::name
bash scripts/cargo_agent.sh test --lib specific::test::name
```

### DO NOT put unit tests in `tests/`

If you're importing internal modules, it's a unit test. Put it in `src/`.

### DO NOT make things `pub` just for testing

Use `pub(crate)` or test from within the module's `#[cfg(test)]` block.

### DO NOT keep "harness" files with code in them

Files like `animation_tests.rs` with 2600 lines are wrong. Harness files should be 1-5 lines:
```rust
mod animation;
```

### DO NOT preserve backwards compatibility

The old structure is wrong. Delete it completely. There is no migration path that preserves the old binaries — they all go away.

### DO NOT do this incrementally "to be safe"

Incremental migration leaves the codebase in a worse hybrid state. Do it all at once per phase, verify, move on.

---

## Verification Checklist

After completion:

- [ ] `ls tests/*.rs | wc -l` returns **2**
- [ ] `cargo test --lib` runs all unit tests
- [ ] `cargo test --test integration` runs all integration tests  
- [ ] `cargo test` completes in < 2 minutes (was 5+ minutes)
- [ ] No `#[path = ...]` patterns in `tests/`
- [ ] No internal module imports in `tests/` files
- [ ] AGENTS.md updated with new test organization
- [ ] CI enforces test binary count limit

---

## Timeline Estimate

| Phase | Effort | Description |
|-------|--------|-------------|
| 1. Inventory | 2-4 hours | Classify all tests |
| 2. Prepare src/ | 2-4 hours | Add `#[cfg(test)]` blocks |
| 3. Move unit tests | 8-16 hours | Bulk of the work |
| 4. Consolidate tests/ | 2-4 hours | Create integration.rs |
| 5. Delete shims | 30 min | Just delete files |
| 6. Special cases | 1-2 hours | Fixtures, WPT, allocation |
| 7. Documentation | 1-2 hours | Update docs and CI |

**Total: 2-4 days of focused work.**

The payoff is permanent: every future `cargo test` runs in seconds instead of minutes.
