# Stackmaps (LLVM statepoints) — runtime assumptions

This project uses **LLVM statepoints** (`rewrite-statepoints-for-gc` → `gc.statepoint` + `.llvm_stackmaps`) to implement precise GC stack scanning.

Our initial native runtime stack-walking strategy is intentionally simple:

- **Walk frames via the frame-pointer chain** (frame pointers):
  - x86_64: `RBP`
  - AArch64: `X29`
- **Compute each frame's callsite stack pointer (SP)** from the ABI + the known frame layout (needed
  for stackmap locations based on `SP`).
- **Do not** use `libunwind`, `ucontext`, or DWARF register reconstruction.

That strategy is only correct if every GC root referenced by a stackmap is **addressable** while the
thread is stopped at a safepoint (either in a spilled stack slot, or in a register that the runtime
can read and rewrite via a saved register context).

## Stackmap `SP` base is the *callsite* SP (not callee-entry SP)

LLVM StackMap `Indirect [SP + off]` locations use the **caller frame's SP at the stackmap record PC**
(the instruction *after* the call). (LLVM may also emit `Indirect [FP + off]` locations; those are
evaluated from the frame pointer chain directly.)

On **x86_64**, `call` pushes an 8-byte return address, so if a thread is stopped inside the safepoint
callee, the callee-entry `RSP` points at the return address and is **8 bytes lower** than the stackmap
`SP` base. `runtime-native` therefore publishes a *post-call* SP for stackmap evaluation
(`sp = sp_entry + 8`).

When unwinding via the frame-pointer chain, the runtime recovers the caller’s stackmap SP base from
the **callee** frame pointer:

```text
caller_sp_callsite = callee_fp + 16
```

This is the same on x86_64 SysV (`RBP`) and AArch64 (`X29`) when frame pointers are enabled.

Important: do **not** try to reconstruct callsite SP from the stackmap function record’s
`stack_size`. `stack_size` is a fixed per-function frame size and can be wrong at callsites with
per-call stack adjustments (notably outgoing stack arguments on x86_64).

## `.llvm_stackmaps` can contain multiple StackMap v3 blobs

LLVM emits a complete StackMap v3 table into each object file’s `.llvm_stackmaps` section.
When linking multiple objects, ELF linkers concatenate those section payloads, producing
**multiple independent StackMap v3 blobs back-to-back**, each starting with its own `version=3`
header.

Linkers may also insert alignment padding between concatenated payloads (usually 0x00), and some
toolchains have been observed to leave short non-zero “tail” bytes between blobs.

This means runtime code must not assume `.llvm_stackmaps` is a single global header + tables or that
blobs are perfectly packed without padding.

Runtime-native provides helpers that handle both cases:

- Use `runtime_native::stackmaps::StackMaps::parse(bytes)` (preferred) when parsing a linked
  image’s `.llvm_stackmaps` section; it iterates all blobs and builds one callsite index.
- `runtime_native::stackmaps::StackMap::parse(bytes)` parses a **single** StackMap v3 blob and
  will fail fast if it looks like the input contains multiple concatenated blobs.

## Contract: GC root locations must be addressable (`Indirect` or `Register`)

At every statepoint, LLVM emits a stackmap record with a list of live GC pointer locations.

`runtime-native` supports two location kinds for GC roots:

- **Spilled stack slots** (`Indirect [SP/FP + off]`) (preferred)
  - The runtime computes the slot address from the caller-frame SP/FP at the statepoint callsite.
- **Register roots** (`Register R#N`, DWARF register numbers)
  - The runtime captures a full register file (`RegContext`) at the safepoint and treats each
    register as a **mutable lvalue** inside that saved register file.
  - This allows a moving GC to relocate pointers by rewriting the saved register slots, then
    restoring registers when the thread resumes.

Register-root constraints:

- Location must be pointer-sized.
- `offset` must be `0` (the StackMap v3 `offset` field is semantically unused for `Register`).
- SP/FP/IP DWARF registers are rejected (they are not GC roots under our frame-pointer policy).
- The DWARF register number must be supported by the runtime's saved `RegContext` for the target.

### Correctness note: register roots in older frames

Stack scanning for older frames uses the **current** register file saved at the safepoint.
A stackmap `Register` root is therefore only meaningful for registers whose values are preserved
across the call stack at the safepoint (typically callee-saved registers, or registers explicitly
spilled/preserved by statepoint lowering).

LLVM's StackMap / statepoint semantics guarantee that any value described as a `Register` GC root is
live and recoverable at the safepoint; `runtime-native` simply reads and (for relocation) updates
the corresponding slot in the saved `RegContext`.

## Recommended codegen options (LLVM 18, x86_64 + AArch64)

