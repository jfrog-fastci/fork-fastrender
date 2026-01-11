# LLVM GC strategy choice (`native-js`)

LLVM selects a GC lowering “strategy” on a per-function basis via:

```llvm
define void @foo() gc "<strategy>" { ... }
```

`native-js` relies on LLVM **statepoints** (`rewrite-statepoints-for-gc`) and the emitted
`.llvm_stackmaps` metadata for precise GC. To avoid mismatches between modules and future LLVM
breakage, we standardize on **one** strategy name for all generated code.

## Candidates in LLVM 18

LLVM 18 ships multiple built-in strategies that support statepoints and stackmaps:

- `gc "coreclr"`: production strategy used by the CoreCLR/.NET toolchain.
- `gc "statepoint-example"`: demo/reference strategy (not production-hardened).

## LLVM 18.1.3 observations relevant to us

### GC pointer address space (`addrspace(1)`)

When running `rewrite-statepoints-for-gc` (LLVM 18.1.3), **only pointers in `addrspace(1)`** are
treated as GC references (“gc-live”) and relocated:

- `ptr addrspace(1)` values show up in the `"gc-live"` bundle and get a corresponding
  `llvm.experimental.gc.relocate.*` in the rewritten IR.
- `ptr` (addrspace(0)) values do **not** get tracked/relocated, even if they are live across a call.

This behavior is the same for both `coreclr` and `statepoint-example`.

### Safepoint polls and lowering

`rewrite-statepoints-for-gc` rewrites **existing calls** into statepoints; it does not insert loop
polls.

To insert entry/backedge polls, LLVM provides the `place-safepoints` pass. On Ubuntu LLVM
**18.1.3**, `place-safepoints` can segfault if it needs to materialize the poll function
declaration itself; predeclaring the poll function avoids the crash:

```llvm
declare void @gc.safepoint_poll()
```

`native-js` supports running the combined pipeline
`function(place-safepoints),rewrite-statepoints-for-gc` and applies this predeclaration workaround
automatically.

For performance, we still prefer compiler-emitted “fast polls” (load+branch with a slow-path call
into the runtime) so the common case is ~1-2 instructions when GC is inactive. See
`vendor/ecma-rs/docs/llvm_place_safepoints_llvm18.md`.

This is independent of the chosen strategy name.

### Stackmap emission

After rewriting to `llvm.experimental.gc.statepoint.*`, both strategies cause LLVM to emit a
`.llvm_stackmaps` section during codegen. (The runtime decodes these in `runtime-native`.)

## Decision: use `gc "coreclr"`

We default to **`coreclr`** because it is the production-used strategy and therefore less likely to
change incompatibly or be removed. `statepoint-example` is explicitly a demonstration strategy.

## Where it is configured / how to change

The strategy name is centralized in:

- `native-js/src/llvm/gc.rs` (`GC_STRATEGY`)

To change it:

1. Update `GC_STRATEGY`.
2. Update the regression tests that assert `gc "coreclr"` appears in emitted IR.
3. Update any documentation/fixtures that embed the old strategy string.
