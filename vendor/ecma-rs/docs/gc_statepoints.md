# LLVM 18 GC Statepoints + Stackmap ABI (runtime-native)

This document specifies the exact LLVM IR **shape** that `native-js` must emit for LLVM 18 (opaque pointers) to interoperate with `runtime-native`'s precise, moving GC.

Scope:

- LLVM **18.x**, opaque pointers (`ptr`, `ptr addrspace(N)`).
- `rewrite-statepoints-for-gc` is run during codegen.
- `runtime-native` consumes the emitted `.llvm_stackmaps` section for **root enumeration + relocation**.

This is intentionally concrete: copy/paste the snippets and adjust names/types as needed.

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
- `<NumTransitionArgs>` and `<NumDeoptArgs>` are both always `0` today (but still required to be present).

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

### Extra immediates after call args (mandatory)

After the normal call arguments, the statepoint argument list must include **two additional constant immediates**:

1. `i32 NumTransitionArgs`
2. `i32 NumDeoptArgs`

For `native-js` today, both are always **constant zero**:

```llvm
i32 0, i32 0
```

These are required even though we do not use transition/deopt arguments.

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
- For interior pointers, the GC uses the **base pointer** to identify the object and the `(derived - base)` offset to reconstruct the derived pointer after moving the object.

#### When to root a derived pointer vs recompute it

**Important:** `runtime-native` currently rejects derived pointers at safepoints (it requires `base` and `derived` to be the same spill slot). Until derived-pointer support is implemented in the runtime, `native-js` codegen should treat derived pointers as **not supported** and use the “recompute after safepoint” strategy below.

If the derived pointer is a pure function of the base pointer (for example: a constant-field offset or an index value that is still available after the safepoint), it's usually better to:

1. Keep only the **base** pointer live across the safepoint (i.e. only base in `"gc-live"`).
2. After the safepoint, use the *relocated base* to recompute the derived pointer with another
   `getelementptr`.

This keeps stack maps smaller (fewer `"gc-live"` entries and fewer `gc.relocate` calls) and can make
later optimization easier.

Once `runtime-native` supports derived pointers, rooting a derived pointer (include it in `"gc-live"` and relocate it with `(base_idx, derived_idx)`) is appropriate when you *cannot* reliably recompute it after the safepoint.

---

## Codegen constraints required by `runtime-native`

### Frame pointers are required

Generated machine code must force frame pointers (no omission), so stackmap locations can be interpreted reliably across optimization levels.

In LLVM IR, this means ensuring safepointing functions are compiled with frame pointers enabled (e.g. via function attributes such as):

- `"frame-pointer"="all"` (preferred)

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

This repo supports two common ways to get that byte range:

#### Option A (repo default): `runtime-native/stackmaps.ld` (`__fastr_stackmaps_start` / `__fastr_stackmaps_end`)

`runtime-native` ships a linker script fragment, `runtime-native/stackmaps.ld`, which:

- `KEEP`s `.llvm_stackmaps` (even under `--gc-sections`)
- defines:
  - `__fastr_stackmaps_start` / `__fastr_stackmaps_end` (preferred names)
  - `__llvm_stackmaps_start`
  - `__llvm_stackmaps_end`

When linking a final ELF binary, apply it (example):

```bash
cc ... -Wl,-T,runtime-native/stackmaps.ld ...
```

Cargo applies this automatically when linking Rust binaries via `runtime-native/build.rs`, but you must pass it explicitly when linking from C/clang.

#### Option B: conventional section start/stop (`__start_llvm_stackmaps` / `__stop_llvm_stackmaps`)

Some ELF linkers/toolchains also provide conventional section boundary symbols of the form:

- `__start_llvm_stackmaps`
- `__stop_llvm_stackmaps`

If you rely on these instead of `stackmaps.ld`, ensure the section is still retained (not removed by `--gc-sections` / stripping).

---

## Stackmap record interpretation (LLVM 18 + `rewrite-statepoints-for-gc`)

This section documents what `runtime-native` expects from `llvm-readobj-18 --stackmap` output after the pipeline runs:

```bash
llvm-as-18 | opt-18 -passes=rewrite-statepoints-for-gc | llc-18 -filetype=obj
```

### Call-site identity: `instruction_offset` is the return address

After `rewrite-statepoints-for-gc` in LLVM 18:

- Stackmap record `instruction_offset` equals the **return address**.
  - For `NumPatchBytes = 0`, this is the PC after the `call` instruction.
  - For `NumPatchBytes > 0`, LLVM reserves a patchable region at the callsite (x86_64: a NOP sled)
    and the return address is the PC after that reserved region.

`runtime-native` uses the return address captured at a safepoint to look up the corresponding stackmap record.

### Location list layout

After `rewrite-statepoints-for-gc` (LLVM 18), each safepoint record’s `locations` list has this layout:

1. A **prefix of 3 constant locations**.
   - The second constant (`locations[1]`, shown as location `#2` in `llvm-readobj --stackmap`) corresponds
     to the IR `gc.statepoint` `flags` immarg.
   - Since `runtime-native` currently assumes `flags = 0`, these three constants are expected to be zero
     in our pipeline.
