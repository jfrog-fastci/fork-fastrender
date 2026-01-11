# LLVM 18 Statepoint StackMap ABI (Regression-Tested)

This project relies on **LLVM 18** `gc.statepoint` stack maps for **precise GC root scanning**. The LLVM stackmap format is not a stable, documented ABI, so we codify the specific behaviors we depend on and **regression-test them**.

The automated check is:

- `vendor/ecma-rs/scripts/test_stackmap_abi.sh`
- Fixture IR: `vendor/ecma-rs/fixtures/llvm_stackmap_abi/statepoint.ll`
- `vendor/ecma-rs/scripts/test_statepoint_flags_patchbytes.sh`
- Fixture IR:
  - `vendor/ecma-rs/fixtures/llvm_stackmap_abi/gc_statepoint_patch_bytes_0_flags_0.ll`
  - `vendor/ecma-rs/fixtures/llvm_stackmap_abi/gc_statepoint_patch_bytes_16_flags_3.ll`

See also:

- `docs/llvm_stackmaps.md` — StackMap v3 binary decoding notes (LocationKind encoding, record layout, etc.).

## Correct LLVM 18 textual IR shape

LLVM 18 requires the `@llvm.experimental.gc.statepoint` call to include **both**:

- `i32 num_transition_args` (must be `0`; inline transition args are deprecated)
- `i32 num_deopt_args`

and they appear **after the call arguments**.

Minimal pattern (matches our fixture):

```llvm
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

define void @stackmap_abi_callee(ptr addrspace(1) %p) { ret void }

define ptr addrspace(1) @stackmap_abi_test(ptr addrspace(1) %obj)
    gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...)
      @llvm.experimental.gc.statepoint.p0(
        i64 0, i32 0,
        ptr elementtype(void (ptr addrspace(1))) @stackmap_abi_callee,
        i32 1, i32 0,
        ptr addrspace(1) %obj, ; call args...
        i32 0,                ; num_transition_args (MUST be 0)
        i32 0                 ; num_deopt_args
      ) [ "gc-live"(ptr addrspace(1) %obj) ]

  %rel = call coldcc ptr addrspace(1)
      @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  ret ptr addrspace(1) %rel
}
```

Notes:

- With opaque pointers, LLVM 18 requires the statepoint **callee operand** to include an `elementtype(...)` annotation (otherwise the IR verifier rejects it).
- We use the `"gc-live"` **operand bundle** to list pointers that must be reported in the stack map.
- **Important:** LLVM does not implicitly treat `ptr addrspace(1)` call arguments as GC roots for stack maps. Any GC pointer passed as a call argument must also appear in `"gc-live"` or it may be missing from the emitted stackmap record.
- **Also important:** LLVM’s emitted stackmap “GC locations” are driven by `gc.relocate` uses. A `"gc-live"` entry with no corresponding `gc.relocate` will not produce a (base, derived) location pair in `.llvm_stackmaps`.

## GC pointers must use a GC address space

LLVM’s statepoint infrastructure expects GC references to be distinguishable from non-GC pointers.

In our pipeline we use a dedicated GC address space; the fixture demonstrates that:

- a GC pointer can be represented as `ptr addrspace(1)`
- `gc.relocate` must return the **same GC pointer type**, so the intrinsic must be the matching overload (e.g. `@llvm.experimental.gc.relocate.p1` returning `ptr addrspace(1)`).

## StackMap record key: return address (next instruction)

Empirically on LLVM 18.1.x (tested on Ubuntu LLVM 18.1.8), each `gc.statepoint` produces a stackmap record keyed by the **return address**:

- `llvm-readobj --stackmap` reports an `instruction offset`
- that offset corresponds to the **next instruction** (i.e. the address the CPU would return to)

This is the lookup key used by stack walkers that match frames via their return PCs.

Important nuance: `gc.statepoint` supports **patchable call sites** via the `patch_bytes` argument.
When `patch_bytes > 0` on x86_64, LLVM 18 emits a NOP sled instead of an actual call instruction, and the stackmap instruction offset points to the end of that reserved region (the "return address" if/when a call is patched in).
This means a runtime patcher must ensure the *actual* call return address matches the stackmap key (i.e. returns to the end of the reserved region), not just to the byte after the patched call instruction.
Equivalently, the reserved patch region starts at:
`patch_region_start = instruction_offset - patch_bytes`
and ends at `instruction_offset`.
LLVM may still emit additional NOP padding outside that reserved region (e.g. for
alignment), so the instruction at `instruction_offset` can itself be a NOP.

## `gc.statepoint`: `flags` is a 2-bit mask on LLVM 18

`gc.statepoint` takes a `flags` immarg as its 5th argument.

On LLVM 18.x, the IR verifier only accepts `flags` values in the range **0..3** (bits 0 and 1).
Any value with bit 2 set (e.g. `flags = 4`) is rejected as an unknown flag.

Project default: use `flags = 0` unless a specific flag is required.

On x86_64 LLVM 18, `llvm-readobj --stackmap` surfaces this value as the second
constant header location in each record (location `#2`), immediately after the
callconv header location `#1` (the statepoint callsite calling convention ID,
usually `0` for `ccc`).
Depending on whether LLVM chooses to use the stackmap constant pool, `#2` may be
printed as either `Constant <flags>` or `ConstIndex`/`ConstantIndex ... (<flags>)`;
the value must match the IR `flags` immarg either way.

On 64-bit targets, LLVM prints the statepoint header constants (`#1` callconv,
`#2` flags, `#3` deopt_count) with `size: 8` even though `flags`/`deopt_count` are
`i32` operands in IR.

## `gc.statepoint`: `patch_bytes > 0` reserves a patchable region (x86_64)

`patch_bytes` is the 2nd argument to `gc.statepoint`.

- `patch_bytes = 0`: LLVM emits a normal call instruction.
- `patch_bytes > 0`: LLVM reserves a patchable region at the statepoint site.
  On x86_64 (LLVM 18.1.x), this becomes a NOP sled and shifts the stackmap instruction offset forward accordingly.

## Stack slot base register: caller-frame SP

On LLVM 18.1.x, spilled stack roots are reported as `Indirect [...]` locations based on the **stack pointer in the caller’s frame**, using DWARF register numbers:

- x86_64: `RSP` (DWARF reg `7`)
- AArch64: `SP` (DWARF reg `31`)

Our regression test asserts we see at least one `Indirect [SP + ...]` / `Indirect [R#31 + ...]` on AArch64 and `Indirect [RSP + ...]` / `Indirect [R#7 + ...]` on x86_64.

## StackMap register numbers are DWARF register numbers

All register identifiers printed in stackmap records (e.g. `R#7`, `R#31`) are DWARF register numbers for the target, not LLVM’s internal register IDs.
