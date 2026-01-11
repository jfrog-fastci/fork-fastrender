# LLVM 18 `place-safepoints` crash + strategy (native-js GC safepoints)

On Ubuntu LLVM **18.1.3** (the default toolchain in this repo), the LLVM pass
`place-safepoints` *segfaults* when it needs to insert **poll** safepoints
(entry/backedge) **and** the module does *not* already declare the poll
function `@gc.safepoint_poll`.

The crash appears to be triggered by `place-safepoints` trying to materialize
the `gc.safepoint_poll` declaration itself.

## Summary / recommendation

- `place-safepoints` **is usable on LLVM 18.1.3** if you apply the workaround:
  ensure every `gc` module predeclares:
  ```llvm
  declare void @gc.safepoint_poll()
  ```
  and ensure the symbol is defined at link time (runtime or per-module wrapper).
  `runtime-native` exports `gc.safepoint_poll` for this purpose.
- To run poll insertion + statepoint rewriting together under the new pass
  manager, use a pipeline that explicitly runs `place-safepoints` as a function
  pass, e.g.:
  ```bash
  opt-18 -S -passes='function(place-safepoints),rewrite-statepoints-for-gc' ...
  ```
- **Performance note:** the default `place-safepoints` scheme inserts poll
  *calls* which become statepoints (stack map emission) after
  `rewrite-statepoints-for-gc`. If we want the “~1 instruction when GC inactive”
  behavior from `EXEC.plan.md`, we should still implement an explicit fast poll
  (load+branch) in IR and only call into the runtime on the slow path.

So the practical strategy for native-js is:

1. **Correctness first / easiest:** predeclare `@gc.safepoint_poll()` and use
   `place-safepoints` + `rewrite-statepoints-for-gc`.
2. **Low overhead polling:** implement explicit fast polls in compiler IR and
   rely on `rewrite-statepoints-for-gc` to statepoint only the slow-path call.

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

Workaround: add `declare void @gc.safepoint_poll()` to the module before running
the pass.

## `opt-18` repro: backedge poll insertion also crashes

Input: `docs/llvm_place_safepoints_llvm18_repro_backedge.ll`

Even if we disable entry safepoints, the pass still crashes when it needs to
insert a backedge poll:

```bash
opt-18 -S -passes=place-safepoints -spp-no-entry \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_backedge.ll \
  -o /tmp/out.ll
```

Workarounds:

```bash
# Avoids the crash, but also prevents poll insertion (defeats the purpose).
opt-18 -S -passes=place-safepoints -spp-no-entry -spp-no-backedge \
  vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_backedge.ll \
  -o /tmp/out.ll
```

```bash
# Real workaround: predeclare the poll function and keep backedge polls enabled.
#   declare void @gc.safepoint_poll()
opt-18 -S -passes=place-safepoints -spp-no-entry \
  /tmp/your_module_with_gc_safepoint_poll_decl.ll \
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

If the input module predeclares `@gc.safepoint_poll()` then
`LLVMRunPasses(..., \"place-safepoints\", ...)` also works, matching the `opt-18`
workaround.

### Rust wrapper (native-js)

`native-js` includes a small Rust helper that runs the safepoint pipeline via
`LLVMRunPasses` and applies the `@gc.safepoint_poll()` predeclaration workaround:

- `native-js/src/llvm_passes.rs`:
  - `ensure_gc_safepoint_poll_decl`
  - `run_place_safepoints_and_rewrite_statepoints_for_gc`

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
   `gc "coreclr"` in this repo).
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

- Works on LLVM 18.1.3 today (no dependence on `place-safepoints` quirks).
- Fast path overhead is a load+branch, not a function call.
- Statepoint is only executed on the slow path, i.e. only when a GC is actually
  requested.
