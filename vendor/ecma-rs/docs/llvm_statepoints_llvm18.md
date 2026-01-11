# LLVM 18 `gc.statepoint` IR (opaque pointers) and `.llvm_stackmaps`

This document captures the **LLVM 18.1.x verifier-accepted** IR form for
`@llvm.experimental.gc.statepoint` under **opaque pointers** (the default in LLVM
18). It exists so `native-js` can emit statepoints without relying on
`llc --disable-verify`.

The repository includes a minimal working fixture:

* `fixtures/llvm_stackmap_abi/statepoint.ll`

That fixture:

1. Assembles with `llvm-as-18` with full verification.
2. Compiles with `llc-18 -filetype=obj` to an object containing a
   `.llvm_stackmaps` section.
3. Uses `"gc-live"` roots and `gc.relocate` indices in a verifier-correct way.

## Non-negotiable requirements (LLVM 18)

### 1) The containing function must be a GC function

Any function containing a statepoint must have a GC strategy name:

```llvm
define void @f(...) gc "coreclr" { ... }
```

If you omit `gc "..."`, LLVM 18 will abort during module verification with an
error like:

```
LLVM ERROR: unsupported GC:
```

For our purposes, any statepoint-capable built-in strategy works. This repo
standardizes on `"coreclr"` (production-used); `"statepoint-example"` also works
but is a demo/reference strategy.

### 2) GC pointers must be in a GC address space (addrspace(1))

Under LLVM 18's statepoint-based strategies (including `"coreclr"`), LLVM treats
**`addrspace(1)` pointers** as GC pointers. This affects:

* the types in the `"gc-live"` bundle
* `gc.relocate` return type
* `gc.result` return type (when the callee returns a GC pointer)

If you try to use `ptr` in addrspace(0) with `gc.relocate`, the verifier rejects
it:

```
gc.relocate: must return gc pointer
```

### 3) The statepoint callee operand must have `elementtype(...)`

With opaque pointers, LLVM cannot recover the callee function type from a `ptr`.
LLVM 18’s verifier therefore requires the callee operand passed to the statepoint
to carry an explicit `elementtype`:

```llvm
ptr elementtype(ptr addrspace(1) (ptr addrspace(1))) @callee
```

If you omit it you will see:

```
gc.statepoint callee argument must have elementtype attribute
```

### 4) The final two `i32` operands are REQUIRED (and must be constant)

In LLVM 18 the intrinsic still syntactically includes the legacy counts for
inline transition + inline deopt operands, and the verifier insists they exist:

* `numTransitionArgs` (constant `i32`)
* `numDeoptArgs` (constant `i32`)

If you omit them you’ll see errors like:

* `gc.statepoint number of transition arguments must be constant integer`
* `gc.statepoint number of deoptimization arguments must be constant integer`

In LLVM 18 these inline forms are **rejected** (not merely warned): the verifier
fails if either count is non-zero:

* `gc.statepoint w/inline transition bundle is deprecated`
* `gc.statepoint w/inline deopt operands is deprecated`

So for LLVM 18 codegen, always emit:

```llvm
i32 0, i32 0
```

and use operand bundles (`"deopt"`, `"gc-transition"`) if you need those features.

## Intrinsic declarations (LLVM 18)

The exact overload suffixes matter:

* `gc.statepoint.p0` — `p0` is the callee pointer address space (almost always 0)
* `gc.result.pN` / `gc.relocate.pN` — `pN` is the **GC pointer address space**
  (`N = 1` for `addrspace(1)`)

Canonical declarations (matching the fixture):

```llvm
declare token @llvm.experimental.gc.statepoint.p0(
    i64 immarg, i32 immarg, ptr,
    i32 immarg, i32 immarg, ...)

declare ptr addrspace(1) @llvm.experimental.gc.result.p1(token)

declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(
    token, i32 immarg, i32 immarg)
```

## Verifier-correct statepoint call shape (LLVM 18)

Conceptually, a statepoint call looks like:

```llvm
%tok = call token (i64, i32, ptr, i32, i32, ...)
  @llvm.experimental.gc.statepoint.p0(
    i64 <id>,
    i32 <num_patch_bytes>,
    ptr elementtype(<callee fn type>) <callee>,
    i32 <num_call_args>,
    i32 <flags>,
    ; <num_call_args> call arguments...
    ...,
    i32 0,  ; numTransitionArgs (required, must be const 0 in LLVM 18)
    i32 0)  ; numDeoptArgs      (required, must be const 0 in LLVM 18)
  [ "gc-live"(<gcptr0>, <gcptr1>, ...),
    "deopt"(<val0>, <val1>, ...)?,
    "gc-transition"(<val0>, ...)? ]
```

Notes:

* `<id>` becomes the StackMap **Record ID** (`llvm-readobj --stackmap` prints it).
  * When using `rewrite-statepoints-for-gc` (instead of emitting `gc.statepoint`
    intrinsics manually), LLVM 18 also supports overriding `<id>` and
    `<num_patch_bytes>` by attaching callsite string attributes to the original
    `call`/`invoke`:
    - `"statepoint-id"="<u64>"`
    - `"statepoint-num-patch-bytes"="<u32>"`
    See [`llvm_statepoint_directives.md`](./llvm_statepoint_directives.md).
