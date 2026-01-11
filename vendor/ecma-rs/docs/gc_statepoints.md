# LLVM 18 GC Statepoints + Stackmap ABI (runtime-native)

This document specifies the exact LLVM IR **shape** that `native-js` must emit for LLVM 18 (opaque pointers) to interoperate with `runtime-native`'s precise, moving GC.

All file paths in this document are relative to the `vendor/ecma-rs/` workspace root unless stated otherwise.

Scope:

- LLVM **18.x**, opaque pointers (`ptr`, `ptr addrspace(N)`).
- `rewrite-statepoints-for-gc` is run during codegen.
- `runtime-native` consumes the emitted `.llvm_stackmaps` section for **root enumeration + relocation**.

This is intentionally concrete: copy/paste the snippets and adjust names/types as needed.

See also (repo-local, complementary):

- `docs/llvm_statepoints_llvm18.md` — verifier-correct minimal fixture IR
- `docs/llvm_statepoint_directives.md` — overriding statepoint ID / patch bytes when using `rewrite-statepoints-for-gc`
- `docs/runtime-native.md` — broader runtime ABI notes (thread anchoring, stackmap parsing, linker script usage)
- `runtime-native/link/stackmaps_nopie.ld` (non-PIE)
- `runtime-native/link/stackmaps.ld` (PIE, lld-friendly)
- `runtime-native/link/stackmaps_gnuld.ld` (GNU ld PIE) — linker script fragments that retain stackmaps and define start/end symbols
- `runtime-native/stackmaps.ld` (compat) — older alias kept for build script compatibility

The ABI assumptions documented here are guarded by fast regression scripts:

- `scripts/test_stackmap_abi.sh` (return PC + SP base)
- `scripts/test_statepoint_flags_patchbytes.sh` (flags range + patch_bytes lowering)
- `scripts/check_llvm_stackmaps.sh` (ensures `.llvm_stackmaps` is retained under `--gc-sections` + common strip modes)
- `scripts/test_stackmaps_pie_link.sh` (PIE link policy regression: ET_DYN + no DT_TEXTREL)

---

## Pointer + address-space conventions

### GC-managed heap pointers

All GC-managed pointers must be represented as:

```llvm
ptr addrspace(1)
```

This includes:

- object pointers (base pointers)
- interior pointers (derived pointers like `getelementptr` results)

Non-GC pointers (C pointers, stack pointers, code pointers) remain `ptr` (addrspace(0)) unless explicitly required otherwise.

### GC pointer discipline (do not hide pointers from LLVM)

LLVM's statepoint rewriting/relocation only tracks GC references that remain typed as `ptr addrspace(1)` in SSA form.

Do **not** let GC pointers “escape” into non-tracked representations across safepoints, such as:

- `ptr` (addrspace(0)) values derived from `ptr addrspace(1)` via `addrspacecast`,
- integers derived from GC pointers via `ptrtoint`,
- non-pointer-typed slots that happen to contain GC pointer bits.

If a GC pointer is hidden this way, LLVM will not emit (or rewrite uses to) the correct `gc.relocate`, and a moving GC will eventually read stale/unrelocated addresses.

In `native-js`, conversion between `ptr addrspace(1)` (managed pointers) and `ptr` (raw runtime ABI pointers) is restricted to dedicated runtime wrapper functions, and a debug lint enforces this discipline (`native_js::llvm::gc_lint`).

### Code pointers (call targets)

Call targets passed to the statepoint intrinsic are normal code pointers:

```llvm
ptr            ; addrspace(0)
```

### GC strategy name (`gc "coreclr"`)

Any function that contains (or may contain after LLVM passes) GC safepoints/statepoints must be marked as a GC-managed function:

```llvm
define void @f(...) gc "coreclr" { ... }
```

`native-js` standardizes on LLVM's production `coreclr` GC strategy name. Without a `gc "..."` strategy on the containing function, LLVM will not apply statepoint lowering correctly (and can fail verification/rewriting).

---

## Safepoint poll insertion (cooperative GC progress)

LLVM statepoint rewriting (`rewrite-statepoints-for-gc`) only rewrites *existing* calls; it does **not**
create new safepoints. For a stop-the-world moving GC, relying only on call sites is insufficient: a
tight loop with no calls can prevent a mutator thread from reaching a safepoint indefinitely.

