# Native GC stackmaps: preserving `.llvm_stackmaps` in ELF binaries

LLVM statepoints emit GC stack map metadata into a loadable ELF section named
`.llvm_stackmaps` (and sometimes `.llvm_faultmaps`).
When linking PIE binaries we prefer to relocate these bytes into
`.data.rel.ro.llvm_stackmaps` in the *input objects* so runtime relocations can be applied safely
(the linker fragment then keeps those inputs under `--gc-sections` and exports stable range
symbols).

## Multi-object linking: concatenated stackmap blobs

Each object file that uses statepoints emits its own StackMap v3 blob (starting with a
`version=3` header) into `.llvm_stackmaps`. When linking multiple objects, ELF linkers concatenate
the section payloads, so the final output section can contain **multiple independent v3 blobs
back-to-back**, with optional alignment padding between them.

Note: `llvm-readobj --stackmap` only prints the first blob in a concatenated section. The runtime
must parse all blobs (see `runtime_native::stackmaps::StackMaps::parse`).

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
# Non-PIE executables (default policy in this repo):
-Wl,--gc-sections -Wl,-T,vendor/ecma-rs/runtime-native/link/stackmaps_nopie.ld

# PIE/shared objects (lld-friendly; see PIE section below):
-Wl,--gc-sections -Wl,-T,vendor/ecma-rs/runtime-native/link/stackmaps.ld
```

Both fragments work with **GNU ld** and **lld**, but they target different layouts:

- `runtime-native/link/stackmaps_nopie.ld`: non-PIE output; emits a dedicated `.llvm_stackmaps`
   output section and anchors at `INSERT AFTER .text;` (which is always present).
- `runtime-native/link/stackmaps.ld`: PIE/lld-friendly; expects stackmaps/faultmaps to be rewritten
  in the *input objects* as `.data.rel.ro.llvm_*` (see below), and emits dedicated
  `.data.rel.ro.llvm_stackmaps` / `.data.rel.ro.llvm_faultmaps` output sections inserted before
  `.bss` (outside the RELRO range) to avoid lld's RELRO contiguity checks.

For GNU ld **PIE** builds where stackmaps must be writable for relocations,
`scripts/native_link.sh` selects `runtime-native/link/stackmaps_gnuld.ld` automatically to keep
stackmaps/faultmaps in dedicated `.data.rel.ro.llvm_*` output sections.

It also defines stable boundary symbols for runtime discovery (see below).

> Note: the stackmaps fragments are injected via the GNU ld/LLD linker-script
> `INSERT` mechanism. If you use a linker that
> doesn't support `INSERT` (some
> alternative linkers do not), switch to GNU ld or lld (e.g. `clang-18 -fuse-ld=lld-18`),
> or avoid `--gc-sections`.
The repository’s wrapper does this for you:

```bash
bash vendor/ecma-rs/scripts/native_link.sh -o myapp <objs...>
```

## Optional: identical code folding (ICF)

When linking with **lld**, you can optionally enable identical code folding:

```bash
-Wl,--icf=all
```

This is compatible with LTO and `--gc-sections` as long as stackmaps are still kept via the linker
script fragment.

Note: ICF can fold identical functions and produce **duplicate callsite PCs** in the final
`.llvm_stackmaps` section (two records with the same `function_address + instruction_offset`).
The parsers in this repository (`runtime_native::stackmaps::StackMaps` and `llvm_stackmaps::StackMaps`)
deduplicate such entries when the records are identical, and reject conflicting duplicates.

`native-js` users should prefer the Rust API helpers in `native_js::link`, which
always inject a linker-script fragment and export:

- `__stackmaps_start`
- `__stackmaps_end`
- `__start_llvm_stackmaps`
- `__stop_llvm_stackmaps`
- `__fastr_stackmaps_start`
- `__fastr_stackmaps_end`
- `__llvm_stackmaps_start`
- `__llvm_stackmaps_end`

For linking arbitrary programs against `runtime-native` (e.g. from C), see:

- `runtime-native/link/stackmaps_nopie.ld` (non-PIE)
- `runtime-native/link/stackmaps.ld` (PIE, lld-friendly)
- `runtime-native/link/stackmaps_gnuld.ld` (GNU ld PIE)
- `runtime-native/stackmaps.ld` (compat), and
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

Another hardening pitfall: if stackmaps are made writable for PIE relocation and a linker script
inserts the output section immediately after `.text` (common `INSERT AFTER .text` fragments), some
linkers (notably GNU ld) can merge that writable stackmaps section into the `.text` LOAD segment,
producing an **RWX** segment. The repo avoids this by anchoring stackmaps in the RELRO/data region
or data segment (never immediately after `.text`):

- lld: `runtime-native/link/stackmaps.ld` inserts stackmaps/faultmaps before `.bss` (outside the RELRO range).
- GNU ld + PIE: the wrappers select `runtime-native/link/stackmaps_gnuld.ld` automatically.

To support PIE safely (without `DT_TEXTREL`), the stackmap section must be **writable during
relocation**.

The recommended approach (used by `native_js::link` and `scripts/native_js_link_linux.sh`) is to
relocate stackmaps (and faultmaps, if present) into RELRO-friendly sections in the *input objects*
using `llvm-objcopy --rename-section`:

```bash
llvm-objcopy \
  --rename-section .llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents \
  --rename-section .llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents \
  <obj>
```

The more general `scripts/native_link.sh` rewrites `.llvm_stackmaps` / `.llvm_faultmaps` into
`.data.rel.ro.llvm_*` in the input objects (via `objcopy`/`llvm-objcopy --rename-section`) when
needed and injects the appropriate linker fragment:

- non-PIE: `runtime-native/link/stackmaps_nopie.ld`
- PIE (lld): `runtime-native/link/stackmaps.ld`
- PIE (GNU ld): `runtime-native/link/stackmaps_gnuld.ld`

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