* `<flags>` (5th argument) is a bitmask. On LLVM 18.x, the IR verifier only accepts
  values in the range **0..3** (bits 0 and 1). This project currently uses
  `flags = 0`.
  In emitted stackmaps on x86_64, this value appears as the second constant
  location in each record (location `#2` in `llvm-readobj --stackmap` output).
* `<num_patch_bytes>` (2nd argument) controls whether LLVM emits a real call or a
  patchable region at the statepoint site:
  * `patch_bytes = 0`: emits a normal `call` instruction.
  * `patch_bytes > 0`: reserves a patchable region (x86_64: a NOP sled) and shifts
    the stackmap `instruction offset` to the end of that reserved region (the
    "return address" if/when a call is patched in).
  This behavior is regression-tested by:
  * `scripts/test_statepoint_flags_patchbytes.sh`
* For varargs intrinsics (like statepoint/stackmap), LLVM 18 is strict about
  writing the full call signature at the callsite, e.g.:
  `call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(...)`.
* Operand bundles are written as a **single** bracket list with comma-separated
  bundles:

  ```llvm
  [ "deopt"(i32 123), "gc-live"(ptr addrspace(1) %root) ]
  ```

  (LLVM IR does **not** accept multiple trailing `[...] [...]` bundle groups.)

## `gc.relocate` index mapping (LLVM 18)

`gc.relocate` picks entries from the `"gc-live"` list using **0-based indices**.
The verifier bounds-checks these indices.

* `base_idx` — which `"gc-live"` entry is the *base* pointer (object reference)
* `derived_idx` — which `"gc-live"` entry is the *derived* pointer (may be the
  same as base for non-interior pointers)

Example:

```llvm
[ "gc-live"(ptr addrspace(1) %obj, ptr addrspace(1) %derived) ]

; Relocate the base pointer itself:
%obj.reloc = call ptr addrspace(1)
  @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)

; Relocate an interior pointer (derived from %obj):
%derived.reloc = call ptr addrspace(1)
  @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 1)
```

## Important: `"gc-live"` does not automatically imply stackmap roots

LLVM’s StackMap record is driven by **relocations** (and deopt state), not by the
mere presence of values in `"gc-live"`.

Practical implications for codegen:

* If you add a GC pointer to `"gc-live"` but never use a corresponding
  `gc.relocate` result (or it gets DCE’d), LLVM may emit a StackMap record with no
  GC pointer locations for that value.
* `gc.relocate` is `memory(none)` and can be optimized away if its result is not
  used. Ensure every relocated pointer is consumed (typically by replacing all
  post-safepoint uses of the original pointer with the relocated SSA value).

The fixture keeps relocations live by using the relocated pointers after the
statepoint.

## What to expect in `.llvm_stackmaps`

Compile the fixture and inspect the stackmap:

```bash
llvm-as-18 fixtures/llvm_stackmap_abi/statepoint.ll -o /tmp/sp.bc
llc-18 -filetype=obj /tmp/sp.bc -o /tmp/sp.o
llvm-readobj-18 --stackmap /tmp/sp.o
llvm-objdump-18 -d --no-show-raw-insn /tmp/sp.o
```

Key observations (x86_64):

* `LLVM StackMap Version: 3`
* A `Record ID` matches the statepoint `<id>` immediate.
* `locations[1]` (location `#2` in `llvm-readobj --stackmap` output) is the
  `gc.statepoint` `flags` immarg.
* `instruction offset` is the **return address** relative to the function start.
  If `patch_bytes = 0`, this is the offset of the instruction *after* the call.
  If `patch_bytes > 0`, LLVM reserves a patchable region and `instruction offset`
  points to the byte *after* that region (where execution would resume if a call
  is patched in).
  This implies a runtime patcher must ensure the call's return address matches
  that end-of-region address (the stackmap lookup key).
* GC roots typically show up as locations like:

  ```
  Indirect [R#7 + <off>], size: 8
  ```

  On x86_64, `R#7` is DWARF register 7 (`RSP`), so this means the root was spilled
  to the stack at `[rsp + off]` at the safepoint.

Frame-pointer note:

* Depending on optimization level and code shape, LLVM may report locations
  relative to `RSP`, `RBP`, or in registers.
* If the runtime wants more predictable frame layouts, compile GC functions with
  a frame pointer (`-fno-omit-frame-pointer` / `frame-pointer="all"`). Stackmaps
  remain valid either way; the runtime just needs to interpret the register
  numbers in the record.

Derived-pointer note:

* LLVM may optimize derived (interior) pointers whose offset is known, and the
  StackMap record may report identical base/derived locations even if the IR
  contained a derived pointer. This is still correct if the backend recomputes
  the derived address from the relocated base.
