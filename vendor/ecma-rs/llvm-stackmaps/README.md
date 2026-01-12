# llvm-stackmaps

LLVM StackMap v3 (`.llvm_stackmaps`) parser + debug tooling.

This crate is used for offline inspection/regression testing of stackmaps emitted by LLVM’s
`gc.statepoint` infrastructure.

## CLI tools

### `verify_stackmaps`

Offline verifier for `.llvm_stackmaps` sections.

Inputs:

- **ELF file** (`verify_stackmaps --elf <path>` or `verify_stackmaps <path>`)  
  The verifier extracts `.data.rel.ro.llvm_stackmaps` / `.llvm_stackmaps` / `llvm_stackmaps` (with
  symbol-based fallback) and validates the contents.
- **Raw section bytes** (`verify_stackmaps --raw <path>`)  
  A byte-for-byte dump of the section payload, e.g. produced by:
  `llvm-objcopy --dump-section ".llvm_stackmaps=out.stackmaps.bin" a.o`

Output:

- A human summary is written to **stderr**
- A deterministic JSON report is written to **stdout**
- Exit code is **non-zero** on verification failure

Examples:

```bash
# Verify raw section bytes (like the committed test fixtures)
cargo run -p llvm-stackmaps --bin verify_stackmaps -- \
  --raw vendor/ecma-rs/llvm-stackmaps/tests/fixtures/deopt_var.stackmaps.bin

# Verify a linked executable/object (extract `.llvm_stackmaps` from ELF)
cargo run -p llvm-stackmaps --bin verify_stackmaps -- \
  --elf ./a.out
```

### `dump_stackmaps`

Minimal stackmap dumper / callsite lookup helper:

```bash
cargo run -p llvm-stackmaps --bin dump_stackmaps -- ./a.out --pc 0x1234
```
