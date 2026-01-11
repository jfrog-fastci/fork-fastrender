# LLVM StackMap v3 reproducers (LLVM 18, x86_64)

This directory contains **minimal LLVM IR** files used to empirically trigger
specific StackMap v3 encodings (primarily for `gc.statepoint`, but also for
`llvm.experimental.stackmap` / patchpoints).

Most files can be inspected with:

```bash
llvm-as-18 <file>.ll -o <file>.bc
opt-18 -passes=rewrite-statepoints-for-gc <file>.bc -o <file>.sp.bc   # statepoint examples only
llc-18 -O2 -filetype=obj <file>.sp.bc -o <file>.o                    # or <file>.bc for non-statepoint examples
llvm-readobj-18 --stackmap <file>.o
llvm-readobj-18 -x .llvm_stackmaps <file>.o
```

Some reproducers require extra `llc` flags; see file headers and
`docs/llvm_stackmaps.md`.

## Statepoint-focused reproducers (`rewrite-statepoints-for-gc`)

| File | What it demonstrates |
|---|---|
| `statepoint_gc_live_register.ll` | `LocationKind=1 (Register)` for a `gc-live` root when enabling `llc -fixup-allow-gcptr-in-csr -max-registers-for-gc-values=1`. |
| `statepoint_deopt_direct.ll` | `LocationKind=2 (Direct)` via recording a stack address as a deopt operand. |
| `statepoint_deopt_constantindex.ll` | `LocationKind=5 (ConstantIndex)` via a >32-bit deopt constant (uses constant pool). |
| `statepoint_deopt_mixed.ll` | Mixed-size deopt operands (`Size` can be 4/8/16; `Indirect` offsets may be unaligned). |
| `statepoint_gc_transition_flags.ll` | meta location `#2` (flags) becomes non-zero (`1`) when using `"gc-transition"`. |
| `statepoint_two_statepoints.ll` | Multiple statepoints in one function; useful for experimenting with `-fixup-max-csr-statepoints`. |
| `statepoint_meta_callconv_fastcc.ll` | meta location `#1` encodes callsite calling convention (`fastcc` => 8). |

## Non-statepoint stackmap / patchpoint reproducers

| File | What it demonstrates |
|---|---|
| `stackmap_register.ll` | `LocationKind=1 (Register)` via `llvm.experimental.stackmap` (integer register). |
| `stackmap_register_xmm0.ll` | `LocationKind=1 (Register)` via `llvm.experimental.stackmap` in an XMM register (`xmm0`, DWARF reg 17). |
| `patchpoint_liveouts.ll` | `LiveOuts[]` list encoding (StackMap record live-outs from `llvm.experimental.patchpoint`). |

