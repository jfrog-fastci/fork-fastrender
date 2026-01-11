# (Deprecated) LLVM Statepoints + `.llvm_stackmaps` ABI

This document is **historical** and may be incorrect for the current implementation.

The up-to-date, developer-facing spec for LLVM 18 statepoints + stackmaps used by `runtime-native`
is:

- `docs/gc_statepoints.md`

In particular, this document predates changes where:

- `runtime-native` enforces SP/FP-based **Indirect** stackmap slots for statepoint roots and supports derived pointers via `(base, derived)` relocation pairs,
- `.llvm_stackmaps` discovery uses the `runtime-native/link/stackmaps*.ld` linker fragments (with `runtime-native/stackmaps.ld` as a compat alias) and stable boundary symbols:
  - `__start_llvm_stackmaps` / `__stop_llvm_stackmaps`
  - `__fastr_stackmaps_{start,end}` / `__llvm_stackmaps_{start,end}` / `__stackmaps_{start,end}` (aliases)
- linking with dead-section GC (`-Wl,--gc-sections`) requires a linker script with `KEEP(*(.llvm_stackmaps ...))` or the section can be discarded entirely
- `native-js` forces statepoint GC roots into stack slots (no stackmap `Register` roots) by setting LLVM codegen options
  such as `--fixup-allow-gcptr-in-csr=false` / `--fixup-max-csr-statepoints=0`.

Later we may relax to allow GC roots in registers for performance, but then the runtime must support register-root relocation
for *all* scanned frames.

---

Authoritative contract between **TS-generated native code** (the `native-js` backend)
and the **Rust runtime / GC** (`runtime-native`) for precise stack scanning using
LLVM statepoints.

This document is intentionally concrete. If any part of the contract is violated,
GC correctness becomes undefined (missed roots, stale pointers, crashes, or
silent corruption).

Target platform for the first implementation:

- **Ubuntu Linux x86_64**, SysV ABI
- LLVM **18**
- Stop-the-world, potentially **moving** GC (compaction allowed)

---

## Terminology

- **TS code**: machine code produced from TypeScript by our `native-js` LLVM backend.
- **Runtime**: Rust code linked into the final binary (allocation, GC, scheduler).
- **Mutator thread**: a thread executing TS code (allocating / mutating the heap).
- **Safepoint**: a program point where a mutator thread can be stopped and its
  roots enumerated precisely.
- **Statepoint**: an LLVM IR `@llvm.experimental.gc.statepoint` intrinsic that
  indicates a call may execute with GC active and must have a corresponding stack
  map entry.
- **Stack map record**: an entry inside `.llvm_stackmaps` describing where live
  values (especially GC references) reside at a safepoint.
- **FP**: frame pointer (`RBP` on x86_64).
- **PC**: program counter / instruction pointer (a code address, `RIP` on x86_64).

---

## 1) What counts as a safepoint

### The only places GC may run

**GC may only run when every mutator thread is parked inside runtime code that
implements a safepoint.**

In other words: we do **cooperative stop-the-world**. A thread is either:

- Running TS code (GC must not move objects), or
- Parked in the runtime at a safepoint (GC may run), having published the state
  needed for stack scanning.

### Which IR constructs must be lowered as statepoints

The `native-js` backend must ensure stack map coverage is complete for all TS
frames that can exist while GC runs.

Minimum required lowering rules:

1. **All TS → TS calls are statepoints**
   - Any direct call from TS code to another TS function must be emitted as
     `gc.statepoint` (not a plain `call`).
   - Rationale: GC may be triggered in the callee (or further down). When GC runs,
     the caller frame is still live, and we must be able to find the caller’s
     roots at exactly this call site.

2. **Explicit safepoint polls are statepoints**
   - **Loop backedge polls are required for GC progress.** Relying only on
     call-site safepoints is insufficient: a tight loop with no calls can block
     stop-the-world GC indefinitely.
   - Long-running loops must contain explicit poll calls, inserted at:
     - loop backedges (minimum), and/or
     - function prologues (for very large leaf functions with no loops).
   - Polls are emitted as calls to a runtime function (e.g. `rt_gc_safepoint` /
     LLVM’s `gc.safepoint_poll`) and must also be expressed as `gc.statepoint` so
     a stack map record exists for the TS frame at that PC.
   - **Relocation contract:** any `ptr addrspace(1)` value that is live across
     the poll may be moved by the GC. The code generator must therefore ensure
     those values appear in the statepoint’s `"gc-live"` bundle and that all uses
     after the poll use the corresponding `gc.relocate` results.

