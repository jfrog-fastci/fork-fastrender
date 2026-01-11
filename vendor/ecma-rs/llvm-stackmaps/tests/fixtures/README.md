# llvm-stackmaps test fixtures

This directory contains raw `.llvm_stackmaps` section bytes (`*.stackmaps.bin`) used by the
`llvm-stackmaps` crate tests.

## Regenerating the LLVM18 statepoint operand-bundle fixtures

The following fixtures are generated from the IR in `./ir/` using LLVM 18’s
`rewrite-statepoints-for-gc` pass:

- `deopt_bundle2.stackmaps.bin`
- `deopt_var.stackmaps.bin`
- `transition_bundle.stackmaps.bin`
- `deopt_transition.stackmaps.bin`

To regenerate them (overwriting the committed binaries):

```bash
bash vendor/ecma-rs/llvm-stackmaps/tests/fixtures/gen.sh
git diff
```

Requirements:

- `opt-18`
- `llc-18`
- `llvm-objcopy-18`

Note: the generator scripts pass `llc-18` fixup flags to avoid register-held GC roots
(`--fixup-allow-gcptr-in-csr=false` / `--fixup-max-csr-statepoints=0`) so the fixtures match the
runtime-native stack-walking contract and remain stable across LLVM register allocation changes.

## `llvm18_stackmaps/` fixtures

`fixtures/llvm18_stackmaps/*.stackmaps.bin` are separate fixtures extracted from a *linked* ELF
produced by LLVM 18 (function addresses resolved), used by callsite-PC mapping tests.

To regenerate them (overwriting the committed binaries):

```bash
bash vendor/ecma-rs/llvm-stackmaps/tests/fixtures/llvm18_stackmaps/gen.sh
git diff
```
