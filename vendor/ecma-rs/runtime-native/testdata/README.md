# runtime-native stackmap fixtures

This directory contains LLVM-produced fixtures used by integration tests in
`runtime-native/tests/`.

## `statepoint_fixture.ll` / `statepoint_fixture.o`

`statepoint_fixture.ll` is a minimal LLVM IR function that explicitly calls
`@llvm.experimental.gc.statepoint` with a `"gc-live"` operand bundle containing
two GC pointers.

`statepoint_fixture.o` is the corresponding x86_64 Linux object file produced by
LLVM 18. It is checked in so `cargo test` does **not** need to invoke LLVM tools
and stays deterministic.

### Regenerating (LLVM 18)

From this directory:

```bash
bash regenerate_statepoint_fixture.sh
```

Or manually:

```bash
llc-18 -O0 -filetype=obj -o statepoint_fixture.o statepoint_fixture.ll
```

If you don’t have versioned binaries, use `llc` but ensure it is LLVM 18:

```bash
llc --version
```

### What the tests validate

`runtime-native/tests/stackmap_fixture.rs` loads `statepoint_fixture.o`,
extracts the `.llvm_stackmaps` section, parses it with the runtime’s stackmap
parser, enumerates the GC root slots described by the statepoint record, and
asserts that the moving (minor) GC updates the pointers stored in those slots
after evacuation.
