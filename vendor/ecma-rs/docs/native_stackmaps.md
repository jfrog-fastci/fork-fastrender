# Native GC stackmaps: preserving `.llvm_stackmaps` in ELF binaries

LLVM statepoints emit GC stack map metadata into a loadable ELF section named
`.llvm_stackmaps` (and sometimes `.llvm_faultmaps`).

This metadata is **not referenced by code**, so link-time and post-link size
tools can accidentally remove it, breaking GC root discovery at runtime.

## Required sections

- `.llvm_stackmaps` (required)
- `.llvm_faultmaps` (keep if present)

## Linker flags (ELF)

Empirically (GNU ld 2.42 + LLVM/clang 18):

- Linking **without** `-Wl,--gc-sections` preserves `.llvm_stackmaps`.
- Linking **with** `-Wl,--gc-sections` **drops** `.llvm_stackmaps` unless we
  explicitly `KEEP` it.

To keep stackmaps while still using `--gc-sections` with **GNU ld**, pass the
linker-script shim:

```bash
-Wl,--gc-sections -Wl,-T,vendor/ecma-rs/scripts/keep_llvm_stackmaps.ld
```

The repository’s default wrapper does this for you:

```bash
bash vendor/ecma-rs/scripts/native_link.sh -o myapp <objs...>
```

## PIE / textrels (Task 408 interaction)

`.llvm_stackmaps` contains absolute relocations into `.text`.

- **lld** rejects PIE links containing these relocations (you’ll see
  `relocation R_X86_64_64 cannot be used against symbol ...; recompile with -fPIC`).
- **GNU ld** can link PIE, but may produce `DT_TEXTREL` warnings.

Current policy: link native AOT binaries as **non-PIE** (`-no-pie`) until the
section/relocation story is finalized.

## Stripping

Common stripping modes keep `.llvm_stackmaps`, but some options (notably
`llvm-strip --strip-sections`) remove the ELF section header table entirely,
which breaks section-name based discovery.

Use the helper:

```bash
bash vendor/ecma-rs/scripts/native_strip.sh ./myapp
```

Or, with `llvm-strip` directly:

```bash
llvm-strip --strip-all \
  --keep-section=.llvm_stackmaps --keep-section=.llvm_stackmaps.* \
  --keep-section=.llvm_faultmaps --keep-section=.llvm_faultmaps.* \
  ./myapp
```

## Verification

Run:

```bash
bash vendor/ecma-rs/scripts/check_llvm_stackmaps.sh
```

It builds a minimal multi-object statepoint example and verifies `.llvm_stackmaps`
survives linking and common strip modes.
