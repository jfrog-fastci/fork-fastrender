# `.llvm_stackmaps` composition across compilation units (`native-js`)

LLVM emits stackmap metadata for statepoints/patchpoints into the ELF section:

> `.llvm_stackmaps` (StackMap v3)

The `native-js` link pipeline preserves this data and exports global symbols that delimit the
in-memory byte range (see `native-js/src/link.rs` and `runtime-native/link/` linker fragments):

- `runtime-native/link/stackmaps_nopie.ld` (non-PIE)
- `runtime-native/link/stackmaps.ld` (PIE, lld-friendly; emits dedicated `.data.rel.ro.llvm_*` output sections)
- `runtime-native/link/stackmaps_gnuld.ld` (GNU ld PIE hardening; avoids RWX segments)

- Canonical:
  - `__start_llvm_stackmaps`
  - `__stop_llvm_stackmaps`
- Aliases (legacy / tooling convenience):
  - `__stackmaps_start` / `__stackmaps_end`
  - `__fastr_stackmaps_start` / `__fastr_stackmaps_end`
  - `__llvm_stackmaps_start` / `__llvm_stackmaps_end`

The native runtime (`runtime-native`) reads that byte range to locate safepoints and enumerate GC
roots.

## `--gc-sections` and linker quirks (Linux)

LLVM stackmap sections are typically **unreferenced** by code/data relocations. When the final link
uses section GC (`-Wl,--gc-sections`), linkers will drop `.llvm_stackmaps` unless it is explicitly
retained.

`native-js` solves this by always injecting a linker script fragment that:

* keeps the relevant stackmap/faultmap sections so they survive `--gc-sections`:
  * non-PIE: keep `.llvm_{stackmaps,faultmaps}*` (and `.data.rel.ro.llvm_*` if present)
  * PIE: keep `.data.rel.ro.llvm_{stackmaps,faultmaps}*` (after rewriting input objects via
    `llvm-objcopy --rename-section`)
* defines stable boundary symbols (`__start_llvm_stackmaps` / `__stop_llvm_stackmaps` and aliases).

Notes:

* Some environments opt into `-fuse-ld=mold` for faster Rust links, but mold does **not** support GNU
  ld linker scripts (`SECTIONS`/`KEEP`/`INSERT`). When injecting the fragment, use lld
  (`-fuse-ld=lld`) or GNU ld.
* GNU ld PIE/DSO builds should use `runtime-native/link/stackmaps_gnuld.ld` (instead of inserting
  stackmaps “after .text”) to avoid producing an RWX LOAD segment. `native_js::link` and
  `scripts/native_link.sh` handle this automatically.

## Why compiled→compiled calls must be statepointed

LLVM stackmap records produced by statepoints are looked up by **return address**.

If the GC runs inside some callee (due to allocation or an explicit safepoint poll), the stack
walker must be able to recover GC roots for *every* frame, including all callers. The return
address stored in the callee’s frame points into the caller at the callsite.

If that callsite in the caller was emitted as a plain `call`, there is no stackmap record for the
return address and precise GC cannot recover the caller’s live roots.

Therefore `native-js` must emit **calls between compiled functions** as statepoints whenever the
callee may trigger GC. Until effect analysis is wired in, we conservatively assume compiled callees
are *may-GC* unless explicitly annotated as a GC leaf via LLVM’s `"gc-leaf-function"` attribute
(future: `no_gc` / `leaf_no_alloc` derived from effect analysis).

## Statepoint IDs (`StackMapRecord.patchpoint_id`)

LLVM StackMap v3 callsite records include a `patchpoint_id: u64` field (the first field in each
record). For `llvm.experimental.gc.statepoint`, this is the `i64` **ID argument** passed to the
intrinsic.

Important: for `native-js` and `runtime-native`, the **callsite return address** is the real
identifier used for stackmap lookup during stack walking. The patchpoint/statepoint ID is *not*
used for lookup.

`runtime-native` does not rely on `patchpoint_id` for statepoint detection: LLVM supports overriding
the statepoint ID via the `"statepoint-id"` callsite attribute when using
`rewrite-statepoints-for-gc`.