`native-js` therefore requires **explicit safepoint polls** in long-running call-free paths.

The current implementation uses LLVM's `place-safepoints` pass as a *marker insertion* phase, then
lowers those markers into a cheap inline poll before running `rewrite-statepoints-for-gc`:

```text
function(place-safepoints),
  <native-js poll lowering>,
rewrite-statepoints-for-gc
```

`place-safepoints` inserts calls to:

```llvm
declare void @gc.safepoint_poll()
```

at:

- function entry, and
- loop backedges (including counted loops; `native-js` enables `--spp-all-backedges`).

`native-js` then rewrites each unconditional poll call into a fast-path inline epoch check:

```llvm
; fast path: no call/statepoint
%epoch = load atomic i64, ptr @RT_GC_EPOCH acquire, align 8
%requested = icmp ne i64 (and i64 %epoch, 1), 0
br i1 %requested, label %gc.poll.slow, label %gc.poll.cont

gc.poll.slow:
  ; slow path: becomes the actual statepoint
  call void @rt_gc_safepoint_slow(i64 %epoch)
  br label %gc.poll.cont
```

Finally, `rewrite-statepoints-for-gc` rewrites the **slow-path** call into an
`llvm.experimental.gc.statepoint.*` intrinsic. LLVM then emits stackmap records at the slow-path poll
PCs and `gc.relocate` for any live `ptr addrspace(1)` values across the poll. The fast path carries
only the load+branch overhead.

### Runtime contract for safepoint polling

The runtime must export:

- `RT_GC_EPOCH` (`_Atomic uint64_t` / `i64`), and
- `rt_gc_safepoint_slow(uint64_t epoch)` (`void (i64)`), entered only when the observed epoch is odd.

`rt_gc_safepoint_slow` must be able to publish the *managed caller's* context (SP/FP/return address)
such that it matches the `.llvm_stackmaps` record for the callsite. In this repo, this is implemented
by `runtime-native` (see `runtime-native/README.md`).

Note: LLVM stackmap `Indirect [SP + off]` locations use the caller's **post-call** stack pointer at
the stackmap record PC, so the published SP for the managed caller must use the same convention.

`runtime-native` also exports `gc.safepoint_poll(void)` for compatibility with LLVM's
`place-safepoints` naming convention, but `native-js` lowers away the marker calls and does not rely
on the symbol in final output.

---

## Required IR pattern: `gc.statepoint` (LLVM 18)

### Canonical intrinsic declaration

`native-js` must declare the LLVM 18 statepoint intrinsic exactly like this:

```llvm
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
```

Notes:

- The return type is `token`.
- The intrinsic is variadic (`...`).
- In LLVM 18, the **callee operand is an opaque `ptr`**, so the call target needs `elementtype(...)` (see below).
- The five fixed arguments are all required; in `native-js` we currently pass constants for all `immarg` positions (e.g. `i64 0xABCDEF00, i32 0, ..., i32 <NumCallArgs>, i32 0`).
  - The 5th argument is `flags`; we currently emit `flags = 0`.
  - On LLVM 18.x, the IR verifier only accepts `flags` values in the range `0..=3` (two-bit mask).

### Statepoint argument ordering (as emitted by `native-js`)

The concrete call shape we emit is:

```llvm
%tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 <ID>, i32 <NumPatchBytes>,
    ptr elementtype(<fn-ty>) <callee>,
    i32 <NumCallArgs>, i32 <Flags>,
    <call args...>,
    i32 <NumTransitionArgs>, i32 <NumDeoptArgs>)
  ["gc-live"(...)]
```

#### LLVM 18 call-site typing gotcha (do not omit)

In LLVM 18, the statepoint call must spell the intrinsic’s **full function type**
at the call site:

```llvm
call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(...)
```

If you omit the `(i64, i32, ptr, i32, i32, ...)` type and write `call token
@llvm.experimental.gc.statepoint.p0(...)`, `llvm-as-18` accepts the text but the
module fails verification with:

```
Invalid user of intrinsic instruction!
Intrinsic called with incompatible signature
```

Where:

- `<NumCallArgs>` is the number of normal call arguments in `<call args...>`.
- `<Flags>` is the `gc.statepoint` `flags` immarg (native-js currently emits `0`; LLVM 18 accepts `0..=3`).
  Empirically, LLVM 18 uses `flags=1` when a `"gc-transition"(...)` operand bundle is present.