3. (Optional but expected) **Potentially allocating runtime calls are statepoints**
   - Calls from TS code into runtime helpers that may allocate (and therefore may
     request GC) should be treated as safepoints too. This can be achieved either by:
     - lowering TS→runtime calls as statepoints, or
     - having those helpers *not* trigger GC and instead return an error that TS
       handles by calling a poll/collect safepoint (see §5).
   - The initial safe policy is described in §5; until we support scanning Rust
     stacks, the runtime must not run GC while it holds GC pointers in locals.

#### Clarification: “statepoint everywhere”, but “park only at runtime polls”

This ABI uses statepoints for **coverage**, not because we intend to run the GC
at an arbitrary instruction.

In practice:

- Threads **park** only by calling into a dedicated runtime safepoint function
  (e.g. `rt_gc_safepoint`).
- We still lower **all TS→TS calls** as statepoints so that, when a callee (or
  deeper frame) parks for GC, every caller frame has a stack map record that
  accurately describes its live roots at the call site.

### Invariant: “all threads parked”

The GC is allowed to start a moving collection **only if**:

- every mutator thread has reached a safepoint and is parked, and
- each parked thread has published a `(top_ts_fp, top_ts_pc, top_ts_sp)` anchor (see §4).

No “signal-the-world” stack capture is assumed in this initial ABI.

---

## 2) Required codegen flags / invariants

These are non-negotiable constraints on TS-generated machine code.

### 2.1 Frame pointer discipline (stack-walk without DWARF/unwind)

**TS code must be compiled with frame pointers enabled for all functions.**

- LLVM IR must set: `frame-pointer="all"` on all TS functions.
- x86_64 consequence: each TS frame forms a linked list via `RBP`.

We intentionally do **not** rely on DWARF CFI or libunwind to scan stacks.

#### x86_64 SysV frame layout requirement

At every TS frame pointer `fp` (i.e., `RBP`), the runtime assumes:

```
fp + 0x00: saved caller FP (previous RBP)
fp + 0x08: return address (saved RIP)
fp + 0x10: ... locals / spills ...
```

So walking to the caller frame is:

```
fp' = *(fp + 0x00)
pc' = *(fp + 0x08)
```

This implies:

- no frame pointer omission,
- no frame pointer “chaining” tricks,
- no stack switching inside TS code.

### 2.2 Tail calls must be disabled for TS code

**Tail call optimization must not remove TS frames.**

Requirements:

- The `native-js` backend must not emit `tail call` / `musttail` in TS IR.
- The backend must also request that LLVM not perform tail call elimination for
  TS functions (e.g., `disable-tail-calls` / equivalent target options).

Rationale: stack scanning depends on a stable, walkable FP chain. Tail calls can
elide frames and destroy the “return address identifies the safepoint” property.

### 2.3 Stackmaps section must survive to the final binary

LLVM emits stackmap metadata into a dedicated ELF section (`.llvm_stackmaps`, and in this repo often
an output section named `.data.rel.ro.llvm_stackmaps`).

This metadata is **not referenced by code**, so link-time dead-section elimination can discard it.

The runtime reads stackmaps at runtime. Therefore:

- The final linked artifact **must contain** stackmaps bytes (either `.llvm_stackmaps` or the
  repo’s preferred `.data.rel.ro.llvm_stackmaps` output section).
- The section must be readable by the runtime **in memory** (after relocations).
  The simplest way to guarantee this is that `.llvm_stackmaps` is emitted as an
  **allocated** section (ELF `SHF_ALLOC`) and ends up in a loadable segment.
- Build tooling must not strip it (explicit `strip`, `objcopy`, or post-link
  tooling must preserve it).
- When linking with `-Wl,--gc-sections`, the final link step must also apply a linker script fragment
  that `KEEP`s stackmaps; in this repo that is:
  - `runtime-native/link/stackmaps.ld` (preferred) / `runtime-native/stackmaps.ld` (compat)

Verification:

```bash
llvm-readobj --sections <binary> | rg llvm_stackmaps
```

