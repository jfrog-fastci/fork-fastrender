# GC stack walking invariants (`native-js`)

`native-js` will eventually generate optimized native code using LLVM and integrate a **precise GC**
via LLVM **statepoints**.

Even with LLVM stack maps, the runtime still needs a way to:

1. Walk the stack frame-by-frame.
2. Recover each frame's **return address / instruction pointer**.
3. Use that return address to locate the matching stack map record and enumerate GC roots.

Note: LLVM 18 `gc.statepoint` supports reserving a patchable callsite region via
the `patch_bytes` argument. When `patch_bytes > 0` on x86_64, LLVM emits a NOP
sled and the stackmap record key (`instruction offset`) points to the *end* of
that reserved region. Any runtime patcher must ensure the call return address
matches that end-of-region address, otherwise stackmap lookup by return PC will
fail.
The reserved patch region start offset is `instruction_offset - patch_bytes`.

## Current strategy: frame-pointer chain (Linux x86_64 + AArch64)

While bringing up statepoints and precise GC, we take the simplest and most deterministic approach:
make the stack trivially walkable by following the platform frame chain:

- x86_64: `rbp` (DWARF reg 6)
- AArch64: `x29` (DWARF reg 29)

To enforce that invariant, every `native-js` generated function has the following LLVM function
attributes:

- `frame-pointer="all"`
  - Forces LLVM to preserve frame pointers in **all** functions.
  - Allows stack walking by following the platform frame pointer chain.
- `disable-tail-calls="true"`
  - Disables tail-call optimization so calls do not elide frames.
  - Prevents sibling-call and other tail-call lowering that would otherwise remove frames and
    confuse a frame-chain based walker.

This is intentionally conservative: it trades some performance for correctness and simplicity while
the GC is being implemented.

## Moving to unwind-table based walking later

Frame pointers cost a register and can reduce optimization headroom. To eventually remove the
frame-pointer requirement, we would need to move to unwinding-based stack walking:

- Ensure unwind information is emitted for all generated code (`uwtable` / `-funwind-tables`) so
  `.eh_frame` contains DWARF CFI for every function.
- Implement a robust unwinder in the runtime (e.g. via `libunwind`) to iterate frames and recover
  return addresses.
- Teach the GC's stack walker to use the unwinder's frame/IP information to locate the correct
  stack map records.

Until then, **frame pointers + no tail calls** are treated as a hard invariant.

## Dynamic stack adjustments (`alloca`, stack realignment)

LLVM StackMap function records include a `stack_size` field. When a function performs **dynamic**
stack adjustments (e.g. variable-size `alloca` or stack realignment), LLVM 18 may emit
`stack_size = -1` (encoded as `u64::MAX`) to indicate the frame size is unknown.

This matters because many stackmap GC roots are described as `Indirect [SP + off]`, which requires
knowing the caller's stack pointer at the safepoint callsite:

- The runtime treats this sentinel value as "unknown".
- When frame pointers are enforced, stack walking does not need `stack_size` to recover a callsite
  stack pointer:
  - for non-top frames it derives the caller's callsite SP from the callee frame pointer
    (`caller_sp_callsite = callee_fp + 16`),
  - for the top managed frame it captures the caller SP at safepoint entry.

Codegen policy: **avoid dynamic `alloca` / stack realignment in functions that participate in GC
statepoints/safepoints**. Prefer heap allocation or fixed-size stack slots. FP-relative root
locations may still work in practice, but are not guaranteed across LLVM versions/opts.

## GC root locations: stack slots preferred, register roots supported

LLVM stackmaps can describe live values as either:

- addressable stack slots (`Indirect [SP/FP + off]`), or
- registers (`Register R#N`, encoded as DWARF register numbers).

`runtime-native` prefers `Indirect` spill slots (they are easier to inspect/debug and do not depend
on register preservation), but it also supports `Register` GC roots by capturing a full register
file at safepoints and treating each register slot in that saved `RegContext` as a mutable lvalue
during stack scanning/relocation.

To reduce register roots in practice:

- `native-js` configures LLVM codegen globally via `LLVMParseCommandLineOptions` (see
  `native-js/src/llvm/mod.rs`) using:
  - `--fixup-allow-gcptr-in-csr=false` (preferred)
  - `--fixup-max-csr-statepoints=0` (defense-in-depth)
- When codegen happens out-of-process (e.g. `llc-18` or `clang-18 -flto`), pass the equivalent
  backend flags:
  - `llc-18 --fixup-allow-gcptr-in-csr=false --fixup-max-csr-statepoints=0`
  - `clang-18 -mllvm --fixup-allow-gcptr-in-csr=false -mllvm --fixup-max-csr-statepoints=0`
- `runtime-native` has a verifier (`runtime-native/src/statepoint_verify.rs`) that validates root
  location kinds and catches obviously-invalid root encodings (e.g. SP/FP/IP register roots).

See `vendor/ecma-rs/docs/stackmaps.md` for the full contract and regression tests around
register-root avoidance.
