# LLVM StackMap ABI notes (LLVM 18)

This repository uses LLVM's **stack map** format (the `.llvm_stackmaps` object
section) as the metadata backbone for:

- precise GC safepoints (`llvm.experimental.gc.statepoint`), and
- patchable call sites / deoptimization (`patch_bytes`-style call-site patching).

LLVM's IR and the resulting stack map records have a few sharp edges that are
easy to misunderstand. This document records the *observed* behaviour on **LLVM
18.1.3** (x86_64) and is guarded by a fast regression test:

`scripts/test_statepoint_flags_patchbytes.sh`

---

## `gc.statepoint`: `flags` (argument #5)

`llvm.experimental.gc.statepoint.*` takes a `flags` **`immarg`** as its 5th
argument.

On LLVM **18.x**, the IR verifier only accepts **bitmask values 0..3**:

- Only bits **0** and **1** are currently valid.
- Any value with bit 2 set (i.e. `flags >= 4`) is rejected with an error like:
  `unknown flag used in gc.statepoint flags argument`.

**Project recommendation:** use `flags = 0` unless a specific flag is required
and its semantics are intentionally relied upon.

The regression test additionally compiles a statepoint with `flags = 2` and
asserts that this value is visible in the emitted stack map record.

---

## `gc.statepoint`: `patch_bytes` (argument #2)

`patch_bytes` is the 2nd argument to `gc.statepoint` and controls how LLVM
lowers the safepoint call site:

- `patch_bytes = 0`: LLVM emits a normal `call` instruction.
- `patch_bytes > 0`: LLVM reserves a **patchable region** at the call site.
  On **x86_64**, LLVM 18 emits a **NOP sled** of approximately `patch_bytes`
  bytes (and *does not* emit a direct call).

### Impact on stack map `instruction offset`

Each stack map record includes an `instruction offset`, which conceptually
represents the **return address** for the call site associated with the
statepoint.

On LLVM 18 (x86_64):

- With `patch_bytes = 0`, the `instruction offset` points to the byte *after*
  the `call` instruction.
- With `patch_bytes > 0`, the `instruction offset` points to the byte *after*
  the entire reserved patchable region (the address where execution would resume
  if/when a call is patched in).

This means `patch_bytes` can **shift** the recorded `instruction offset` even
though the statepoint ID is unchanged. Consumers must treat the instruction
offset as "return address", not as "address of a `call` instruction".

