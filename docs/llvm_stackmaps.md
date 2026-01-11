# LLVM `.llvm_stackmaps` (StackMap v3) — empirical notes (LLVM 18, x86_64)
This repo uses LLVM GC **statepoints** (`opt-18 -passes=rewrite-statepoints-for-gc`) and/or StackMap records to describe:

- GC roots at safepoints (statepoints)
- Deopt / side-metadata values (statepoint `deopt` bundle)
- Other recorded values (`llvm.experimental.stackmap`, patchpoints, etc.)

This doc is a decoding reference for StackMap **version 3** as produced by **LLVM 18.1.8** on **x86_64-pc-linux-gnu**, and captures real bytes for several `LocationKind`s we need to support.

Related IR reproducers live in:

- `investigation/llvm_stackmaps/`

See also:

- `docs/llvm_statepoint_stackmap_abi.md` — statepoint-specific ABI assumptions we rely on (return PC keying, flags range, and `patch_bytes` semantics) + regression scripts.

## Reproducing the examples

All examples are intended to be runnable with:

```bash
llvm-as-18 <file>.ll -o <file>.bc
opt-18 -passes=rewrite-statepoints-for-gc <file>.bc -o <file>.sp.bc   # statepoint examples only
llc-18 -O2 -filetype=obj <file>.sp.bc -o <file>.o                    # or <file>.bc for stackmap-only examples
llvm-readobj-18 --stackmap <file>.o
llvm-readobj-18 -x .llvm_stackmaps <file>.o
```

Some reproducers require extra `llc` flags (e.g. to allow keeping GC roots in
registers); see the comments at the top of each `.ll`.

## StackMap v3 binary layout (little-endian)

### Section header

```c
struct StackMapHeaderV3 {
  uint8_t  Version;        // = 3
  uint8_t  Reserved0;      // = 0
  uint16_t Reserved1;      // = 0

  uint32_t NumFunctions;
  uint32_t NumConstants;
  uint32_t NumRecords;
};
```

### Per-function info

```c
struct StackSizeRecord {
  uint64_t FunctionAddress;  // In .o files is typically 0 (relocation later)
  uint64_t StackSize;
  uint64_t RecordCount;
};
```

### Constant pool

`NumConstants` entries, each:

```c
uint64_t Constant;
```

### Callsite record

```c
struct StkMapRecord {
  uint64_t PatchPointID;        // For statepoints: the i64 "ID" operand
  uint32_t InstructionOffset;   // from function entry
                               // NOTE (statepoints): this is the "return PC" lookup key. For
                               // `gc.statepoint` with `patch_bytes=0`, it points at the instruction
                               // *after* the call. For `patch_bytes>0` (patchable callsites), it
                               // points at the end of the reserved patch region (x86_64: NOP sled).
                               // See `docs/llvm_statepoint_stackmap_abi.md`.
  uint16_t Reserved;            // = 0
  uint16_t NumLocations;

  Location Locations[NumLocations];

  // Then padding to an 8-byte boundary, then:
  uint16_t NumLiveOuts;
  uint16_t Reserved2;           // = 0
  LiveOut LiveOuts[NumLiveOuts];

  // Then padding to an 8-byte boundary.
};
```

### Location encoding (12 bytes each)

```c
struct Location {
  uint8_t  Kind;                 // LocationKind enum
  uint8_t  Reserved0;            // = 0
  uint16_t Size;                 // bytes
  uint16_t DwarfRegNum;          // when relevant
  uint16_t Reserved1;            // = 0
  int32_t  OffsetOrSmallConst;   // sign-extended to pointer size when needed
};
```

## Statepoint record: location ordering and the “meta” prefix

For stackmaps created from `gc.statepoint` (including `rewrite-statepoints-for-gc` output), LLVM 18 emits a fixed “meta” prefix in the `Locations[]` array:

| Location index | Meaning | Observed encoding |
|---:|---|---|
| `#1` | **Calling convention ID** of the original call | `Constant <cc-id>` |
| `#2` | **Statepoint flags** (the `i32 flags` operand to `gc.statepoint`) | `Constant <flags>` |
| `#3` | **Number of deopt operands** | `Constant <n>` |
| next `n` | The deopt operands themselves (`"deopt"(... )`) | one `Location` per operand |
| remaining | GC roots (`"gc-live"`) | encoded as base/derived pairs |

### Parsing GC roots from the record

After consuming the 3 meta locations and `n` deopt locations, the remaining
locations are the GC root list encoded as **base/derived pairs**.

For a non-derived root, LLVM encodes it as a pair where **base == derived**,
which is why `llvm-readobj --stackmap` output often shows duplicated entries.

To parse:

```text
deopt_count = Locations[2] (meta #3)
gc_pair_count = (NumLocations - 3 - deopt_count) / 2
```