- `<NumTransitionArgs>` and `<NumDeoptArgs>` are both always `0` today (but still required to be present).
  These are the **inline** transition/deopt counts; LLVM 18 rejects non-zero values here. This is
  separate from the stackmap record header `deopt_count` (`locations[2]`), which may be non-zero when
  a `"deopt"(...)` operand bundle is present.

#### Statepoint `<ID>` (StackMap record `patchpoint_id`)

The first `i64 <ID>` becomes the StackMap record's `patchpoint_id` / `Record ID` (as printed by `llvm-readobj-18 --stackmap`).

By default, LLVM's `rewrite-statepoints-for-gc` pass uses a fixed ID:

```llvm
i64 2882400000   ; 0xABCDEF00
```

However, this ID is **not required to be constant**:

- You can override it by attaching callsite string attributes before running `rewrite-statepoints-for-gc`
  (see `docs/llvm_statepoint_directives.md`).
- If you directly emit statepoints (instead of relying on the rewrite pass), you may use any `i64` IDs.

`runtime-native` does **not** rely on `patchpoint_id` for normal operation; it looks up the right record by **return address** (`instruction_offset` interpretation below). The ID is primarily useful for debugging and for optional verification heuristics.

### Callee operand must use `elementtype(...)`

Because pointers are opaque in LLVM 18, the callee operand must carry the callee function type via `elementtype(...)`:

```llvm
ptr elementtype(<fn-ty>) @callee
```

Without `elementtype(<fn-ty>)` on the callee operand, `rewrite-statepoints-for-gc` cannot reliably recover the call signature.

#### Indirect calls through function pointers

For indirect calls (callee is a `ptr`-typed function pointer), the same rule applies: the callee operand must be annotated with the *intended* function type:

```llvm
; %fp is a `ptr` holding a function pointer.
%tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 2882400000, i32 0,
    ptr elementtype(void (i64)) %fp,
    i32 1, i32 0,
    i64 123,
    i32 0, i32 0)
  ["gc-live"(ptr addrspace(1) %root)]
```

This is especially important with opaque pointers because `%fp` does not otherwise carry a signature.

### Extra immediates after call args (mandatory)

After the normal call arguments, the statepoint argument list must include **two additional constant immediates**:

1. `i32 NumTransitionArgs`
2. `i32 NumDeoptArgs`

For `native-js` today, both are always **constant zero**:

```llvm
i32 0, i32 0
```

These are required even though we do not use transition/deopt arguments.

LLVM 18 notes:

- Both values must be **constant `i32`** (they are `immarg`).
- Inline transition/deopt operands are deprecated; LLVM 18 rejects non-zero counts. If you ever need those features, use operand bundles (e.g. `"gc-transition"`, `"deopt"`) instead of inline operands.

### Live GC values: operand bundle `["gc-live"(...)]`

All GC values live across the safepoint must be listed in the `"gc-live"` operand bundle attached to the statepoint call:

```llvm
["gc-live"(ptr addrspace(1) %v0, ptr addrspace(1) %v1, ...)]
```

Only GC-managed pointers (`ptr addrspace(1)`) belong in this bundle.

### Non-void call returns: `gc.result`

A statepoint call always returns a `token`, even if the wrapped callee returns a value.

To recover the wrapped return value, emit a `gc.result` call with an overload matching the callee return type:

```llvm
; callee returns a GC pointer (addrspace(1))
declare ptr addrspace(1) @llvm.experimental.gc.result.p1(token)

; callee returns an integer
declare i64 @llvm.experimental.gc.result.i64(token)
```

Example:

```llvm
%tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(...)
%ret = call ptr addrspace(1) @llvm.experimental.gc.result.p1(token %tok)
```

If you use LLVM's `rewrite-statepoints-for-gc` pass, it inserts the required `gc.result.*` calls automatically. If you emit statepoints directly, you must emit `gc.result.*` yourself for any non-void wrapped call.

---

## Canonical statepoint + relocate IR (copy/paste)

### Simple base-pointer relocation

This is the minimal pattern for a safepoint call that takes one GC pointer argument and keeps it live across the call.

