# ecma-rs Ownership Principle

**FastRender owns ecma-rs. Period.**

This is not a dependency relationship. This is not "upstream vs downstream." FastRender can and should modify `vendor/ecma-rs/` directly for anything needed to ship browser JavaScript support.

---

## The Rule

**All JavaScript/WebIDL infrastructure belongs in `vendor/ecma-rs/`, not in parallel FastRender crates.**

For a contributor-facing overview of the consolidated WebIDL crate layout (and where new code should
go), see [`docs/webidl_stack.md`](webidl_stack.md).

If you need functionality that doesn't exist in ecma-rs, **add it to ecma-rs**.  
If ecma-rs code doesn't work for FastRender's needs, **change ecma-rs**.  
If workspace configuration makes this awkward, **fix the workspace configuration**.

---

## Anti-patterns (FORBIDDEN)

### ❌ Creating parallel JS/WebIDL infrastructure crates outside `vendor/ecma-rs/`

**Wrong:** putting a second copy of WebIDL infrastructure under `crates/` (or anywhere else in this
repo) when the canonical implementation lives in:

- `vendor/ecma-rs/webidl/`
- `vendor/ecma-rs/webidl-vm-js/`
- `vendor/ecma-rs/webidl-runtime/`

This creates maintenance burden, sync overhead, and divergence. Every line in a parallel crate is
technical debt.

Note: a transitional workspace-local copy of the legacy heap-only runtime previously existed as a
`webidl-js-runtime` crate under `crates/`, but it has been removed. Do not re-introduce it; modify
`vendor/ecma-rs/webidl-runtime/` directly.
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

FastRender-specific DOM/Web API bindings integration → `src/js/webidl/`  
Browser event loop integration → `src/js/event_loop.rs`  
HTML script scheduling/processing scaffolding → `src/js/html_script_scheduler.rs` + `src/js/html_script_pipeline.rs`

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
| WebIDL IR + algorithms | `vendor/ecma-rs/webidl/` | Spec infrastructure |
| WebIDL ↔ vm-js adapter | `vendor/ecma-rs/webidl-vm-js/` | Engine integration |
| Legacy heap-only WebIDL runtime adapter | `vendor/ecma-rs/webidl-runtime/` | Compatibility layer |
| DOM/Web API bindings integration (FastRender) | `src/js/webidl/` | FastRender-specific glue |
| Browser APIs (timers, fetch, URL, ...) | `src/js/` + `src/web/` | FastRender-specific |
| Event loop | `src/js/event_loop.rs` | FastRender-specific |
| Script scheduling/processing | `src/js/html_script_scheduler.rs` + `src/js/html_script_pipeline.rs` | FastRender-specific |

---

## Repository shape (post-consolidation)

`crates/` should be reserved for FastRender-specific tooling (currently `crates/js-wpt-dom-runner/`).
No JS/WebIDL infrastructure crates should exist there.

All generic JS/WebIDL infrastructure lives in `vendor/ecma-rs/`. If you find yourself reaching for
`crates/` to add WebIDL parsing/conversions/VM integration, that is almost certainly a design
mistake: put it in ecma-rs instead.

---

## Checklist for new JS/WebIDL work

Before creating any new code:

1. **Is this JavaScript language infrastructure?** → Put it in `vendor/ecma-rs/`
2. **Is this WebIDL spec implementation?** → Put it in `vendor/ecma-rs/webidl*/`
3. **Is this specific to FastRender's DOM?** → Put it in `src/js/`
4. **Am I about to create a new JS/WebIDL infrastructure crate in `crates/`?** → **STOP.** This is almost certainly wrong.

---

## No new exceptions

There are no valid reasons to:
- Create new parallel JS/WebIDL infrastructure crates in `crates/`
- Introduce or extend "workspace-local copies" of ecma-rs crates
- Avoid modifying ecma-rs "because it's separate"

If you think you have an exception, you don't. Fix the underlying issue instead.