If stripping is required for release binaries, the build must be configured to
keep `.llvm_stackmaps` (exact mechanism is toolchain-dependent; verify with the
command above).

In this repository, you can also run:

```bash
bash scripts/check_llvm_stackmaps.sh
```

Locating the section at runtime is a runtime-linking detail, but the ABI assumes
the runtime can obtain a raw `&[u8]` containing the stack map payload from the
loaded image (i.e. without having to apply relocations itself).

### 2.4 Stack map location restrictions (initial supported subset)

To keep the first runtime implementation small and predictable, we define a
supported subset of LLVM stackmap location forms.

**The current runtime stack scanner only supports GC root locations that are:**

- **8-byte values** (full machine pointer sized), and
- stored in **memory** at a location computed as:
  - location kind = **`Indirect`** (an addressable spill slot), and
  - base register = **SP or FP** (DWARF regnum for stack pointer or frame pointer), and
  - an **immediate signed offset**.

In LLVM stackmap terminology, this corresponds to `Indirect [SP + off]` and
`Indirect [FP + off]` locations.

**Important:** for `Indirect [SP + off]`, LLVM's `SP` is the **caller** stack
pointer value at the stackmap record PC (the callsite return address), not the
callee-entry SP. On x86_64 this differs by 8 bytes because `call` pushes the
return address. The runtime must therefore publish a **post-call** SP for
stackmap evaluation (see `runtime-native/src/arch/mod.rs` and
`docs/gc-stackmaps-stackwalking.md`).

Not supported (codegen must avoid until runtime grows support):

- live GC pointers in registers (`Register` locations)
- non-addressable values (`Direct`, `Constant`, `ConstIndex`) as GC roots
- non-8-byte pointer encodings (compressed pointers, vector lanes, etc.)

The `native-js` backend must treat any emission of unsupported location kinds as
a **codegen bug**.

### 2.5 Stack map contents restrictions (what values are described)

LLVM stack map records can describe arbitrary live values. For GC scanning we
need *only* GC references.

For LLVM 18 `gc.statepoint` records, the stack map `Locations[]` list is
structured and includes non-root metadata:

- 3 leading constant header locations (`callconv`, `flags`, `deopt_count`)
- `deopt_count` deopt operand locations (not GC roots; ignored by the GC)
- then 2 locations per GC-live value: `(base, derived)` pairs corresponding to
  `gc.relocate` results.

The runtime must decode this layout and enumerate GC roots from the base/derived
pairs. Derived (interior) pointers are not traced as independent roots; moving
GC must relocate the base slot and recompute the derived value relative to it.

---

## 3) Runtime stack scanning algorithm

### 3.1 Why the per-frame safepoint PC is a return address

LLVM stack map records for call-based safepoints identify the safepoint by a
code address associated with the call site. On x86_64, the *only reliably
recoverable code address from a stack frame without unwind metadata* is the
**return address** saved by a `call`.

When TS code executes:

```
call <callee>   ; safepoint is associated with this call site
; next instruction here
```

the CPU pushes the address of the next instruction (the return address) onto the
stack. That return address is stored at `fp+8` in the *callee’s* frame.

Therefore:

- The safepoint for a TS frame **F** is identified by the return address stored
  in the frame of the function **F called at the safepoint**.
- When walking the stack, the runtime uses the return address stored in each
  frame to find the stack map record for the *caller*.

This is why the scan uses a `(fp, pc)` pair and updates `pc` from `*(fp+8)` as it
walks upward.

### 3.2 Inputs: the scan anchor

The stack scanner starts from a per-thread anchor published at the moment the
thread parks at a safepoint:

- `top_ts_fp`: frame pointer for the topmost TS frame
- `top_ts_pc`: safepoint PC for that frame (return address back into TS code)
- `top_ts_sp`: stackmap-semantics stack pointer for that frame at `top_ts_pc`
  (the caller SP at the stackmap record PC / callsite return address)

How this anchor is captured is specified in §4.

### 3.3 Frame-walk + root enumeration

The runtime maintains an in-memory lookup structure built from `.llvm_stackmaps`:

```
lookup(pc) -> StackMapRecord | None
```

Where `pc` is a code address (the saved return address).

The scan algorithm is:

```
fp = top_ts_fp
pc = top_ts_pc
sp = top_ts_sp

loop:
  record = lookup(pc)
  if record == None:
    break  // crossed into non-TS frames; stop scanning

  // Statepoints encode GC roots as (base, derived) relocation pairs.
  // For tracing, only the base slots are GC roots; derived slots are interior pointers.
  pairs = decode_statepoint_pairs(record)  // yields (base_loc, derived_loc) pairs

  // Snapshot old values/deltas for derived pointers first (base slots may repeat).
  //
  // Then relocate each unique base slot, then update derived slots relative to the
  // relocated base (derived_new = base_new + (derived_old - base_old)).
  relocate_pairs_in_batch(fp, sp, pairs)

  next_fp = *(fp + 0x00)
  next_pc = *(fp + 0x08)
  next_sp = fp + 0x10  // caller SP at the return address is callee_fp + 16 (forced FP ABI)
  fp = next_fp
  pc = next_pc
  sp = next_sp
```

**Stop condition:** the first `pc` that has no stack map record indicates a
boundary into runtime frames (Rust) or other non-TS code. At that point stack
scanning stops; Rust frames are not scanned (see §5).

### 3.4 Location evaluation (supported subset)

Given a stack map location describing a pointer spill slot as:

- `Kind = Indirect`
- `base_reg = SP` or `FP`
- `offset = k` (signed 32-bit)
- `size = 8`

then:

```
addr = base + k
```

Where:

- if `base_reg == FP`: `base = caller_fp`
- if `base_reg == SP`: `base = caller_sp_callsite`

`caller_sp_callsite` is the caller's stack pointer value at the stackmap record
PC (the callsite return address). When walking frames via frame pointers, this
can be derived from the callee frame pointer as `callee_fp + 16` (x86_64 SysV
and AArch64 with frame pointers enabled). When a thread is stopped inside the
safepoint callee, the runtime captures/publishes this callsite SP directly.

The value at `addr` is treated as a GC pointer (possibly tagged; tagging is part
of the object model and must be applied consistently by codegen + GC).

If a stack map record contains any unsupported location form, the runtime must
fail fast (panic/abort) rather than silently ignore it.

### 3.5 Building `lookup(pc)` from `.llvm_stackmaps` (runtime requirement)

The runtime must parse `.llvm_stackmaps` and build a mapping from **callsite
address** → **stack map record**.

For this ABI, the key used for lookup is:

- `pc`: the return address value read from the stack (`*(fp + 0x08)`), and
- `callsite_addr`: the address encoded in `.llvm_stackmaps` for a record.

**Invariant:** `pc == callsite_addr` for TS safepoints on x86_64. If this is not
true in practice (e.g. LLVM encodes the call instruction address rather than the
return address), the ABI must be updated and the runtime must apply a consistent
adjustment. Until verified, treat mismatches as a bug.

#### Stack map binary format (LLVM “StackMap v3” as of LLVM 18)

All integer fields are little-endian on x86_64.

`stackmap` section layout:

```
Header:
  u8  Version
  u8  Reserved0
  u16 Reserved1
  u32 NumFunctions
  u32 NumConstants
  u32 NumRecords

Function[NumFunctions]:
  u64 FunctionAddress
  u64 StackSize
  u64 RecordCount

Constant[NumConstants]:
  u64 Value

Record[NumRecords] (grouped by functions using RecordCount):
  u64 PatchpointId
  u32 InstructionOffset
  u16 Reserved
  u16 NumLocations
  Location[NumLocations]
  u16 Padding
  u16 NumLiveOuts
  LiveOut[NumLiveOuts]
  u8  AlignmentPadding[...]  // pad to 8-byte boundary

Location (8 bytes):
  u8  Kind               // 0=Register, 1=Direct, 2=Indirect, 3=Constant, 4=ConstIndex
  u8  Size               // in bytes
  u16 DwarfRegNum
  i32 OffsetOrSmallConst // meaning depends on Kind

LiveOut (4 bytes):
  u16 DwarfRegNum
  u8  Size
  u8  Reserved
```

For supported `Indirect` locations, `DwarfRegNum` must equal the stack pointer
or frame pointer DWARF register number (`SP`/`FP`), and `OffsetOrSmallConst` is
the signed byte offset from that base to the spill slot.

#### Building the record key

For each record belonging to a function:

```
callsite_addr = FunctionAddress + InstructionOffset
```