```llvm
declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

declare void @rt_safepoint(ptr addrspace(1))

define void @example(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
      i64 2882400000, i32 0,
      ptr elementtype(void (ptr addrspace(1))) @rt_safepoint,
      i32 1, i32 0,
      ptr addrspace(1) %obj,
      i32 0, i32 0)
    ["gc-live"(ptr addrspace(1) %obj)]

  ; For a base pointer, base_index == derived_index.
  %obj.relocated = call ptr addrspace(1)
      @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)

  ; IMPORTANT: do not use %obj after the statepoint. Use %obj.relocated.
  ret void
}
```

Key points:

- `i32 1` = number of normal call arguments (`%obj`).
- The two trailing immediates `i32 0, i32 0` are required (`NumTransitionArgs`, `NumDeoptArgs`).
- The `"gc-live"` bundle lists values that must be tracked/relocated.
- The relocation is expressed by a `gc.relocate` call and its result must replace the original pointer in all subsequent uses.

### Interior pointer relocation (base + derived)

LLVM supports relocating derived (interior) pointers by rooting **both** the base and the derived pointer and using `gc.relocate` with `(base_idx, derived_idx)`:

```llvm
; base: the object pointer
; derived: an interior pointer into the object
%field_ptr = getelementptr i8, ptr addrspace(1) %obj, i64 16

%tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 2882400000, i32 0,
    ptr elementtype(void (ptr addrspace(1))) @rt_safepoint,
    i32 1, i32 0,
    ptr addrspace(1) %obj,
    i32 0, i32 0)
  ["gc-live"(ptr addrspace(1) %obj, ptr addrspace(1) %field_ptr)]

; Indices are 0-based into the "gc-live" operand bundle list.
; base_index=0 (%obj), derived_index=1 (%field_ptr).
%field_ptr.relocated = call ptr addrspace(1)
    @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 1)
```

#### `gc.relocate` base/derived semantics

`@llvm.experimental.gc.relocate.*(token %tok, i32 base_index, i32 derived_index)`:

- `base_index` points at the **base object** in the `"gc-live"` list.
- `derived_index` points at the value to be relocated (often the same as base).
- Both indices must be **constant `i32` immediates** (`immarg`), and are 0-based.
- For interior pointers, the GC uses the **base pointer** to identify the object and the `(derived - base)` offset to reconstruct the derived pointer after moving the object.

#### When to root a derived pointer vs recompute it

`runtime-native` supports derived pointers at safepoints when they are encoded as `(base, derived)` pairs (per `gc.relocate`) in the stackmap record.

GC/runtime model:

- **Tracing/marking:** only the **base slot** is treated as a GC root (derived slots are *not* traced as independent roots).
- **Relocation/pointer update:** the GC updates pointers using the stackmap’s `(base_slot, derived_slot)` relocation pairs, preserving the interior offset:

  ```text
  delta       = derived_old - base_old
  derived_new = base_new + delta
  ```

Runtime contract:

- Both `base` and `derived` locations must be **addressable spill slots** (`Location::Indirect`).
- The `Location::Indirect` base register must be either the caller-frame **SP** or **FP**:
  - x86_64: DWARF reg 7 (`RSP`) or 6 (`RBP`)
  - AArch64: DWARF reg 31 (`SP`) or 29 (`X29`)
- Notes:
  - The runtime must read both `base_old` and `derived_old` before overwriting the base slot (or use equivalent per-frame fixup logic).
  - LLVM can emit duplicate relocation pairs; the runtime dedups repeated base slots / derived fixups within the same frame.
- Null convention: if `base_old == 0` or `derived_old == 0`, `derived_new` is forced to `0` (null is preserved).

This behavior is locked down by tests:

- [`runtime-native/tests/stackwalk_fp.rs`](../runtime-native/tests/stackwalk_fp.rs) (`derived_pointers_are_relocated_from_base`)
- [`runtime-native/tests/scan_reloc_pairs.rs`](../runtime-native/tests/scan_reloc_pairs.rs) (fixture containing `base != derived`)

If the derived pointer is a pure function of the base pointer (for example: a constant-field offset or an index value that is still available after the safepoint), it's usually better to:

1. Keep only the **base** pointer live across the safepoint (i.e. only base in `"gc-live"`).
2. After the safepoint, use the *relocated base* to recompute the derived pointer with another
   `getelementptr`.

