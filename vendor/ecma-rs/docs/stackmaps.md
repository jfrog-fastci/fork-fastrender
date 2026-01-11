# Stackmaps (LLVM statepoints) — runtime assumptions

This project uses **LLVM statepoints** (`rewrite-statepoints-for-gc` → `gc.statepoint` + `.llvm_stackmaps`) to implement precise GC stack scanning.

Our initial native runtime stack-walking strategy is intentionally simple:

- **Walk frames via the RBP chain** (frame pointers).
- **Compute each frame's RSP** from the ABI + the known frame layout.
- **Do not** use `libunwind`, `ucontext`, or DWARF register reconstruction.

That strategy is only correct if every GC root referenced by a stackmap is stored in a **memory location**.

## Contract: no `Register` GC roots

At every statepoint, LLVM emits a stackmap record with a list of live GC pointer locations.

We require:

- All GC pointer locations in `.llvm_stackmaps` are **addressable stack slots**
  (LLVM StackMaps `Indirect` locations).
  - In particular, GC roots must **not** be `Register` locations.

Rationale:

- Without an unwind-based register context, we cannot read or update register-held roots for non-top frames.
- A moving/compacting GC must be able to update *all* roots, not just the topmost frame.

## Required codegen options (LLVM 18, x86_64)

LLVM *can* place statepoint GC roots in callee-saved registers under some settings.
The runtime has a verifier (`runtime-native/src/statepoint_verify.rs`) that rejects such stackmaps in
debug builds (and optionally in release builds via the `verify-statepoints` feature).

To force spills, ensure codegen uses:

- `llc-18 --fixup-max-csr-statepoints=0`

This keeps statepoint GC roots out of registers even if other GC register options are enabled.

Additionally, stack walking requires frame pointers:

- `frame-pointer="all"` (LLVM function attribute), or `llc-18 --frame-pointer=all`

## Troubleshooting

If you see a stackmap entry like:

```
#4: Register R#12, size: 8
```

then one of the following is true:

1. Codegen did not pass `--fixup-max-csr-statepoints=0`, or
2. LLVM changed behavior / we upgraded LLVM and need to re-evaluate defaults.

Run the regression suite:

```
cargo test -p runtime-native --test statepoint_register_roots_codegen
```

The tests:

- compile a matrix of IR functions with 0–64 GC roots,
- run `opt-18 -passes=rewrite-statepoints-for-gc` + `llc-18`,
- parse the resulting `.llvm_stackmaps` section,
- and assert (via the runtime verifier) that GC roots are not reported as `Register`.