### Notes on statepoint operand bundles

In LLVM 18 `rewrite-statepoints-for-gc` output, the `gc.statepoint` intrinsic
uses operand bundles (`"deopt"`, `"gc-live"`, `"gc-transition"`). Even though
the intrinsic call itself still contains two trailing `i32 0` placeholder
arguments, **the stackmap record is derived from the operand bundles**, and
the deopt count in meta location `#3` reflects the `"deopt"` bundle length.

### Verifying meta location `#1` (calling convention)

Example: `investigation/llvm_stackmaps/statepoint_meta_callconv_fastcc.ll`

`llvm-readobj-18 --stackmap` shows:

```text
#1: Constant 8, size: 8
```

`8` matches LLVM IR’s `fastcc` calling convention ID.

Notes:

- `ccc` (default) shows `#1: Constant 0`.
- `x86_regcallcc` shows `#1: Constant 92` (not included as a committed example; observed during investigation).

## LocationKind decoding rules (x86_64)

LLVM’s `LocationKind` values (as printed by `llvm-readobj --stackmap`):

| Kind | Name | How to interpret | Fields used |
|---:|---|---|---|
| `1` | `Register` | Value is in a register. | `DwarfRegNum`, `Size` |
| `2` | `Direct` | Value is the address `Reg + Offset`. **Do not dereference.** | `DwarfRegNum`, `OffsetOrSmallConst`, `Size` |
| `3` | `Indirect` | Value is loaded from memory at address `Reg + Offset`. | `DwarfRegNum`, `OffsetOrSmallConst`, `Size` |
| `4` | `Constant` | Value is `sign_extend_i32(OffsetOrSmallConst)`. | `OffsetOrSmallConst`, `Size` |
| `5` | `ConstantIndex` | Value is `Constants[OffsetOrSmallConst]` (64-bit pool). | `OffsetOrSmallConst`, `Size` |

`llvm-readobj` prints registers as `R#<n>`. The `<n>` is the **DWARF register
number** for the target, not an LLVM-internal register enum. For x86_64,
examples we observed include:

| DWARF reg | Common name |
|---:|---|
| `0` | `rax` |
| `3` | `rbx` |
| `7` | `rsp` |
| `17` | `xmm0` |

Runtime consequences:

- For **GC roots**:
  - `Indirect`: `(Reg + Offset)` is the *root slot address*; the slot contents are the root value and may be updated by GC.
  - `Register`: root value is in the register. If GC relocates, it must update the paused thread’s register context.
  - `Direct`: root value is an address computed from a register + offset (rare for heap roots; commonly used for *stack addresses* in deopt info).
- For **deopt operands**:
  - `Constant` / `ConstantIndex` are immediate values.
  - `Indirect` reads the value from a spill slot.
  - `Direct` is commonly used to record a stack address.

## Concrete LLVM 18 examples and `.llvm_stackmaps` bytes

All byte dumps below are `llvm-readobj-18 -x .llvm_stackmaps <obj>`.

### LocationKind = 1 (Register)

#### Statepoint `gc-live` root kept in a register

IR: `investigation/llvm_stackmaps/statepoint_gc_live_register.ll`

Requires `llc` flags:

```bash
llc-18 -O2 -filetype=obj \
  -fixup-allow-gcptr-in-csr \
  -max-registers-for-gc-values=1 \
  <file>.sp.bc -o <file>.o
```

`llvm-readobj-18 --stackmap`:

```text
Record ID: 2882400000, instruction offset: 9
  5 locations:
    #1: Constant 0, size: 8
    #2: Constant 0, size: 8
    #3: Constant 0, size: 8
    #4: Register R#3, size: 8
    #5: Register R#3, size: 8
```

Bytes:

```text
Hex dump of section '.llvm_stackmaps':
0x00000000 03000000 01000000 00000000 01000000 ................
0x00000010 00000000 00000000 08000000 00000000 ................
0x00000020 01000000 00000000 00efcdab 00000000 ................
0x00000030 09000000 00000500 04000800 00000000 ................
0x00000040 00000000 04000800 00000000 00000000 ................
0x00000050 04000800 00000000 00000000 01000800 ................
0x00000060 03000000 00000000 01000800 03000000 ................
0x00000070 00000000 00000000 00000000 00000000 ................
```

Decoding a `Register` `Location` (12 bytes):

```text
01 00 08 00  03 00 00 00  00 00 00 00
Kind=1 (Register), DwarfRegNum=3 (RBX), Size=8
```

#### `llvm.experimental.stackmap` operand in a register (non-statepoint)

IR: `investigation/llvm_stackmaps/stackmap_register.ll`

`llvm-readobj-18 --stackmap`:

```text
Record ID: 42, instruction offset: 7
  1 locations:
    #1: Register R#0, size: 8
```