This keeps stack maps smaller (fewer `"gc-live"` entries and fewer `gc.relocate` calls) and can make
later optimization easier.

Rooting a derived pointer (include it in `"gc-live"` and relocate it with `(base_idx, derived_idx)`) is appropriate when you *cannot* reliably recompute it after the safepoint.

---

## Codegen constraints required by `runtime-native`

### Frame pointers are required

Generated machine code must force frame pointers (no omission), so stackmap locations can be interpreted reliably across optimization levels.

In LLVM IR, this means ensuring safepointing functions are compiled with frame pointers enabled (e.g. via function attributes such as):

- `"frame-pointer"="all"` (preferred)

### GC roots must be spilled to addressable stack slots (no register roots)

LLVM can legally keep some statepoint GC roots in registers and describe them as `Location::Register` in `.llvm_stackmaps`.

`runtime-native`'s current stack-walking implementation for GC statepoints only supports **addressable spill slots** (`Location::Indirect`), so `native-js` codegen must ensure LLVM spills all `"gc-live"` values to the stack at safepoints.

Mitigation:

- Ensure LLVM codegen is configured to disallow register GC roots at statepoints:
  - `llc-18 --fixup-allow-gcptr-in-csr=false` (preferred)
  - `llc-18 --fixup-max-csr-statepoints=0` (fallback / defense-in-depth)
  - When embedding LLVM, set the equivalent global options via `LLVMParseCommandLineOptions`
    (`native-js/src/llvm/mod.rs`, `init_native_target`).
  - When codegen happens inside `clang-18 -flto`, pass the equivalent `-mllvm` flags
    (`native-js/src/link.rs`).
  - See `runtime-native/tests/statepoint_register_roots_codegen.rs` (tooling matrix) and
    `native-js/tests/stackmaps_no_register_roots.rs` (embedded LLVM) for regression coverage.

### Safepoints must not be tail calls

Safepoint calls must not be turned into tail calls.

Rationale: `runtime-native` identifies a safepoint by the **return address** of the call site (see stackmap interpretation below). Tail calls do not have a normal return address at the call site.

Practical requirements:

- Do not mark statepoint calls as `tail`.
- Disable tail-call elimination for functions containing statepoints (e.g. function attribute `"disable-tail-calls"="true"`), or ensure call sites are `notail`.

---

## `.llvm_stackmaps` section loading

After codegen, LLVM emits a `.llvm_stackmaps` section in the object file.

`runtime-native` requires this section to be:

- retained by the linker (do not strip / GC it away),
- mapped into memory in the final binary,
- discoverable at runtime.

### ELF stackmap boundaries

On ELF, the runtime locates stackmaps by taking the in-memory byte range of the linked `.llvm_stackmaps` section.

Important linker detail: because the section name starts with a dot (`.llvm_stackmaps`), GNU ld / lld do **not**
automatically synthesize usable `__start_*` / `__stop_*` boundary symbols for it. This repo therefore relies on a
small linker script fragment to both `KEEP` the section and define stable start/end symbols.

#### Linker script (repo default): `runtime-native/link/stackmaps.ld`

(`runtime-native/stackmaps.ld` exists for backwards compatibility and has the same effect.)

`runtime-native` ships a linker script fragment which:

- `KEEP`s `.llvm_stackmaps` (even under `--gc-sections`)
- defines:
  - `__start_llvm_stackmaps` / `__stop_llvm_stackmaps` (stable boundary symbols; preferred)
  - `__stackmaps_start` / `__stackmaps_end` (generic aliases)
  - `__fastr_stackmaps_start` / `__fastr_stackmaps_end` (project-specific aliases)
  - `__llvm_stackmaps_start` / `__llvm_stackmaps_end` (legacy aliases)

When linking a final ELF binary, apply it (example):

```bash
cc ... -Wl,-T,runtime-native/link/stackmaps.ld ...
```

Verify the linked output still contains the stackmaps section (or its relocated variant):

```bash
llvm-readobj --sections <bin> | rg llvm_stackmaps
```

When linking from Rust/Cargo:

- `runtime-native` can provide weak fallback range symbols, but for `--gc-sections` builds you still want the final link
  step to apply `runtime-native/link/stackmaps.ld` so the section is `KEEP`ed and fast symbol-based discovery works.
