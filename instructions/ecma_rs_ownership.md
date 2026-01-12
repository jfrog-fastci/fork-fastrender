# ecma-rs Ownership Principle

**FastRender owns ecma-rs. Period.**

This is not a dependency relationship. This is not "upstream vs downstream." FastRender can and should modify `vendor/ecma-rs/` directly for anything needed to ship browser JavaScript support.

---

## The Rule

**All JavaScript/WebIDL infrastructure belongs in `vendor/ecma-rs/`, not in parallel FastRender crates.**

If you need functionality that doesn't exist in ecma-rs, **add it to ecma-rs**.  
If ecma-rs code doesn't work for FastRender's needs, **change ecma-rs**.  
If workspace configuration makes this awkward, **fix the workspace configuration**.

---

## Anti-patterns (FORBIDDEN)

### ❌ Creating parallel crates in `crates/`

**Wrong:**
```
crates/webidl-vm-js/          ← Parallel to vendor/ecma-rs/webidl-vm-js
crates/webidl-bindings-core/  ← Should be in vendor/ecma-rs/webidl
crates/webidl-ir/             ← Should be in vendor/ecma-rs/webidl
```

This creates maintenance burden, sync overhead, and divergence. Every line in a parallel crate is technical debt.

### ❌ "Workspace isolation" as justification

**Wrong reasoning:**
> "We keep a workspace-local copy to avoid pulling ecma-rs into FastRender's workspace"

If workspace configuration is the blocker, fix the workspace configuration. Don't create 15K+ lines of parallel code to avoid a Cargo.toml change.

### ❌ "Small patches" that grow

**Wrong reasoning:**
> "We just need a few FastRender-specific tweaks"

This is how parallel codebases start. One "small adapter" becomes 5x larger than the original. Put the tweaks in ecma-rs directly.

### ❌ Treating ecma-rs as external

**Wrong reasoning:**
> "We shouldn't modify ecma-rs too much, it's a separate project"

No. FastRender owns it. Modify it as much as needed. There is no "too much."

---

## Correct patterns

### ✅ Modify ecma-rs directly

Need WebIDL parsing? Add it to `vendor/ecma-rs/webidl/`.  
Need vm-js integration? Add it to `vendor/ecma-rs/webidl-vm-js/`.  
Need a new crate? Create it in `vendor/ecma-rs/`.

### ✅ Keep FastRender-specific code in `src/`

DOM bindings that specifically bind FastRender's DOM → `src/js/dom/`  
Browser event loop integration → `src/js/event_loop.rs`  
HTML script processing → `src/js/script_processing.rs`

The boundary is:
- **ecma-rs**: JavaScript language, WebIDL spec, engine infrastructure
- **FastRender src/**: FastRender DOM, browser APIs, embedding glue

### ✅ Fix workspace issues at the root

If ecma-rs crates are awkward to depend on:

```toml
# Cargo.toml - workspace dependencies
[workspace.dependencies]
vm-js = { path = "vendor/ecma-rs/vm-js" }
webidl = { path = "vendor/ecma-rs/webidl" }
webidl-vm-js = { path = "vendor/ecma-rs/webidl-vm-js" }
```

This is a one-time fix. Don't create parallel crates to avoid it.

---

## What belongs where

| Code | Location | Rationale |
|------|----------|-----------|
| JS parser | `vendor/ecma-rs/parse-js/` | Language infrastructure |
| JS runtime/VM | `vendor/ecma-rs/vm-js/` | Language infrastructure |
| WebIDL types/traits | `vendor/ecma-rs/webidl/` | Spec infrastructure |
| WebIDL ↔ vm-js adapter | `vendor/ecma-rs/webidl-vm-js/` | Engine integration |
| WebIDL parsing | `vendor/ecma-rs/webidl/` | Spec infrastructure |
| DOM bindings (FastRender DOM) | `src/js/dom/` | FastRender-specific |
| Browser APIs (timers, fetch) | `src/js/web_apis/` | FastRender-specific |
| Event loop | `src/js/event_loop.rs` | FastRender-specific |
| Script processing | `src/js/script_processing.rs` | FastRender-specific |

---

## Migration: Eliminating `crates/`

The `crates/` directory currently contains parallel WebIDL infrastructure that should be merged into ecma-rs:

| Current location | Target location |
|-----------------|-----------------|
| `crates/webidl-ir/` | `vendor/ecma-rs/webidl/` (merge) |
| `crates/webidl-bindings-core/` | `vendor/ecma-rs/webidl/` (merge) |
| `crates/webidl-vm-js/` | `vendor/ecma-rs/webidl-vm-js/` (merge) |
| `crates/webidl-js-runtime/` | `vendor/ecma-rs/webidl-runtime/` (new crate) |
| `crates/js-dom-bindings/` | `src/js/dom/` (move) |
| `crates/js-dom-bindings-quickjs/` | `src/js/dom/quickjs/` (move) |
| `crates/js-wpt-dom-runner/` | `tests/wpt/` or tool (evaluate) |

After migration, `crates/` should be **deleted entirely**.

---

## Checklist for new JS/WebIDL work

Before creating any new code:

1. **Is this JavaScript language infrastructure?** → Put it in `vendor/ecma-rs/`
2. **Is this WebIDL spec implementation?** → Put it in `vendor/ecma-rs/webidl*/`
3. **Is this specific to FastRender's DOM?** → Put it in `src/js/`
4. **Am I about to create a new crate in `crates/`?** → **STOP.** This is almost certainly wrong.

---

## No exceptions

There are no valid reasons to:
- Create parallel crates in `crates/`
- Maintain "workspace-local copies" of ecma-rs crates
- Avoid modifying ecma-rs "because it's separate"

If you think you have an exception, you don't. Fix the underlying issue instead.