Bytes:

```text
Hex dump of section '.llvm_stackmaps':
0x00000000 03000000 01000000 00000000 01000000 ................
0x00000010 00000000 00000000 08000000 00000000 ................
0x00000020 01000000 00000000 2a000000 00000000 ........*.......
0x00000030 07000000 00000100 01000800 00000000 ................
0x00000040 00000000 00000000 00000000 00000000 ................
```

Decoding the single `Location` (12 bytes):

```text
01 00 08 00  00 00 00 00  00 00 00 00
^  ^  ^^^^^  ^^^^^  ^^^^^  ^^^^^^^^^^
|  |   Size  Reg    rsvd      Offset=0
|  rsvd
Kind=1 (Register), DwarfRegNum=0 (RAX), Size=8
```

#### `llvm.experimental.stackmap` operand in an XMM register (non-statepoint)

IR: `investigation/llvm_stackmaps/stackmap_register_xmm0.ll`

`llvm-readobj-18 --stackmap`:

```text
Record ID: 43, instruction offset: 4
  1 locations:
    #1: Register R#17, size: 16
```

On x86_64, DWARF register 17 corresponds to `xmm0`. Note the `Size` reported is
the full register width (16 bytes), even if the IR operand is a scalar `double`.

Bytes:

```text
Hex dump of section '.llvm_stackmaps':
0x00000000 03000000 01000000 00000000 01000000 ................
0x00000010 00000000 00000000 08000000 00000000 ................
0x00000020 01000000 00000000 2b000000 00000000 ........+.......
0x00000030 04000000 00000100 01001000 11000000 ................
0x00000040 00000000 00000000 00000000 00000000 ................
```

Decoding the single `Location`:

```text
01 00 10 00  11 00 00 00  00 00 00 00
Kind=1 (Register), DwarfRegNum=17 (xmm0), Size=16
```

### LocationKind = 2 (Direct)

IR: `investigation/llvm_stackmaps/statepoint_deopt_direct.ll`

`llvm-readobj-18 --stackmap` (excerpt):

```text
6 locations:
  #4: Direct R#7 + 16, size: 8
```

Bytes:

```text
Hex dump of section '.llvm_stackmaps':
0x00000000 03000000 01000000 00000000 01000000 ................
0x00000010 00000000 00000000 18000000 00000000 ................
0x00000020 01000000 00000000 00efcdab 00000000 ................
0x00000030 0e000000 00000600 04000800 00000000 ................
0x00000040 00000000 04000800 00000000 00000000 ................
0x00000050 04000800 00000000 01000000 02000800 ................
0x00000060 07000000 10000000 03000800 07000000 ................
0x00000070 08000000 03000800 07000000 08000000 ................
0x00000080 00000000 00000000                   ................
```

Decoding the Direct `Location`:

```text
02 00 08 00  07 00 00 00  10 00 00 00
Kind=2 (Direct), DwarfRegNum=7 (RSP), Offset=16
=> value = (RSP + 16)
```

### LocationKind = 5 (ConstantIndex)

IR: `investigation/llvm_stackmaps/statepoint_deopt_constantindex.ll`

`llvm-readobj-18 --stackmap` (excerpt):

```text
Num Constants: 1
  #1: 1311768467463790320
...
  #4: ConstantIndex #0 (1311768467463790320), size: 8
```

Bytes:

```text
Hex dump of section '.llvm_stackmaps':
0x00000000 03000000 01000000 01000000 01000000 ................
0x00000010 00000000 00000000 08000000 00000000 ................
0x00000020 01000000 00000000 f0debc9a 78563412 ............xV4.
0x00000030 00efcdab 00000000 0a000000 00000600 ................
0x00000040 04000800 00000000 00000000 04000800 ................
0x00000050 00000000 00000000 04000800 00000000 ................
0x00000060 01000000 05000800 00000000 00000000 ................
0x00000070 03000800 07000000 00000000 03000800 ................
0x00000080 07000000 00000000 00000000 00000000 ................
```

Key points:

- The constant pool entry starts at `0x28` (after the per-function table): `f0debc9a 78563412` = `0x123456789abcdef0`.
- The `ConstantIndex` location has `Kind=5` and `OffsetOrSmallConst=0`, selecting `Constants[0]`.

## What must be implemented for correctness

Even if current `rewrite-statepoints-for-gc` output tends to spill most non-constant values (`Indirect`), a correct stackmap parser/runtime should support **all v3 LocationKinds**:

- `Register` (1)
- `Direct` (2)
- `Indirect` (3)
- `Constant` (4)
- `ConstantIndex` (5)

Additionally, statepoint consumers must understand the **meta prefix** and deopt count described above; otherwise the GC root list will be mis-parsed.
