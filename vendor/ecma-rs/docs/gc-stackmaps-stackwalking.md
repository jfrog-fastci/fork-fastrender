# GC Stackmaps: Why We Need Stack Walking (Unwinding)

LLVM's statepoint-based GC support (`llvm.experimental.gc.statepoint`) emits a `.llvm_stackmaps`
section describing where GC references live at each safepoint.

Two details matter for the runtime:

1. **Stackmap records are keyed by the _return address_**  
    The "instruction offset" in a stackmap record refers to the address of the instruction *after*
    the safepoint call (i.e. the callsite return address).

2. **Most stack locations are `Indirect [SP + off]`**  
    When LLVM needs a GC ref to be in memory at a safepoint, stackmaps typically describe it as
    an indirect address relative to `SP`.

    Critically, this `SP` is the **caller frame's stack pointer at the return address**, not the
    callee's current `SP`.

    **x86_64 note:** `call` pushes an 8-byte return address. If a thread is stopped *inside* the
    safepoint callee, the callee-entry `RSP` points at that return address and is therefore **8 bytes
    lower** than the stackmap `SP` base. `runtime-native` captures/publishes the **post-call** SP for
    stackmap evaluation (`sp = sp_entry + 8`).

## Implication: a GC must unwind

Stop-the-world GC commonly stops threads *inside* the safepoint slow path (e.g. inside
`rt_gc_safepoint_slow`), but the stackmap we need to interpret is for the *caller* frame (the
managed callsite):

- The stackmap record we need is keyed by the **return address back into the caller**.
- The stack slots we must scan are relative to the **caller**'s `SP` at that return address.

Therefore the runtime needs a reliable way to unwind a thread's stack and compute, per frame:

- `return_address` (caller PC after the call)
- `caller_sp` (caller stack pointer at that return address)

## Current policy: frame-pointer walking (required)

For the first milestone we use **frame-pointer walking** on Linux:

- x86_64: `FP = RBP`, return address at `[FP + 8]`, caller SP = `FP + 16`
- AArch64: `FP = X29`, return address at `[FP + 8]` (saved LR), caller SP = `FP + 16`

This only works if **all code that can run on GC-managed threads keeps frame pointers**.

### Stackmap `stack_size` caveat (`Indirect [SP + off]` locations)

LLVM stackmaps for `gc.statepoint` usually describe stack roots as:

```
Indirect [SP + off]
```

In LLVM StackMaps, `SP` is the **caller**'s stack pointer value at the stackmap record PC (the
callsite return address), not the callee's current `SP`.

The stackmap function record also includes a fixed `stack_size`, which is sometimes used to
normalize SP-relative slots into FP-relative offsets when inspecting stackmaps offline. However,
`stack_size` does **not** account for per-callsite stack adjustments (e.g. outgoing stack argument
pushes), so it is not reliable for reconstructing the exact callsite `SP` in general.

`runtime-native` avoids this by deriving the callsite `SP` directly from the callee frame pointer
when walking frames:

```
caller_sp_callsite = callee_fp + 16
```

Frame record layout remains:

- next FP at `[fp + 0]`
- return PC at `[fp + 8]`

### Enforcement

- **Generated LLVM code** must be compiled with frame pointers:
  - `llc -frame-pointer=all` (or equivalent target options/attributes).
- **Generated LLVM code** must also keep statepoint GC roots in *addressable stack slots* (not registers):
  - `llc -fixup-allow-gcptr-in-csr=false` (preferred) and/or `llc -fixup-max-csr-statepoints=0`
  - See `docs/stackmaps.md` for the full “no Register roots” contract and regression tests.
- **Rust runtime code** must be compiled with frame pointers:
  - `RUSTFLAGS="-C force-frame-pointers=yes"`  
    The repo's LLVM wrapper script (`scripts/cargo_llvm.sh`) sets this automatically.

Future work may add a DWARF/`libunwind` fallback, but precise GC currently assumes frame pointers
are enabled.