- Cargo does **not** automatically propagate linker-script args from dependencies, so Rust binaries must pass the linker
  script at the final link step (e.g. via `RUSTFLAGS` or your build system), or use the `native_js::link` /
  `scripts/native_link.sh` helpers which always inject it.

---

## Stackmap record interpretation (LLVM 18 + `rewrite-statepoints-for-gc`)

This section documents what `runtime-native` expects from `llvm-readobj-18 --stackmap` output after the pipeline runs:

```bash
llvm-as-18 | opt-18 -passes=rewrite-statepoints-for-gc | llc-18 --fixup-allow-gcptr-in-csr=false --fixup-max-csr-statepoints=0 -filetype=obj
```

### Call-site identity: `instruction_offset` is the return address

After `rewrite-statepoints-for-gc` in LLVM 18:

- Stackmap record `instruction_offset` equals the **return address offset** from the function start.
  - Absolute callsite address is `FunctionAddress + instruction_offset` (StackMap v3 format).
  - For statepoints, this absolute address equals the callsite return address (or the end of the reserved patch region).
  - For `NumPatchBytes = 0`, this is the PC after the `call` instruction.
  - For `NumPatchBytes > 0`, LLVM reserves a patchable region at the callsite (x86_64: a NOP sled)
    and the return address is the PC after that reserved region.
    The reserved region start offset is `instruction_offset - NumPatchBytes`.

`runtime-native` uses the return address captured at a safepoint to look up the corresponding stackmap record.

### Location list layout

After `rewrite-statepoints-for-gc` (LLVM 18), each safepoint record’s `locations` list has this layout:

1. A **prefix of 3 constant locations** (the statepoint "header"):
   - `locations[0]`: `callconv` (call convention ID; commonly `0` for C, `8` for `fastcc`)
   - `locations[1]`: `flags` (the `gc.statepoint` `flags` immarg; a 2-bit mask `0..=3`; `1` when a `"gc-transition"` operand bundle is present)
   - `locations[2]`: `deopt_count` (number of `"deopt"` operand locations; GC ignores these but must skip them)
   - These header entries are stackmap constants (`Constant` or `ConstIndex`/`ConstantIndex`), so
     `llvm-readobj --stackmap` may print either form.
2. Then `deopt_count` deopt operand locations (not GC roots).
3. Then, for each `gc.relocate` call associated with that statepoint:
   - 2 locations: `(base, derived)` in that order

`runtime-native` enumerates and updates roots from these `(base, derived)` pairs. Practically, this means:

- Any GC pointer that must remain valid after a safepoint must have a corresponding `gc.relocate` (and the relocated SSA value must be used after the safepoint).

So:

```
locations =
  Constant(callconv=0)
  Constant(flags)
  Constant(deopt_count)
  deopt_0, deopt_1, ... (deopt_count entries)
  base_0, derived_0
  base_1, derived_1
  ...
```

`runtime-native` interprets the header + deopt locations, then consumes the remaining locations as `(base, derived)` pairs.

### How relocation is performed in the runtime

`runtime-native` enumerates GC roots by interpreting the location pairs emitted for `gc.relocate` calls.

Current runtime contract (v1):

- Root locations must be **addressable spill slots**:
  - stackmap location kind must be `Location::Indirect`
  - base register must be either the caller-frame stack pointer (SP) or frame pointer (FP):
    - x86_64: DWARF reg 7 (`RSP`) or 6 (`RBP`)
    - AArch64: DWARF reg 31 (`SP`) or 29 (`X29`)
  - slot size must be pointer-sized (8 bytes on our supported 64-bit targets)
- Root locations in registers (`Location::Register`) or non-addressable expressions (`Location::Direct`) are currently **rejected**.
- Derived pointers (where the `(base, derived)` locations differ) are supported when both locations satisfy the spill-slot rules above:
  - The **base** slot is the GC root (traced/relocated).
  - The derived slot is updated after the base is relocated by preserving the interior offset:
    `derived_new = base_new + (derived_old - base_old)`.
  - Null convention: if `base_old == 0` or `derived_old == 0` (or if the GC writes `0` into the relocated base slot), the derived slot is forced to `0` (null).
  - LLVM can emit duplicate relocation pairs, so the runtime dedups repeated base slots / derived fixups within the same frame.

Operationally, within a frame:

- For each `(base, derived)` pair:
  - Evaluate the stack slots for both locations (`base_slot`, `derived_slot`).
  - Treat `base_slot` as a GC root slot to visit (deduped within the frame).
  - If `base_slot != derived_slot`, read both slot values before relocating anything:
    - If `base_old == 0` or `derived_old == 0`, record a fixup that forces the derived slot to `0`.
    - Otherwise, record `delta = derived_old - base_old` for this `(base_slot, derived_slot)` fixup.
- Then, in deterministic order for each unique `base_slot`:
  - Let the GC relocate the base slot in-place.
  - Apply any recorded derived fixups for that base:
    - if the fixup forces null (or `relocated_base == 0`): write `0` to the derived slot
    - else: `derived_new = relocated_base + delta` (written back to the derived slot in-place)

---

## Verification appendix (exact local pipeline)

To verify statepoint lowering and inspect emitted stackmaps locally:

```bash
# 1) Assemble LLVM IR to bitcode
llvm-as-18 < input.ll > input.bc

# 2) Lower statepoints (this is the ABI boundary we document)
opt-18 -passes=rewrite-statepoints-for-gc < input.bc > lowered.bc

# 3) Emit an object file containing .llvm_stackmaps
llc-18 --fixup-allow-gcptr-in-csr=false --fixup-max-csr-statepoints=0 -filetype=obj < lowered.bc > out.o

# 4) Inspect stackmaps
llvm-readobj-18 --stackmap out.o
```

---

## Linux linking policy for `.llvm_stackmaps` (lld)

### The problem

LLVM `.llvm_stackmaps` records contain absolute function addresses. In `.o` files this shows up as
`R_X86_64_64` relocations against `.text` symbols.

When linking a **PIE** binary with lld, this commonly fails with:

```
ld.lld: error: relocation R_X86_64_64 cannot be used against symbol '...'; recompile with -fPIC
```

### Options (evaluated)

- **Option A (simple): link non-PIE** with `-no-pie`
  - ✅ Works without extra steps.
  - ❌ Disables ASLR for the main executable (worse exploit mitigation).

- **Option B: PIE + allow text relocs** with `-Wl,-z,notext`
  - ✅ Keeps PIE/ASLR.
  - ❌ Enables relocations in read-only segments (“text relocs”), which is undesirable for hardening
    and can be rejected by some build policies.

- **Option C (recommended PIE mode): PIE without text relocs**
  - ✅ Keeps PIE/ASLR.
  - ✅ Avoids `-z notext` text relocations.
  - ✅ Works with lld by relocating stackmaps into a RELRO-friendly data section (so relocations are
    applied to RW memory and do not require `DT_TEXTREL`).
  - ❌ Requires an extra `llvm-objcopy` step in the link driver.

### Policy (default + PIE mode)

- **Default (today):** link non-PIE (`-no-pie`) to avoid runtime relocations entirely.
- **If PIE is required:** use Option C (relocate `.llvm_stackmaps` into `.data.rel.ro.llvm_stackmaps`) to avoid `DT_TEXTREL`.

When producing a PIE, native-js AOT output must:
 
1. Rewrite any input object containing `.llvm_stackmaps` (and `.llvm_faultmaps`, if present) to relocate it into a RELRO-friendly data section:
 
   ```bash
   llvm-objcopy-18 \
      --rename-section .llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents \
      --rename-section .llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents \
      input.o
   ```

2. Link with a linker script fragment that:
   - `KEEP`s `.llvm_stackmaps` / `.llvm_faultmaps` so `--gc-sections` can’t discard them
   - defines `__start_llvm_stackmaps` / `__stop_llvm_stackmaps` (plus aliases like `__fastr_stackmaps_start/end`)

   The script lives at:
   - `runtime-native/link/stackmaps.ld` (preferred)
   - `runtime-native/stackmaps.ld` (compat)

3. Use `--gc-sections` in release builds (safe because stackmaps are explicitly kept).

We provide reference link wrappers:

- `scripts/native_link.sh` — general-purpose linker wrapper (defaults to `-no-pie`; set
  `ECMA_RS_NATIVE_PIE=1` for PIE mode)
- `scripts/native_js_link_linux.sh` — native-js-specific PIE wrapper (always `-pie`)

