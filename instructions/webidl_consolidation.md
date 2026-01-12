# WebIDL Stack Consolidation

This document describes the consolidation of FastRender's parallel WebIDL infrastructure into `vendor/ecma-rs/`.

**Status: REQUIRED WORK**

---

## Problem Statement

FastRender has built a parallel WebIDL stack in `crates/` that is 5x larger than ecma-rs's WebIDL:

| Location | Lines | Purpose |
|----------|-------|---------|
| `vendor/ecma-rs/webidl/` | ~1.7K | Core WebIDL traits, conversions |
| `vendor/ecma-rs/webidl-vm-js/` | ~1.0K | vm-js adapter |
| `crates/webidl-ir/` | ~2.2K | WebIDL IR, parsing |
| `crates/webidl-bindings-core/` | ~3.6K | Runtime traits, conversions |
| `crates/webidl-vm-js/` | ~4.8K | Extended vm-js adapter |
| `crates/webidl-js-runtime/` | ~7.0K | Actual runtime impl |

**Total `crates/webidl*`: ~18K lines** — technical debt that must be merged into ecma-rs.

---

## Target State

After consolidation:

```
vendor/ecma-rs/
├── webidl/
│   ├── src/
│   │   ├── lib.rs          ← unified exports
│   │   ├── ir/             ← from crates/webidl-ir
│   │   │   ├── mod.rs
│   │   │   ├── parser.rs
│   │   │   ├── types.rs
│   │   │   └── ...
│   │   ├── runtime/        ← from crates/webidl-bindings-core/runtime.rs
│   │   ├── conversions.rs  ← merged conversions
│   │   ├── overload.rs     ← merged overload resolution
│   │   └── ...
│   └── Cargo.toml
├── webidl-vm-js/
│   ├── src/
│   │   ├── lib.rs          ← merged from crates/webidl-vm-js
│   │   └── ...
│   └── Cargo.toml
└── webidl-runtime/         ← NEW: from crates/webidl-js-runtime
    ├── src/
    │   └── ...
    └── Cargo.toml

crates/
└── (DELETED - only js-wpt-dom-runner remains as FastRender-specific tool)
```

FastRender's `src/js/webidl/` continues to hold **FastRender-specific** DOM bindings and browser integration.

---

## Phase 1: Merge `webidl-ir` into `vendor/ecma-rs/webidl`

### What moves

| Source | Target |
|--------|--------|
| `crates/webidl-ir/src/parser.rs` | `vendor/ecma-rs/webidl/src/ir/parser.rs` |
| `crates/webidl-ir/src/idl_type.rs` | `vendor/ecma-rs/webidl/src/ir/types.rs` |
| `crates/webidl-ir/src/default_value.rs` | `vendor/ecma-rs/webidl/src/ir/default_value.rs` |
| `crates/webidl-ir/src/default_eval.rs` | `vendor/ecma-rs/webidl/src/ir/eval.rs` |
| `crates/webidl-ir/src/value.rs` | `vendor/ecma-rs/webidl/src/ir/value.rs` |

### Steps

1. Create `vendor/ecma-rs/webidl/src/ir/` directory
2. Copy files from `crates/webidl-ir/src/` to `vendor/ecma-rs/webidl/src/ir/`
3. Create `vendor/ecma-rs/webidl/src/ir/mod.rs` with appropriate exports
4. Update `vendor/ecma-rs/webidl/src/lib.rs` to include `pub mod ir;`
5. Update any dependencies in `vendor/ecma-rs/webidl/Cargo.toml`
6. Update FastRender imports from `webidl_ir::*` to `webidl::ir::*`
7. Delete `crates/webidl-ir/`

---

## Phase 2: Merge `webidl-bindings-core` into `vendor/ecma-rs/webidl`

### Analysis needed

Compare:
- `crates/webidl-bindings-core/src/conversions.rs` vs `vendor/ecma-rs/webidl/src/convert.rs`
- `crates/webidl-bindings-core/src/overload_resolution.rs` vs `vendor/ecma-rs/webidl/src/overload.rs`
- `crates/webidl-bindings-core/src/runtime.rs` vs `vendor/ecma-rs/webidl/src/lib.rs` traits

### Steps

1. Identify unique functionality in `webidl-bindings-core` not in ecma-rs `webidl`
2. Merge unique parts into `vendor/ecma-rs/webidl/`
3. Update trait definitions if needed (may require breaking changes)
4. Update all FastRender imports
5. Delete `crates/webidl-bindings-core/`

---

## Phase 3: Merge `webidl-vm-js` extensions

### Analysis needed

`crates/webidl-vm-js/` is 4.8K lines vs `vendor/ecma-rs/webidl-vm-js/` at 1.0K lines.

Understand what the 4x growth provides:
- Extended conversions?
- FastRender-specific hooks?
- Host dispatch integration?

### Steps

1. Diff the two implementations
2. Identify what's generic (belongs in ecma-rs) vs FastRender-specific (stays in `src/`)
3. Merge generic parts into `vendor/ecma-rs/webidl-vm-js/`
4. Move FastRender-specific parts to `src/js/webidl/`
5. Delete `crates/webidl-vm-js/`

---

## Phase 4: Handle `webidl-js-runtime`

### Analysis needed

Is this:
- Generic WebIDL runtime (→ new `vendor/ecma-rs/webidl-runtime/`)
- FastRender-specific (→ merge into `src/js/webidl/`)

### Steps

1. Analyze contents and dependencies
2. Decide target location
3. Move or merge accordingly
4. Delete `crates/webidl-js-runtime/`

---

## Phase 5: Clean up legacy QuickJS crates

The following are legacy and likely unused:
- `crates/js-dom-bindings/`
- `crates/js-dom-bindings-quickjs/`

### Steps

1. Verify nothing depends on them (search codebase)
2. Remove from workspace
3. Delete directories

---

## Phase 6: Update Cargo.toml and workspace

### Steps

1. Remove deleted crates from `[workspace]` members
2. Add ecma-rs crates as workspace dependencies:
   ```toml
   [workspace.dependencies]
   webidl = { path = "vendor/ecma-rs/webidl" }
   webidl-vm-js = { path = "vendor/ecma-rs/webidl-vm-js" }
   vm-js = { path = "vendor/ecma-rs/vm-js" }
   ```
3. Update main `[dependencies]` to use workspace deps
4. Verify build: `timeout -k 10 600 bash scripts/cargo_agent.sh build`
5. Verify tests: `timeout -k 10 600 bash scripts/cargo_agent.sh test --lib`

---

## Verification Checklist

- [ ] `crates/webidl-ir/` deleted, functionality in `vendor/ecma-rs/webidl/src/ir/`
- [ ] `crates/webidl-bindings-core/` deleted, merged into `vendor/ecma-rs/webidl/`
- [ ] `crates/webidl-vm-js/` deleted, merged into `vendor/ecma-rs/webidl-vm-js/`
- [ ] `crates/webidl-js-runtime/` deleted, moved to ecma-rs or `src/js/`
- [ ] `crates/js-dom-bindings/` deleted (legacy)
- [ ] `crates/js-dom-bindings-quickjs/` deleted (legacy)
- [ ] Only `crates/js-wpt-dom-runner/` remains (FastRender-specific tool)
- [ ] `cargo build` succeeds
- [ ] `cargo test --lib` succeeds
- [ ] No `crates/webidl*` references remain in imports

---

## Notes

This consolidation will likely require:
- Breaking changes to trait signatures
- Careful import updates across many files
- Possible API design decisions for merged functionality

Do not attempt incremental "small fixes." The crates are entangled; partial merges create worse hybrid states. Execute phases completely or not at all.