The runtime stores `callsite_addr` in the lookup table (sorted array + binary
search is sufficient).

---

## 4) Thread context capture contract (`rt_gc_safepoint`)

### 4.1 Purpose

`rt_gc_safepoint` is the runtime entry used for cooperative safepoints. Its
responsibilities include:

1. Capture the caller’s TS frame anchor `(top_ts_fp, top_ts_pc, top_ts_sp)`.
2. Publish it into the current thread’s `ThreadState`.
3. If GC is requested, park until GC completes.
4. Return back to TS code.

### 4.2 What must be captured

At the machine ABI boundary, when TS code calls `rt_gc_safepoint`, the callee
can recover the TS frame anchor.

On x86_64 SysV, at the instant `rt_gc_safepoint` is entered:

- `RBP` still holds the caller’s (TS) frame pointer.
- `RSP` points at the return address back into TS code.

Therefore, `rt_gc_safepoint` must capture:

- `top_ts_fp = RBP` (caller FP)
- `top_ts_pc = *(RSP + 0x00)` (return address into TS)
- `top_ts_sp`:
  - x86_64: `top_ts_sp = RSP + 8` (post-call SP; stackmap SP base)
  - (conceptually: the caller SP value at `top_ts_pc`, i.e. the instruction after the call)

If `rt_gc_safepoint` uses a standard `push rbp; mov rbp, rsp` prologue (i.e. it
has its own frame pointer), the equivalent capture is:

- `top_ts_fp = *(RBP_rt + 0x00)`
- `top_ts_pc = *(RBP_rt + 0x08)`

**Important:** Rust does not guarantee frame pointers by default. Implement
`rt_gc_safepoint` in a way that makes the capture unambiguous (recommended:
`#[naked]` + inline assembly stub that records `RBP` and `[RSP]` before any other
work).

These are stored in the per-thread `ThreadState` (exact struct layout is a
runtime implementation detail, but the fields must exist conceptually).

### 4.3 Publication requirements

The publication must be ordered such that the GC thread never observes “parked”
without a valid anchor.

Concrete requirement:

- `rt_gc_safepoint` must store `top_ts_fp` and `top_ts_pc` into `ThreadState`
  (and `top_ts_sp` when required for `Indirect [SP + off]` evaluation) **before**
  marking the thread as parked / safepointed.
- The GC coordinator must only read anchors from threads that are marked parked.

Use atomics with release/acquire ordering as needed; this is part of the runtime
implementation, but the ordering constraint is part of the ABI.

### 4.4 Platform assumptions

This contract is defined for Ubuntu x86_64. Other targets will require:

- different base register(s) for FP,
- different return address conventions,
- potentially different stack map register numbering.

Porting to a new platform requires revisiting every offset assumption in §2–§4.

---

## 5) Rust runtime boundary rules (critical)

Rust runtime code is **not** compiled with LLVM GC statepoints in the initial
implementation. Therefore:

- The GC must **not** scan Rust stacks.
- The GC must assume Rust stack locals are *invisible* unless explicitly
  registered.

This yields hard rules:

### 5.1 GC initiation policy (initial safe policy)

**Moving GC is only initiated from TS code, at a known safepoint call.**

Concretely:

- Runtime helpers (allocation, string ops, etc.) must not directly start a moving
  GC while they have GC pointers in locals.
- If a runtime helper needs GC (e.g., allocation failure), it returns a failure
  indicator to TS code.
- TS code then calls a safepoint/collect entry (e.g., `rt_gc_safepoint` /
  `rt_gc_collect`) which parks threads and runs GC.

This ensures that at GC time, the only stacks that may contain unregistered GC
pointers are TS stacks, which are precisely scanned via stack maps.

### 5.2 Runtime helpers must not hold unregistered pointers across TS calls

If runtime code ever calls back into TS code (callbacks, async continuations,
etc.), it must obey:

- Do not call into TS while holding GC pointers in Rust locals unless those
  pointers are registered as GC roots through an explicit root mechanism.

### 5.3 Runtime-held GC pointers must be traced explicitly

Any GC pointer stored in runtime-managed structures must be made visible to the
GC via:

- explicit root registration APIs (handle tables, pinned roots), and/or
- heap metadata that the GC traces directly (globals list, intern tables, etc.).