2. Then, for each `gc.relocate` call associated with that statepoint:
   - 2 locations: `(base, derived)` in that order

So:

```
locations =
  Constant(0)
  Constant(<flags>)
  Constant(0)
  base_0, derived_0
  base_1, derived_1
  ...
```

`runtime-native` ignores the three leading constant entries and consumes the remaining locations as pairs.

### How relocation is performed in the runtime

`runtime-native` enumerates GC roots by interpreting the location pairs emitted for `gc.relocate` calls.

Current runtime contract (v1):

- Root locations must be **addressable spill slots** (`Location::Indirect` based on SP/FP), not values kept only in registers.
- Derived pointers (base/derived locations that differ) are currently **rejected** to avoid silent corruption.
  - `native-js` should avoid keeping interior pointers live across safepoints; keep the base pointer live and recompute derived pointers after the safepoint if needed.

For each `(base, derived)` pair (where the locations are the same spill slot):

- Read the *current* pointer value from the spill slot.
- If the pointer is `0` (null), treat it as non-root (skip).
- Ask the GC to relocate it to `new_ptr` (moving/compacting collector).
- Write `new_ptr` back to the spill slot (in place).

---

## Verification appendix (exact local pipeline)

To verify statepoint lowering and inspect emitted stackmaps locally:

```bash
# 1) Assemble LLVM IR to bitcode
llvm-as-18 < input.ll > input.bc

# 2) Lower statepoints (this is the ABI boundary we document)
opt-18 -passes=rewrite-statepoints-for-gc < input.bc > lowered.bc

# 3) Emit an object file containing .llvm_stackmaps
llc-18 -filetype=obj < lowered.bc > out.o

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
  - ✅ Works with lld by making `.llvm_stackmaps` **writable in the object file**, so relocations are
    applied to RW memory (normal dynamic relocations).
  - ❌ Requires an extra `llvm-objcopy` step in the link driver.

### Policy (default + PIE mode)

- **Default (today):** link non-PIE (`-no-pie`) to avoid runtime relocations entirely.
- **If PIE is required:** use Option C (rewrite `.llvm_stackmaps` to be writable) to avoid `DT_TEXTREL`.

When producing a PIE, native-js AOT output must:

1. Rewrite any input object containing `.llvm_stackmaps` to make the section writable:

   ```bash
   llvm-objcopy-18 \
     --set-section-flags .llvm_stackmaps=alloc,load,contents,data \
     input.o
   ```

2. Link with a linker script fragment that:
   - `KEEP`s `.llvm_stackmaps` so `--gc-sections` can’t discard it
   - defines `__fastr_stackmaps_start` / `__fastr_stackmaps_end` symbols

   The script lives at:
   - `vendor/ecma-rs/runtime-native/stackmaps.ld`

3. Use `--gc-sections` in release builds (safe because stackmaps are explicitly kept).

We provide a reference PIE link wrapper:

- `vendor/ecma-rs/scripts/native_js_link_linux.sh`

## Example link commands

### Default (non-PIE): debug (no section GC)

```bash
clang-18 -fuse-ld=lld -no-pie \
  -Wl,--script=vendor/ecma-rs/runtime-native/stackmaps.ld \
  -o app_debug \
  main.o codegen.o
```

### Default (non-PIE): release (`--gc-sections`)

```bash
clang-18 -fuse-ld=lld -no-pie \
  -Wl,--gc-sections \
  -Wl,--script=vendor/ecma-rs/runtime-native/stackmaps.ld \
  -o app_release \
  main.o codegen.o
```

### PIE (Option C): debug (no section GC)

```bash
# (Optional) make stackmaps writable if present:
llvm-objcopy-18 --set-section-flags .llvm_stackmaps=alloc,load,contents,data codegen.o

clang-18 -fuse-ld=lld -pie \
  -Wl,--script=vendor/ecma-rs/runtime-native/stackmaps.ld \
  -o app_debug \
  main.o codegen.o
```

### PIE (Option C): release (`--gc-sections`)

```bash
llvm-objcopy-18 --set-section-flags .llvm_stackmaps=alloc,load,contents,data codegen.o

clang-18 -fuse-ld=lld -pie \
  -Wl,--gc-sections \
  -Wl,--script=vendor/ecma-rs/runtime-native/stackmaps.ld \
  -o app_release \
  main.o codegen.o
```

## Stack map range symbols (linker-script integration)

The linker script defines the following symbols:

- `__fastr_stackmaps_start`
- `__fastr_stackmaps_end`

It also defines legacy aliases:

- `__llvm_stackmaps_start`
- `__llvm_stackmaps_end`

All of these span the retained `.llvm_stackmaps` contents in the final binary and are intended to be the
primary runtime discovery mechanism (instead of parsing ELF section headers at runtime).

Example C usage:

```c
extern const unsigned char __fastr_stackmaps_start[];
extern const unsigned char __fastr_stackmaps_end[];

size_t size = (size_t)(__fastr_stackmaps_end - __fastr_stackmaps_start);
```
