# Native GC stackmaps: preserving `.llvm_stackmaps` in ELF binaries

LLVM statepoints emit GC stack map metadata into a loadable ELF section named
`.llvm_stackmaps` (and sometimes `.llvm_faultmaps`).
When linking PIE binaries we prefer to relocate these bytes into
`.data.rel.ro.llvm_stackmaps` so runtime relocations can be applied safely.

This metadata is **not referenced by code**, so link-time and post-link size
tools can accidentally remove it, breaking GC root discovery at runtime.

## Required sections

- `.llvm_stackmaps` (StackMap v3; required for statepoints GC)
- `.data.rel.ro.llvm_stackmaps` (hardened output location used by some link scripts)
- `.llvm_faultmaps` / `.data.rel.ro.llvm_faultmaps` (keep if present; patchpoint/faultmap metadata)

## Linker flags (ELF): keeping stackmaps under `--gc-sections`

Empirically (GNU ld 2.42 + LLVM/clang 18):

- Linking **without** `-Wl,--gc-sections` preserves `.llvm_stackmaps`.
- Linking **with** `-Wl,--gc-sections` **drops** `.llvm_stackmaps` unless we
  explicitly `KEEP` it.

To keep stackmaps while still using `--gc-sections`, pass a linker-script shim
that uses `KEEP(*(.llvm_stackmaps ...))`:

```bash
-Wl,--gc-sections -Wl,-T,vendor/ecma-rs/runtime-native/link/stackmaps.ld
```

This works with both **GNU ld** and **lld**, and also defines stable boundary symbols
for runtime discovery (see below).
The repository’s wrapper does this for you:

```bash
bash vendor/ecma-rs/scripts/native_link.sh -o myapp <objs...>
```

`native-js` users should prefer the Rust API helpers in `native_js::link`, which
always inject a linker-script fragment and export:

- `__start_llvm_stackmaps`
- `__stop_llvm_stackmaps`
- `__fastr_stackmaps_start`
- `__fastr_stackmaps_end`

For linking arbitrary programs against `runtime-native` (e.g. from C), see:

- `runtime-native/link/stackmaps.ld` (preferred) / `runtime-native/stackmaps.ld` (compat), and
- `runtime-native/README.md`

For the Linux AOT/PIE linking policy used by the native-js toolchain scripts, see:

- `scripts/native_js_link_linux.sh` (objcopy rewrite + lld PIE link)
- `scripts/test_stackmaps_pie_link.sh` (DT_TEXTREL regression test)

## PIE / textrels (Task 408 interaction)

`.llvm_stackmaps` contains absolute relocations into `.text`.

Naively linking a PIE with lld can fail (you’ll see `relocation R_X86_64_64 cannot be used ...`)
because the linker needs to apply relocations to stackmap records.

Naively linking a PIE with GNU ld can succeed but emit `DT_TEXTREL` warnings if
`.llvm_stackmaps` is mapped read-only.

To support PIE safely (without `DT_TEXTREL`), the stackmap section must be **writable during
relocation**.

The recommended approach (used by `native_js::link` and `scripts/native_js_link_linux.sh`) is to
relocate stackmaps into a RELRO-friendly section in the *input objects* using
`llvm-objcopy --rename-section`:

```bash
llvm-objcopy --rename-section \
  .llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents \
  <obj>
```

The more general `scripts/native_link.sh` uses `llvm-objcopy --set-section-flags` when
`ECMA_RS_NATIVE_PIE=1` and relies on the injected `runtime-native/link/stackmaps.ld` linker script
to place stackmaps in a writable/RELRO output section.

Current default policy in `native-js` and `native_link.sh`: **non-PIE** (`-no-pie`) unless the
caller opts into PIE explicitly (note: non-PIE disables main-executable ASLR on Linux).

## Stripping

Common stripping modes keep allocated sections like `.llvm_stackmaps`, but some options (notably
`llvm-strip --strip-sections`) remove the ELF section header table entirely, which breaks any
section-name based discovery.

Use the helper:

```bash
bash vendor/ecma-rs/scripts/native_strip.sh ./myapp
```

Or, with `llvm-strip` directly:

```bash
llvm-strip --strip-all \
  --keep-section=.llvm_stackmaps --keep-section=.llvm_stackmaps.* \
  --keep-section=.data.rel.ro.llvm_stackmaps --keep-section=.data.rel.ro.llvm_stackmaps.* \
  --keep-section=.llvm_faultmaps --keep-section=.llvm_faultmaps.* \
  --keep-section=.data.rel.ro.llvm_faultmaps --keep-section=.data.rel.ro.llvm_faultmaps.* \
  ./myapp
```

## Verification

Run:

```bash
bash vendor/ecma-rs/scripts/check_llvm_stackmaps.sh
```

It builds a minimal multi-object statepoint example and verifies the stackmaps
section survives linking and common strip modes.
