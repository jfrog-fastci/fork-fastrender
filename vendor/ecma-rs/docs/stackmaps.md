# Stackmaps (LLVM statepoints) — runtime assumptions

This project uses **LLVM statepoints** (`rewrite-statepoints-for-gc` → `gc.statepoint` + `.llvm_stackmaps`) to implement precise GC stack scanning.

Our initial native runtime stack-walking strategy is intentionally simple:

- **Walk frames via the RBP chain** (frame pointers).
- **Compute each frame's RSP** from the ABI + the known frame layout.
- **Do not** use `libunwind`, `ucontext`, or DWARF register reconstruction.

That strategy is only correct if every GC root referenced by a stackmap is stored in a **memory location**.

## `.llvm_stackmaps` can contain multiple StackMap v3 blobs

LLVM emits a complete StackMap v3 table into each object file’s `.llvm_stackmaps` section.
When linking multiple objects, ELF linkers concatenate those section payloads, producing
**multiple independent StackMap v3 blobs back-to-back**, each starting with its own `version=3`
header.

This means runtime code must not assume `.llvm_stackmaps` is a single global header + tables.

Runtime-native provides helpers that handle both cases:

- Use `runtime_native::stackmaps::StackMaps::parse(bytes)` (preferred) when parsing a linked
  image’s `.llvm_stackmaps` section; it iterates all blobs and builds one callsite index.
- `runtime_native::stackmaps::StackMap::parse(bytes)` parses a **single** StackMap v3 blob and
  will fail fast if it looks like the input contains multiple concatenated blobs.

## Contract: no `Register` GC roots

At every statepoint, LLVM emits a stackmap record with a list of live GC pointer locations.

We require:

- All GC pointer locations in `.llvm_stackmaps` are **addressable stack slots**
  (LLVM StackMaps `Indirect` locations).
  - In particular, GC roots must **not** be `Register` locations.

Rationale:

- Without an unwind-based register context, we cannot read or update register-held roots for non-top frames.
- A moving/compacting GC must be able to update *all* roots, not just the topmost frame.

## Required codegen options (LLVM 18, x86_64)

LLVM *can* place statepoint GC roots in callee-saved registers under some settings.
The runtime has a verifier (`runtime-native/src/statepoint_verify.rs`) that rejects such stackmaps in
debug builds (and optionally in release builds via the `verify-statepoints` feature).

To force spills, ensure codegen uses:

- `llc-18 --fixup-allow-gcptr-in-csr=false` (preferred), and/or
- `llc-18 --fixup-max-csr-statepoints=0` (fallback / defense-in-depth)

This keeps statepoint GC roots out of registers even if other GC register options are enabled.

### `clang -flto` note

If machine code generation happens inside `clang-18` (e.g. `clang-18 -flto`), pass the equivalent
backend flag:

- `clang-18 -mllvm --fixup-allow-gcptr-in-csr=false -mllvm --fixup-max-csr-statepoints=0`

`native-js`'s LTO linking helpers do this automatically.

Additionally, stack walking requires frame pointers:

- `frame-pointer="all"` (LLVM function attribute), or `llc-18 --frame-pointer=all`

## Troubleshooting

If you see a stackmap entry like:

```
#4: Register R#12, size: 8
```

then one of the following is true:

1. Codegen did not pass `--fixup-max-csr-statepoints=0`, or
   (and/or did not set `--fixup-allow-gcptr-in-csr=false`), or
2. LLVM changed behavior / we upgraded LLVM and need to re-evaluate defaults.

Run the regression suite:

```
bash scripts/cargo_llvm.sh test -p runtime-native --test statepoint_register_roots_codegen
```

The tests:

- compile a matrix of IR functions with 0–64 GC roots,
- run `opt-18 -passes=rewrite-statepoints-for-gc` + `llc-18`,
- parse the resulting `.llvm_stackmaps` section,
- and assert (via the runtime verifier) that GC roots are not reported as `Register`.

## Base/Derived pointer pairs (derived / interior pointers)

LLVM statepoints encode GC relocation information as *(base, derived)* pairs:

- **base**: the address of the GC-managed object
- **derived**: the value actually in machine state (may be an interior pointer)

For non-interior pointers, LLVM typically emits **duplicate** base/derived locations (`base == derived`).

For interior pointers (`base != derived`), a moving collector must update the derived value to preserve the
interior offset:

```text
base_new    = relocate(base_old)
derived_new = base_new + (derived_old - base_old)
```

Null convention:

- If `base_old == 0` or `derived_old == 0`, the derived value stays null (`derived_new = 0`).

`runtime-native` implements this during stack walking:

- The stack walker visits only the **base** root slots (deduplicated) and lets the GC relocate them in-place.
- After relocating a base slot, it updates any derived slots that reference it using the formula above.

See `runtime-native/src/stackwalk_fp.rs` and `runtime-native/tests/stackwalk_fp.rs`.

### Stackmap location layout for `gc.statepoint` (LLVM 18)

For LLVM 18 statepoints lowered by `rewrite-statepoints-for-gc`, stackmap record `locations` follow a predictable
structure:

1. **3 constant header locations**:
   - `callconv` (call convention ID; commonly `0` for C, `8` for `fastcc`)
   - `flags` (the `gc.statepoint` `flags` immarg; a 2-bit mask `0..=3`; `1` when a `"gc-transition"` operand bundle is present)
   - `deopt_count` (number of `"deopt"` operand locations; GC ignores these but must skip them)
2. Then `deopt_count` deopt operand locations (not GC roots; can be `Constant`, `Indirect`, etc.).
3. Then **2 locations per `gc.relocate` call**: `(base, derived)`

`runtime-native::statepoints::StatepointRecord` enforces this layout (`LLVM18_STATEPOINT_HEADER_CONSTANTS = 3`),
and provides `gc_pairs() -> &[GcLocationPair]` for iterating the base/derived relocation pairs.
