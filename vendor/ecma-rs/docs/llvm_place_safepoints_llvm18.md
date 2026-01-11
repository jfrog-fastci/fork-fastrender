# LLVM 18 `place-safepoints` crash + strategy (native-js GC safepoints)

On Ubuntu LLVM **18.1.3** (the default toolchain in this repo), the new-PM pass
`place-safepoints` segfaults for any function that has a `gc "<strategy>"`
attribute *when the pass needs to insert safepoint polls* (entry/backedge).

This blocks relying on `place-safepoints` for loop safepoint polling.

## Summary / recommendation

- **Do not use `place-safepoints` on LLVM 18.1.3**. It segfaults in both `opt-18`
  and when invoked via the LLVM C API `LLVMRunPasses`.
- Use `rewrite-statepoints-for-gc` to turn **calls** into statepoints.
- Implement **safepoint polling explicitly in IR generation**:
  - Insert a fast poll at loop backedges (load+branch).
  - Only the slow path calls a runtime function (e.g. `rt_gc_safepoint()`), which
    `rewrite-statepoints-for-gc` will rewrite into a statepoint.

This yields the desired “~1–2 instructions when GC inactive” behavior without
depending on `place-safepoints`.

## `opt-18` repro: entry poll insertion crashes

Input: `docs/llvm_place_safepoints_llvm18_repro_entry.ll`

Command:

```bash
opt-18 -S -passes=place-safepoints \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_entry.ll \
  -o /tmp/out.ll
```

Expected: the pass inserts an entry poll safepoint.

Actual (LLVM 18.1.3): segfault, with backtrace showing:

- `llvm::PlaceSafepointsPass::runImpl`
- `llvm::CallInst::CallInst(...)`

## `opt-18` repro: backedge poll insertion also crashes

Input: `docs/llvm_place_safepoints_llvm18_repro_backedge.ll`

Even if we disable entry safepoints, the pass still crashes when it needs to
insert a backedge poll:

```bash
opt-18 -S -passes=place-safepoints -spp-no-entry \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_backedge.ll \
  -o /tmp/out.ll
```

Workaround attempt:

```bash
# Avoids the crash, but also prevents poll insertion (defeats the purpose).
opt-18 -S -passes=place-safepoints -spp-no-entry -spp-no-backedge \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_backedge.ll \
  -o /tmp/out.ll
```

## LLVM C API repro (`LLVMRunPasses`)

`vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_runpasses.c` is a tiny driver
that parses IR and runs a new-PM pipeline via `LLVMRunPasses`.

Build:

```bash
clang-18 vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_runpasses.c \
  -o /tmp/llvm_run_passes \
  $(llvm-config-18 --cflags --ldflags --libs core passes irreader native --system-libs)
```

Run (crashes):

```bash
/tmp/llvm_run_passes place-safepoints \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_entry.ll
```

Sanity check (works; prints statepoints):

```bash
/tmp/llvm_run_passes rewrite-statepoints-for-gc \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_call.ll \
  > /tmp/out.ll
```

This suggests the issue is in the pass itself (not just the `opt` driver).

## Legacy pass manager routes

- `opt-18` (LLVM 18) does not expose a legacy `-place-safepoints` style flag; it
  only accepts the new-PM `-passes=...` pipeline syntax.
- The Ubuntu LLVM 18 development headers also do not ship the legacy
  `llvm-c/Transforms/*.h` headers (e.g. `llvm-c/Transforms/Scalar.h`), so there
  is no obvious C API entry point like `LLVMAddPlaceSafepointsPass(...)` to try.

In practice this means: **there is no usable legacy-PM escape hatch** here.

## `rewrite-statepoints-for-gc` works for callsites (but does not insert polls)

Calls in `gc` functions are rewritten into `llvm.experimental.gc.statepoint.*`
without needing `place-safepoints`:

```bash
opt-18 -S -passes=rewrite-statepoints-for-gc \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_call.ll \
  -o /tmp/out.ll
```

However, for tight loops with no calls, `rewrite-statepoints-for-gc` **does not**
insert polling safepoints. (It only rewrites existing calls.)

## Recommended strategy for native-js on LLVM 18

1. Mark generated functions with `gc "<strategy>"` (we currently use
   `gc "statepoint-example"` in the repros).
2. Run `rewrite-statepoints-for-gc` to convert callsites to statepoints.
3. For loop polling, have the compiler explicitly emit an IR poll:
   - Fast path: load a global/TLS “GC requested” flag and branch.
   - Slow path: call `rt_gc_safepoint()` (or similar) which triggers/joins GC.
   - The slow-path call becomes a statepoint, so stack maps are correct when GC
     actually runs.

Example IR template: `vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_manual_poll.ll`

```bash
opt-18 -S -passes=rewrite-statepoints-for-gc \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_manual_poll.ll \
  -o /tmp/out.ll
```

### Why this is preferable

- Works on LLVM 18.1.3 today (no crashing pass).
- Fast path overhead is a load+branch, not a function call.
- Statepoint is only executed on the slow path, i.e. only when a GC is actually
  requested.
