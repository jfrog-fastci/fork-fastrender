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

### Stackmap SP reconstruction (AArch64-specific)

LLVM stackmaps for `gc.statepoint` usually describe stack roots as:

```
Indirect [SP + off]
```

For AArch64 (LLVM 18, `llc -frame-pointer=all`) we empirically observe:

- `DWARF_SP_REG = 31` (`SP`)
- `DWARF_FP_REG = 29` (`X29`)
- StackMap function `stack_size` is the **total** stack-pointer delta for the frame.
- The fixed `{fp, lr}` save area is always **16 bytes**.

This lets the runtime reconstruct the `SP` value used as the base for stackmap locations from
`FP` + `stack_size`:

```
sp_at_safepoint = fp - (stack_size - 16)
               = (fp + 16) - stack_size
```

The frame record layout remains:

- next FP at `[fp + 0]`
- return PC (saved LR) at `[fp + 8]`

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
