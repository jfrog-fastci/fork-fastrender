# ELF x86_64: PIE linking and `.llvm_stackmaps` (`DT_TEXTREL`)

On Linux x86_64, LLVM stack maps are emitted into a section named `.llvm_stackmaps`.
The stack map format contains absolute `FunctionAddress` entries.

When producing a PIE (`ET_DYN`), those absolute addresses are *not known at static link time* and must be relocated by the dynamic loader at runtime.

If `.llvm_stackmaps` ends up in a read-only `PT_LOAD` segment (the default with GNU ld), the loader would need to temporarily make that segment writable to apply relocations. The linker therefore emits `DT_TEXTREL` and prints warnings like:

```
relocation against `fooB' in read-only section `.llvm_stackmaps'
creating DT_TEXTREL in a PIE
```

This is undesirable (hardening, some environments reject textrels, etc).

## Minimal reproduction (2 objects: `fooA`/`fooB`)

The repo already contains a minimal example under `.investigation/stackmap/`:

- `mod_a.ll` / `mod_b.ll`: each defines one function, emits stack maps
- `callee.c`: provides `callee()` + `main()`

### Build objects

```bash
llc -filetype=obj .investigation/stackmap/mod_a.ll -o /tmp/mod_a.o
llc -filetype=obj .investigation/stackmap/mod_b.ll -o /tmp/mod_b.o
clang -c .investigation/stackmap/callee.c -o /tmp/callee.o
```

### Link with GNU ld (default `clang` PIE)

```bash
clang /tmp/callee.o /tmp/mod_a.o /tmp/mod_b.o -o /tmp/linked_default
```

Expected:

- Warnings shown above
- `readelf -d /tmp/linked_default | grep TEXTREL` shows `DT_TEXTREL`
- `readelf -r /tmp/linked_default` shows `R_X86_64_RELATIVE` relocations targeting offsets inside `.llvm_stackmaps`
- `readelf -l /tmp/linked_default` shows `.llvm_stackmaps` mapped in a read-only `PT_LOAD` segment (with `.rodata`)

### `-no-pie` (works, but drops PIE/ASLR)

```bash
clang -no-pie /tmp/callee.o /tmp/mod_a.o /tmp/mod_b.o -o /tmp/linked_no_pie
```

Expected:

- no `DT_TEXTREL`
- no dynamic relocations against `.llvm_stackmaps` (the addresses are fully resolved at static link time)

### `-Wl,-z,relro -Wl,-z,now` (does *not* fix)

Hardening flags do not help by themselves if `.llvm_stackmaps` remains read-only:

```bash
clang -Wl,-z,relro -Wl,-z,now /tmp/callee.o /tmp/mod_a.o /tmp/mod_b.o -o /tmp/linked_relro_now
```

Still emits `DT_TEXTREL`.

### lld (fails without a workaround)

```bash
clang -fuse-ld=lld-18 /tmp/callee.o /tmp/mod_a.o /tmp/mod_b.o -o /tmp/linked_lld
```

Expected:

- lld errors out because the `.llvm_stackmaps` section contains absolute relocations that are not acceptable for PIE when the section is read-only.

## Recommended strategy (PIE, no textrels): make `.llvm_stackmaps` writable *before* linking

The fix is to ensure the ELF section has `SHF_WRITE` set, so the linker places it in a writable segment.
The dynamic loader can then apply relocations without textrels:

```bash
llvm-objcopy --set-section-flags=.llvm_stackmaps=alloc,contents,load,data /tmp/mod_a.o
llvm-objcopy --set-section-flags=.llvm_stackmaps=alloc,contents,load,data /tmp/mod_b.o
```

Then link normally (GNU ld or lld), optionally with hardening flags:

```bash
clang -Wl,-z,relro -Wl,-z,now /tmp/callee.o /tmp/mod_a.o /tmp/mod_b.o -o /tmp/linked_pie_no_textrel
# or:
clang -fuse-ld=lld-18 -Wl,-z,relro -Wl,-z,now /tmp/callee.o /tmp/mod_a.o /tmp/mod_b.o -o /tmp/linked_pie_no_textrel
```

Expected:

- no warnings
- no `DT_TEXTREL`
- `readelf -r` still shows `R_X86_64_RELATIVE` relocations for `.llvm_stackmaps` offsets (so `FunctionAddress` values are correct at runtime)
- `.llvm_stackmaps` is mapped in a writable `PT_LOAD` segment

This repo provides a ready-to-use wrapper for this step:

- `vendor/ecma-rs/scripts/clang_link_stackmaps.sh`

## Notes

- This is a Linux ELF x86_64 workaround. Other platforms are out of scope for now.
- With the `SHF_WRITE` approach, `.llvm_stackmaps` remains writable at runtime. If we want it to become read-only after relocations, we can follow up later with a custom linker script / section renaming so it lands inside the `GNU_RELRO` range.

