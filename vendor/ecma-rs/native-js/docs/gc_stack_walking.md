# GC stack walking invariants (`native-js`)

`native-js` will eventually generate optimized native code using LLVM and integrate a **precise GC**
via LLVM **statepoints**.

Even with LLVM stack maps, the runtime still needs a way to:

1. Walk the stack frame-by-frame.
2. Recover each frame's **return address / instruction pointer**.
3. Use that return address to locate the matching stack map record and enumerate GC roots.

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
