# `webidl-vm-js` (legacy shim)

This crate provides the `vm-js` adapter for the `webidl` conversion/runtime traits.

FastRender’s canonical implementation lives in `vendor/ecma-rs/webidl-vm-js/`.

This workspace-local crate remains only as a compatibility layer while the repository finishes
migrating away from the old in-tree WebIDL stack. Avoid adding new dependencies on it; prefer the
vendored crate for new code.