Instead, `runtime-native` identifies statepoints by the (LLVM 18, empirically stable) **record
layout**:

- the first 3 locations are constant header entries (`callconv`, `flags`, `deopt_count`), followed by
- `deopt_count` deopt operand locations (if any), followed by
- `(base, derived)` relocation pairs (two locations per `gc.relocate`).

Note: GC roots are encoded as `(base, derived)` pairs. For interior pointers (`base != derived`),
relocation must preserve the observed base→derived delta (see
[`Derived / interior pointer relocation pairs`](#derived--interior-pointer-relocation-pairs)).

The default `rewrite-statepoints-for-gc` ID is still useful for debugging:

- LLVM 18 uses `0xABCDEF00` (decimal `2882400000`) when a callsite does not specify an explicit
  `"statepoint-id"` directive.
- `native-js` adopts the same convention for any manually emitted `gc.statepoint`.

When using `"statepoint-id"` directives, stack walking and verification continue to work because
they key off the callsite return address + record layout, not the ID.

See also: `vendor/ecma-rs/docs/llvm_statepoint_directives.md`.

## Derived / interior pointer relocation pairs

LLVM encodes statepoint GC roots as a sequence of `(base, derived)` **relocation pairs** in the
stackmap record.

- For a normal GC root (a base object pointer), `base == derived`:
  - the `gc.relocate(token, idx, idx)` uses the same `"gc-live"` index for both operands, and
  - the stackmap record contains two identical `Location`s that refer to the same spill slot.
- For an interior pointer (a derived pointer into an object), `base != derived`:
  - `gc.relocate(token, base_idx, derived_idx)` uses two different `"gc-live"` indices, and
  - the stackmap record contains **two distinct spill slots** (typically `Location::Indirect` with
    different `offset`s).

Runtime relocation must treat derived pointers as dependent on their base:

```
delta = old_derived - old_base
new_derived = new_base + delta
```

The regression test `vendor/ecma-rs/native-js/tests/stackmaps_derived_pairs.rs` locks down the
end-to-end contract:

- `native-js` emits a real `base != derived` relocation pair that survives codegen into
  `.llvm_stackmaps`.
- `runtime-native::relocate_derived_pairs` preserves the interior-pointer delta when updating slots.

## Two observed composition modes

When linking multiple compilation units, `.llvm_stackmaps` is **not guaranteed** to be a single
StackMap blob.

### 1) Object-file link (ELF section concatenation)

If you compile multiple independent LLVM modules to **separate object files** (`.o`) and link them
normally, the linker typically **concatenates same-named input sections**:

```
.llvm_stackmaps = [ StackMapBlob(module A) ][ StackMapBlob(module B) ] ...
```

That means the output `.llvm_stackmaps` section contains **multiple independent StackMap v3
tables**, each starting with its own header (Version=3).

### 2) Bitcode + `clang -flto` (merged StackMap table)

If you link multiple LLVM **bitcode** modules (`.bc`) under **full LTO** (`clang -flto`), LLVM
typically merges the stackmaps into a **single** StackMap v3 table:

```
.llvm_stackmaps = [ StackMapBlob(merged; NumFunctions >= N) ]
```

## Runtime requirement: parse blobs until end

Because both layouts exist in practice, the runtime stackmap decoder MUST treat
`__start_llvm_stackmaps..__stop_llvm_stackmaps` (or any of its aliases) as a byte range that may
contain **one or more** StackMap v3 blobs.

The format is self-describing: each blob begins with a fixed-size header that includes the counts
needed to skip to the end of the blob. A robust decoder should:

1. Start at `__start_llvm_stackmaps`.
2. Parse one StackMap v3 blob.
3. Advance by the parsed blob length.
4. Repeat until reaching `__stop_llvm_stackmaps`.

Linux regression tests covering both modes live in:

- `vendor/ecma-rs/native-js/tests/stackmaps_multimodule_linux.rs` (object-file section concatenation)
- `vendor/ecma-rs/native-js/tests/stackmaps_lto.rs` (LTO link path; stackmaps often merged into a single blob)
- `vendor/ecma-rs/native-js/tests/stackmaps_symbols_linux.rs` (start/end symbol bounds)
