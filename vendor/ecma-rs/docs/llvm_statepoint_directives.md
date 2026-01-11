# LLVM 18 statepoint directive callsite attributes

LLVM 18’s `RewriteStatepointsForGC` pass (`opt-18 -passes=rewrite-statepoints-for-gc`) rewrites calls in GC-managed
functions into `@llvm.experimental.gc.statepoint.*` intrinsics. By default, the pass assigns a constant ID to each
rewritten statepoint:

- Default statepoint ID: `0xABCDEF00` (`2882400000`)
- Default patch bytes: `0`
- (If you encounter deopt bundles) default deopt ID: `0xABCDEF0F`

LLVM also supports **overriding the emitted statepoint ID and patch-byte directive** by attaching *callsite string
attributes* to the original `call`/`invoke` instruction (before running `rewrite-statepoints-for-gc`):

```llvm
call void @bar() #0

attributes #0 = { "statepoint-id"="42" "statepoint-num-patch-bytes"="16" }
```

## Directive attributes

These are **string attributes** (not enum attributes):

- `"statepoint-id"="<u64>"`
  - Controls the first `i64` argument to `@llvm.experimental.gc.statepoint.*`.
  - This value is also emitted as the StackMap record **patchpoint ID** (the leading `u64` in the
    `.llvm_stackmaps` record), so setting it is useful when you want deterministic/unique StackMap
    IDs instead of LLVM’s default constant.
- `"statepoint-num-patch-bytes"="<u32>"`
  - Controls the second `i32` argument to `@llvm.experimental.gc.statepoint.*`.
  - On x86_64 (LLVM 18.1.x), `patch_bytes=0` emits a real `call`, while
    `patch_bytes>0` reserves a patchable region (NOP sled) and shifts the stackmap
    `instruction offset` to the end of that reserved region.
    The reserved region start offset is `instruction_offset - patch_bytes`.
    Any runtime patcher must ensure the call return address matches that end-of-region address.

## native-js support

`native-js` does **not** assign statepoint directive attributes by default. Consumers can opt into
deterministic/unique IDs by attaching them before running `rewrite-statepoints-for-gc`.

`native-js` exposes helpers to attach these callsite attributes via the LLVM C API:

- `native_js::llvm::statepoint_directives::set_callsite_statepoint_id(call, id)`
- `native_js::llvm::statepoint_directives::set_callsite_statepoint_num_patch_bytes(call, bytes)`

Additionally, `native-js` includes an **opt-in** module annotator (behind the `statepoint-directives` cargo feature)
that assigns deterministic/unique sequential IDs to callsites in GC-managed functions:

- `native_js::llvm::statepoint_directives::assign_statepoint_ids(module, start)`

This annotator is intended to run before invoking `rewrite-statepoints-for-gc`, so the resulting rewritten
`gc.statepoint` IDs are stable across builds.

Enable it with:

```bash
bash scripts/cargo_llvm.sh test -p native-js --features statepoint-directives
```
