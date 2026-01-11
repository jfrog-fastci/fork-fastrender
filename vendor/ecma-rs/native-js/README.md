# native-js

`native-js` is the LLVM-backed code generation crate for `ecma-rs`.

It is intended to compile a **strict subset of TypeScript** into **LLVM IR**
(and, eventually, object files / binaries) as part of the native
TypeScript→LLVM pipeline.

At the moment, the crate is a **skeleton**: it wires up LLVM and defines the
public API surface that future TS/HIR lowering will target. `Compiler::compile`
currently returns `NativeJsError::Unimplemented`.

This crate is not a general-purpose JavaScript engine and it does not try to
support the full JavaScript/TypeScript language.

## What it does

At a high level, the native pipeline looks like:

```
TypeScript source
  → parse-js (parser)
  → hir-js (lowering)
  → typecheck-ts (types + diagnostics)
  → native-js (LLVM IR generation)
  → LLVM (opt/codegen) + linker
```

`native-js` starts from the typechecked program and produces an LLVM module
representing the program (usually a single entry function plus any referenced
helpers/runtime stubs).

## Build prerequisites

### LLVM 18

`native-js` uses the LLVM 18 C API via Rust bindings (e.g. `llvm-sys`/`inkwell`),
so you must have an LLVM 18 installation available at build time.

On Ubuntu, install:

```bash
sudo apt-get install -y llvm-18 llvm-18-dev clang-18 lld-18
```

Then set:

```bash
export LLVM_SYS_180_PREFIX=/usr/lib/llvm-18
export PATH="/usr/lib/llvm-18/bin:$PATH"
```

You can also run the dependency checker:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/check_system.sh
#
# Or, from within vendor/ecma-rs/:
bash scripts/check_system.sh
```

### Wrapper scripts (recommended in agent environments)

LLVM builds are memory-hungry. In this repo, prefer the wrapper which increases
the process memory limit and auto-detects LLVM 18:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p native-js
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib
#
# Or, from within vendor/ecma-rs/:
bash scripts/cargo_llvm.sh build -p native-js
bash scripts/cargo_llvm.sh test -p native-js --lib
```

## Public API overview (current)

The API is intentionally small and currently consists of:

- `CodeGen`: a minimal façade around `inkwell` that applies LLVM function
  attributes required for deterministic stack walking (used by the planned
  precise GC integration).
- `Compiler`: entry point (configured with `CompileOptions`)
- `Compiler::compile() -> Result<(), NativeJsError>`: compilation entrypoint
  (currently unimplemented)
- `CompileOptions`: codegen configuration
- `OptLevel`: optimization level (`O0`/`O1`/`O2`/`O3`/`Os`/`Oz`)
- `EmitKind`: artifact kind (`LlvmIr`, `Object`, `Assembly`)
- `NativeJsError`: error type (includes `Unimplemented` and `Llvm(String)`)

Example (API shape):

```rust
use native_js::{CompileOptions, Compiler, EmitKind, OptLevel};

let compiler = Compiler::new(CompileOptions {
  opt_level: OptLevel::O2,
  emit: EmitKind::Object,
  target: None,
  debug: false,
});

// Note: currently returns NativeJsError::Unimplemented.
compiler.compile()?;
```

> Note: `native_js::emit` and `native_js::codegen` exist, but are currently stubs.

Example (generating LLVM IR via `CodeGen`):

```rust
use inkwell::context::Context;
use native_js::CodeGen;

let context = Context::create();
let cg = CodeGen::new(&context, "example");

cg.define_trivial_function("trivial");

// Prints IR that includes the stack-walking-related function attributes.
println!("{}", cg.module_ir());
```

## GC stack walking (current invariant)

The native runtime is expected to perform **precise GC** using LLVM statepoints.
Even with LLVM stack maps, the runtime still needs to reliably walk stack frames
and recover return addresses.

`native-js` currently enforces a conservative invariant on generated functions:

- `frame-pointer="all"` (keep frame pointers so a frame-chain walker can work)
- `disable-tail-calls="true"` (avoid tail-call elimination collapsing frames)

See [`docs/gc_stack_walking.md`](./docs/gc_stack_walking.md) for details.

## Diagnostics (codes)

When `native-js` reports user-facing diagnostics, they use stable code strings
with the `NJS####` prefix (see `vendor/ecma-rs/docs/diagnostic-codes.md` for the
repo-wide policy).

The intended place to define new native-js diagnostics is
`native-js/src/codes.rs`.

## Supported TypeScript subset (intended)

We compile a **strict subset** of TypeScript. The compiler is intended to error
on constructs that TypeScript (`tsc`) would normally accept.

> Note: strict-mode validation is planned to live under `native_js::strict` and
> is not implemented yet.

### Rejected (hard errors)

- `any` type (explicit or inferred)
- Type assertions that “lie” (`x as T` where `x` is not `T`)
- Non-null assertions on nullable values (`x!` where `x` might be `null`/`undefined`)
- `eval()` / `new Function()`
- `with` statement
- `arguments` object
- Prototype mutation after construction
- Computed property access with non-constant keys (in strict paths)
- `Proxy` (unsupported or heavily restricted)

### Restricted

- Union types: allowed, but lowered to tag-checked dispatch
- `unknown`: allowed, but requires explicit narrowing before use
- Dynamic property access: allowed only via slower paths (may warn)

### Allowed (intended to work end-to-end)

- Primitive types (`number`, `boolean`, `string`, `null`, `undefined`)
- Interfaces, type aliases, generics
- Classes (single inheritance; treated nominally for optimization purposes)
- Tagged/discriminated unions
- Literal types and `as const`
- Tuples
- `readonly` modifiers
- `async`/`await` and `Promise`-based code
- ES modules (`import`/`export`)

## Debugging tips

### Smoke test LLVM wiring (`llvm_ir_sanity`)

The crate includes a small unit test that constructs and verifies a trivial LLVM
module. This is a good first step when debugging LLVM environment issues:

```bash
# From repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib llvm_ir_sanity
```

### Emit IR and run the verifier

Once real codegen exists, the fastest debug loop is usually:

1. Emit textual IR (`.ll`) from the compiler.
   - In-process, you can always use `inkwell` (or `native_js::CodeGen`) directly:

     ```rust
     // Given an inkwell::module::Module
     let ir = module.print_to_string().to_string();
     std::fs::write("out.ll", ir)?;
     ```
2. Run LLVM’s verifier:

```bash
opt-18 -verify -disable-output out.ll
```

If you only have unversioned tools, ensure they’re LLVM 18:

```bash
llvm-config --version
```

### Common LLVM environment issues

- **Build error: “No suitable version of LLVM found”**
  - Ensure `llvm-18-dev` is installed.
  - Ensure `LLVM_SYS_180_PREFIX` points at the LLVM 18 prefix (contains `bin/`,
    `include/`, and `lib/`).
- **Using the wrong LLVM version**
  - If you have multiple LLVM versions installed, make sure the LLVM 18 tools
    are on `PATH` (e.g. `/usr/lib/llvm-18/bin`).
- **Linker/runtime errors due to missing LLVM shared libs**
  - Some setups require `LD_LIBRARY_PATH="$LLVM_SYS_180_PREFIX/lib:$LD_LIBRARY_PATH"`.
    (This depends on how LLVM was installed and whether it’s linked statically.)

### Always keep a backtrace handy

```bash
# From repo root:
RUST_BACKTRACE=1 bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib
```
