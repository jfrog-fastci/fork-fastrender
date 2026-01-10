# `webidl-vm-js` (FastRender)

This crate provides the `vm-js` adapter for the `webidl` conversion/runtime traits.

## Why does this exist when `vendor/ecma-rs/` already contains `webidl-vm-js`?

FastRender vendors `ecma-rs` under `vendor/ecma-rs/`, and upstream `ecma-rs` includes a
`webidl-vm-js` crate. FastRender keeps a **workspace-local copy** at `crates/webidl-vm-js` so the
adapter can be used like a normal FastRender crate without pulling the entire vendored `ecma-rs`
workspace into FastRender’s workspace.

FastRender may also carry small embedder-specific adjustments here (for example: using
host-aware `Vm::call*` paths when WebIDL helpers need to call into JS (iterator protocol, callback
invocation, object coercions) so embedder `VmHostHooks` and (when available) embedder host context
are preserved.

In particular, `VmJsWebIdlCx` has a `from_native_call` constructor intended for use inside `vm-js`
`NativeCall`/`NativeConstruct` handlers, so conversions that call back into JS do **not** route
through `Vm::call_without_host` (which bypasses embedder hook overrides).

## Syncing with `ecma-rs`

When updating `vendor/ecma-rs`, check whether `vendor/ecma-rs/webidl-vm-js/` changed and port the
relevant changes into `crates/webidl-vm-js/` (keeping any FastRender-specific patches).

Validate with:

```bash
bash scripts/cargo_agent.sh test -p webidl-vm-js
```

Do **not** depend on `vendor/ecma-rs/webidl-vm-js` directly from FastRender; use this crate.