LLVM *can* place statepoint GC roots in callee-saved registers under some settings.
The runtime supports register roots, but keeping roots in stack slots tends to make stackmaps easier
to debug and reduces reliance on register-file capture.

To encourage spills, prefer:

- `llc-18 --fixup-allow-gcptr-in-csr=false` (preferred), and/or
- `llc-18 --fixup-max-csr-statepoints=0` (fallback / defense-in-depth)

This keeps statepoint GC roots out of registers even if other GC register options are enabled.

### `clang -flto` note

If machine code generation happens inside `clang-18` (e.g. `clang-18 -flto`), pass the equivalent
backend flag:

- `clang-18 -mllvm --fixup-allow-gcptr-in-csr=false -mllvm --fixup-max-csr-statepoints=0`

`native-js`'s LTO linking helpers do this automatically.

Additionally, stack walking requires frame pointers:

- `frame-pointer="all"` (LLVM function attribute), or `llc-18 --frame-pointer=all`

## Troubleshooting

If you see a stackmap entry like:

```
#4: Register R#12, size: 8
```

then LLVM kept a GC value in a register at that statepoint callsite.

This is supported by `runtime-native`, but it may indicate one of the following:

1. Codegen did not pass `--fixup-max-csr-statepoints=0`, or
    (and/or did not set `--fixup-allow-gcptr-in-csr=false`), or
2. LLVM changed behavior / we upgraded LLVM and need to re-evaluate defaults.

Run the regression suite:

```
bash scripts/cargo_llvm.sh test -p runtime-native --test statepoint_register_roots_codegen
```

The tests:

- compile a matrix of IR functions with 0–64 GC roots,
- run `opt-18 -passes=rewrite-statepoints-for-gc` + `llc-18`,
- parse the resulting `.llvm_stackmaps` section,
- and assert (via the runtime verifier) that GC roots are not reported as `Register`.

## Base/Derived pointer pairs (derived / interior pointers)

LLVM statepoints encode GC relocation information as *(base, derived)* pairs:

- **base**: the address of the GC-managed object
- **derived**: the value actually in machine state (may be an interior pointer)

For non-interior pointers, LLVM typically emits **duplicate** base/derived locations (`base == derived`).

For interior pointers (`base != derived`), a moving collector must update the derived value to preserve the
interior offset:

```text
base_new    = relocate(base_old)
derived_new = base_new + (derived_old - base_old)
```

Important: LLVM can reuse the *same base spill slot* across multiple pairs when multiple derived
pointers share a base (and the base itself may also appear as `base == derived`). Derived relocation
must therefore be performed **in a batch** (per frame): snapshot old base/derived values first, then
write relocated bases, then write relocated derived values using the snapshotted deltas.

Use `runtime_native::relocate_derived_pairs` to relocate a per-frame batch of `(base_slot, derived_slot)` pairs
safely even when base slots repeat.

Null convention:

- If `base_old == 0` or `derived_old == 0`, the derived value stays null (`derived_new = 0`).
- If the GC relocator returns `base_new == 0` for a non-null base (should not happen for live
  objects), the derived value is forced to `0` to keep the pair consistent.

`runtime-native` implements this during stack walking:

- The stack walker visits only the **base** root slots (deduplicated) and lets the GC relocate them in-place.
- After relocating a base slot, it updates any derived slots that reference it using the formula above.

See `runtime-native/src/stackwalk_fp.rs` and `runtime-native/tests/stackwalk_fp.rs`.

### Stackmap location layout for `gc.statepoint` (LLVM 18)

For LLVM 18 statepoints lowered by `rewrite-statepoints-for-gc`, stackmap record `locations` follow a predictable
structure:

1. **3 constant header locations**:
   - `callconv` (call convention ID; commonly `0` for C, `8` for `fastcc`)
   - `flags` (the `gc.statepoint` `flags` immarg; a 2-bit mask `0..=3`; `1` when a `"gc-transition"` operand bundle is present)
   - `deopt_count` (number of `"deopt"` operand locations; GC ignores these but must skip them)
2. Then `deopt_count` deopt operand locations (not GC roots; can be `Constant`, `Indirect`, etc.).
3. Then **2 locations per `gc.relocate` call**: `(base, derived)`

`runtime-native::statepoints::StatepointRecord` enforces this layout (`LLVM18_STATEPOINT_HEADER_CONSTANTS = 3`),
provides `gc_pairs() -> &[GcLocationPair]` for iterating the base/derived relocation pairs, and
`runtime-native::stackwalk_fp::walk_gc_root_pairs_from_fp` can translate those locations into
`(base_slot, derived_slot)` spill-slot pairs for each frame.