The GC must never rely on “finding” those pointers by scanning Rust stack frames.

---

## 6) Derived / interior pointers

LLVM statepoints and stack maps may represent **derived pointers** (a.k.a.
interior pointers), e.g.:

- a pointer to an object field (`obj + 16`)
- a pointer into an array payload (`elements + i * 8`)

These may appear as live values at safepoints.

### 6.1 Required behavior

The runtime/GC must be able to relocate derived pointers when their base object
moves.

Two acceptable strategies:

#### Strategy A: base + offset recomputation

At the safepoint:

1. Read `base` and `derived`.
2. Compute `delta = derived - base` (as integer byte offset).
3. After relocating `base` to `base'`, write back:
   - `derived' = base' + delta`

Requirements for this strategy:

- `base` must be available (also live in the stack map record).
- `derived` must be within the same allocation as `base` (delta remains valid).

#### Strategy B: base/derived mapping from LLVM encoding

If LLVM encodes explicit base↔derived relationships in stack map records, the
runtime may use that instead of computing deltas from raw values.

### 6.2 Observed LLVM 18 encoding (used by the runtime)

LLVM 18 `.llvm_stackmaps` explicitly encodes base/derived relationships for
statepoints:

- Statepoint stackmap records start with the standard 3-constant header and any
  deopt operand locations (see §2.5).
- The remaining locations are a sequence of `(base, derived)` pairs, one pair
  per `gc.relocate` use in the frame.
- For non-interior pointers, LLVM may emit duplicate locations where
  `base == derived`.
- A base slot may be reused across multiple pairs (multiple derived pointers can
  share one base).

For `Indirect` locations, the `offset` is a signed byte offset from the DWARF
base register value (`SP` or `FP`). For `Indirect [SP + off]`, `SP` uses the
caller stack pointer at the stackmap record PC (callsite return address).

Codegen must ensure every derived pointer has its base pointer also present in
the GC-live set at that safepoint.

---

## 7) Debugging / verification checklist

This section is the “first response” guide when GC crashes or corrupts memory.

### 7.1 Inspecting stack maps

Dump stack map records:

```bash
llvm-readobj --stackmap <binary>
```

Verify the section exists:

```bash
llvm-readobj -S <binary> | rg '\.llvm_stackmaps'
```

Correlate a return address PC with disassembly:

```bash
llvm-objdump -d --no-show-raw-insn <binary> | rg -n '<hex-address>'
```

### 7.2 Codegen invariants checklist (native-js must enforce)

At minimum:

- [ ] All TS functions: `frame-pointer=all`
- [ ] No tail calls in TS code (IR + backend options)
- [ ] Every TS→TS call lowered via `gc.statepoint`
- [ ] Poll safepoints exist for long-running loops (backedges and/or prologues)
- [ ] `.llvm_stackmaps` present in final binary and not stripped
- [ ] Stack map GC root locations are pointer-sized `Indirect` spill slots relative to SP/FP (no Register/Direct roots)
- [ ] Derived pointers: base pointers are also present in GC-live sets

### 7.3 Common failure modes

1. **Missing stack map records**
   - symptom: stack scanning stops early (lookup(pc) fails inside TS frames)
   - cause: some TS→TS call emitted as a plain `call`, or statepoint pass not run.

2. **Tail-call-elided frames**
   - symptom: FP chain skips expected frames; roots are missed.
   - cause: tail call optimization not fully disabled.

3. **Frame pointers omitted**
   - symptom: FP chain walk reads nonsense; crashes or corrupted scan.
   - cause: `frame-pointer=all` not set for all TS code (or LTO merged in code
     built without it).

4. **Register roots or unsupported stackmap location kinds**
   - symptom: runtime cannot interpret locations; either aborts (by design) or
     misses roots if incorrectly handled.
   - cause: codegen/runtime mismatch; need either runtime support or stricter
     codegen constraints (e.g. keep GC roots in addressable spill slots).

5. **`.llvm_stackmaps` missing in release builds**
   - symptom: lookup table empty; GC cannot scan.
   - cause: post-link strip removed the section, or link-time GC-sections
     discarded it.

6. **GC triggered from Rust runtime while holding pointers**
   - symptom: objects move but runtime locals still point to old addresses.
   - cause: violating §5 (Rust stacks are not scanned).
