# `.llvm_stackmaps` composition across compilation units (`native-js`)

LLVM emits stackmap metadata for statepoints/patchpoints into the ELF section:

> `.llvm_stackmaps` (StackMap v3)

The `native-js` link pipeline preserves this section and exports two global symbols that delimit the
in-memory byte range:

- `__fastr_stackmaps_start`
- `__fastr_stackmaps_end`

The native runtime (`runtime-native`) reads that byte range to locate safepoints and enumerate GC
roots.

## Why compiled→compiled calls must be statepointed

LLVM stackmap records produced by statepoints are looked up by **return address**.

If the GC runs inside some callee (due to allocation or an explicit safepoint poll), the stack
walker must be able to recover GC roots for *every* frame, including all callers. The return
address stored in the callee’s frame points into the caller at the callsite.

If that callsite in the caller was emitted as a plain `call`, there is no stackmap record for the
return address and precise GC cannot recover the caller’s live roots.

Therefore `native-js` must emit **calls between compiled functions** as statepoints whenever the
callee may trigger GC. Until effect analysis is wired in, we conservatively assume compiled callees
are *may-GC* unless explicitly annotated `no_gc` / `leaf_no_alloc`.

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
`__fastr_stackmaps_start..__fastr_stackmaps_end` as a byte range that may contain **one or more**
StackMap v3 blobs.

The format is self-describing: each blob begins with a fixed-size header that includes the counts
needed to skip to the end of the blob. A robust decoder should:

1. Start at `__fastr_stackmaps_start`.
2. Parse one StackMap v3 blob.
3. Advance by the parsed blob length.
4. Repeat until reaching `__fastr_stackmaps_end`.

Linux regression tests covering both modes live in:

- `vendor/ecma-rs/native-js/tests/stackmaps_multimodule_linux.rs` (object-file section concatenation)
- `vendor/ecma-rs/native-js/tests/stackmaps_lto.rs` (LTO link path; stackmaps often merged into a single blob)
- `vendor/ecma-rs/native-js/tests/stackmaps_symbols_linux.rs` (start/end symbol bounds)