### GNU ld (system linker) behavior in PIE mode

On Ubuntu, invoking `clang` without `-fuse-ld=lld` typically uses GNU ld and defaults to PIE.
If you link objects that contain a read-only `.llvm_stackmaps` section into a PIE, GNU ld will
generally **succeed** but emit warnings and mark the binary with `DT_TEXTREL`:

```
relocation against `...` in read-only section `.llvm_stackmaps'
creating DT_TEXTREL in a PIE
```

This happens because the stackmap table contains absolute `FunctionAddress` entries which become
runtime relocations under PIE; if `.llvm_stackmaps` is mapped read-only (e.g. alongside `.rodata`),
the dynamic loader would need to temporarily write to that segment to apply relocations.

To confirm, inspect the linked binary:

- `readelf -d <exe> | grep -E 'TEXTREL|FLAGS'` (presence of `TEXTREL` / `DF_TEXTREL`)
- `readelf -r <exe>` (look for `R_X86_64_RELATIVE` relocations whose **offsets fall inside**
  the `.llvm_stackmaps` address range)

If you need PIE **without** `DT_TEXTREL`, apply Option C (relocate `.llvm_stackmaps` into `.data.rel.ro.llvm_stackmaps`)
before linking. The dynamic relocations will still be present (and must be, for correct stackmap
addresses at runtime), but they will apply to a writable segment instead of requiring text relocs.

One more hardening note: if a linker script inserts the (writable) stackmaps output section
immediately after `.text` (common `INSERT AFTER .text` fragments), GNU ld may merge it into the
executable text PT_LOAD under PIE, producing an **RWX** segment once relocations require the
segment to be writable.

The repo's linker fragments avoid this by anchoring stackmaps in the RELRO/data region
(`INSERT BEFORE .dynamic;`). For GNU ld + PIE, the wrappers (`scripts/native_link.sh`,
`native_js::link`) select the GNU ld-specific fragment (`runtime-native/link/stackmaps_gnuld.ld`)
automatically.

## Example link commands

### Default (non-PIE): debug (no section GC)

```bash
clang-18 -fuse-ld=lld-18 -no-pie \
  -Wl,--script=runtime-native/link/stackmaps.ld \
  -o app_debug \
  main.o codegen.o
```

### Default (non-PIE): release (`--gc-sections`)

```bash
clang-18 -fuse-ld=lld-18 -no-pie \
  -Wl,--gc-sections \
  -Wl,--script=runtime-native/link/stackmaps.ld \
  -o app_release \
  main.o codegen.o
```

### PIE (Option C): debug (no section GC)

```bash
# (Optional) relocate stackmaps into a RELRO-friendly section if present:
llvm-objcopy-18 \
  --rename-section .llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents \
  --rename-section .llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents \
  codegen.o

clang-18 -fuse-ld=lld-18 -pie \
  -Wl,--script=runtime-native/link/stackmaps.ld \
  -o app_debug \
  main.o codegen.o
```

### PIE (Option C): release (`--gc-sections`)

```bash
llvm-objcopy-18 \
  --rename-section .llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents \
  --rename-section .llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents \
  codegen.o

clang-18 -fuse-ld=lld-18 -pie \
  -Wl,--gc-sections \
  -Wl,--script=runtime-native/link/stackmaps.ld \
  -o app_release \
  main.o codegen.o
```

## Stack map range symbols (linker-script integration)

The linker script defines the following symbols:

Canonical:

- `__start_llvm_stackmaps`
- `__stop_llvm_stackmaps`

Aliases:

- `__stackmaps_start` / `__stackmaps_end` (generic)
- `__fastr_stackmaps_start` / `__fastr_stackmaps_end` (project-specific)
- `__llvm_stackmaps_start` / `__llvm_stackmaps_end` (legacy)

All of these span the retained stackmaps contents in the final binary (usually in
`.data.rel.ro.llvm_stackmaps`, but legacy `.llvm_stackmaps` is still supported) and are intended to be the
primary runtime discovery mechanism (instead of parsing ELF section headers at runtime).

Example C usage:

```c
extern const unsigned char __start_llvm_stackmaps[];
extern const unsigned char __stop_llvm_stackmaps[];

size_t size = (size_t)(__stop_llvm_stackmaps - __start_llvm_stackmaps);
```
