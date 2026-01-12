# GC Stackmaps: Why We Need Stack Walking (Unwinding)

LLVM's statepoint-based GC support (`llvm.experimental.gc.statepoint`) emits a `.llvm_stackmaps`
section describing where GC references live at each safepoint.

Two details matter for the runtime:

1. **Stackmap records are keyed by the _return address_**  
    The "instruction offset" in a stackmap record refers to the address of the instruction *after*
    the safepoint call (i.e. the callsite return address).

2. **Most stack locations are `Indirect [SP/FP + off]`**  
    When LLVM needs a GC ref to be in memory at a safepoint, stackmaps describe it as an indirect
    stack slot relative to either `SP` or `FP` (most commonly `SP`).

    Critically, when the base register is `SP`, that `SP` value is the **caller frame's stack pointer
    at the return address**, not the callee's current `SP`.

    **x86_64 note (SP-based locations):** `call` pushes an 8-byte return address. If a thread is
    stopped *inside* the safepoint callee, the callee-entry `RSP` points at that return address and is
    therefore **8 bytes lower** than the stackmap `SP` base. `runtime-native` captures/publishes the
    **post-call** SP for stackmap evaluation (`sp = sp_entry + 8`).

## Implication: a GC must unwind

Stop-the-world GC commonly stops threads *inside* the safepoint slow path (e.g. inside
`rt_gc_safepoint_slow`), but the stackmap we need to interpret is for the *caller* frame (the
managed callsite):

- The stackmap record we need is keyed by the **return address back into the caller**.
- The stack slots we must scan are described as `Indirect [SP/FP + off]` relative to the **caller**'s
  `SP`/`FP` at that return address.

Therefore the runtime needs a reliable way to unwind a thread's stack and compute, per frame:

- `return_address` (caller PC after the call)
- `caller_sp` (caller stack pointer at that return address)

## Current policy: frame-pointer walking (required)

For the first milestone we use **frame-pointer walking** on Linux:

- x86_64: `FP = RBP`, return address at `[FP + 8]`, caller SP = `FP + 16`
- AArch64: `FP = X29`, return address at `[FP + 8]` (saved LR), caller SP = `FP + 16`

This only works if **all code that can run on GC-managed threads keeps frame pointers**.

### Do not reconstruct callsite SP from stackmap `stack_size`

StackMap function records include a `stack_size` field, but it is a **fixed per-function frame
size**:

- It can be **unknown** (`u64::MAX`) for dynamic stack frames (e.g. variable-size `alloca`).
- Even when it is known, it does **not** reliably account for per-call adjustments at a particular
  callsite (notably outgoing stack arguments on x86_64 SysV), so it is not a safe way to recover the
  callsite stack pointer used by stackmaps.

Instead, under the forced-frame-pointer contract we recover the stackmap SP base directly from the
**callee** frame pointer:

```text
caller_sp_callsite = callee_fp + 16
```

This holds for both x86_64 SysV (`RBP`) and AArch64 (`X29`) and matches the caller’s stack pointer
value at the stackmap record PC (the return address).

Note: `stack_size` can still be useful for **offline inspection** (e.g. normalizing some locations
into FP-relative offsets), but the runtime stack walker must not depend on it for reconstructing the
callsite `SP`.

### Enforcement

- **Generated LLVM code** must be compiled with frame pointers:
  - `llc -frame-pointer=all` (or equivalent target options/attributes).
- **Generated LLVM code** should preferably keep statepoint GC roots in *addressable stack slots*
  (`Indirect [SP/FP + off]`):
  - This keeps stackmaps easier to inspect/debug and avoids relying on register-root preservation.
  - `runtime-native` supports `Register` roots by saving a full register file at safepoints and
    treating registers as mutable lvalues, but spills are still preferred.
  - To encourage spills, use `llc -fixup-allow-gcptr-in-csr=false` (preferred) and/or
    `llc -fixup-max-csr-statepoints=0`.
  - See `docs/stackmaps.md` for supported location kinds and constraints.
- **Rust runtime code** must be compiled with frame pointers:
  - `RUSTFLAGS="-C force-frame-pointers=yes"`  
    The repo's LLVM wrapper script (`scripts/cargo_llvm.sh`) sets this automatically.

Future work may add a DWARF/`libunwind` fallback, but precise GC currently assumes frame pointers
are enabled.
