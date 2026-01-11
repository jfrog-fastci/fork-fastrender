# native-js

`native-js` is the LLVM-backed code generation crate for `ecma-rs`.

It is intended to compile a **strict subset of TypeScript** into **LLVM IR**
(and, eventually, object files / binaries) as part of the native
TypeScript→LLVM pipeline.

At the moment, the crate is a **skeleton**: it wires up LLVM and defines the
public API surface that future TS/HIR lowering will target. `Compiler::compile`
currently returns `NativeJsError::Unimplemented`.

For bring-up and testing, the crate also includes a small `parse-js`-driven LLVM
IR emitter (`compile_typescript_to_llvm_ir`) that supports a tiny expression-only
subset. `native-js-cli` uses this path to compile and run small snippets.

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

> Note: the long-term plan is typechecked/HIR-based codegen, but the currently
> implemented `compile_typescript_to_llvm_ir` entrypoint is **parse-js-only** and
> does not perform type checking.

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
- `strict::validate(...)`: strict TypeScript-subset validator that rejects
  unsafe constructs (`any`, `eval`, type assertions, etc) even if the TypeScript
  typechecker accepts them.
- `compile_typescript_to_llvm_ir(&str, CompileOptions) -> Result<String, NativeJsError>`:
  compile a single TypeScript module to textual LLVM IR (very small subset; used
  by `native-js-cli`).
- `Compiler`: entry point (configured with `CompileOptions`)
- `Compiler::compile() -> Result<(), NativeJsError>`: compilation entrypoint
  (currently unimplemented)
- `CompileOptions`: codegen configuration
- `OptLevel`: optimization level (`O0`/`O1`/`O2`/`O3`/`Os`/`Oz`)
- `EmitKind`: artifact kind (`LlvmIr`, `Object`, `Assembly`)
- `NativeJsError`: error type (includes parse errors, codegen errors, and
  `Unimplemented` for the not-yet-implemented typechecked backend)

Example (API shape):

```rust
use native_js::{CompileOptions, Compiler, EmitKind, OptLevel};

let mut opts = CompileOptions::default();
opts.opt_level = OptLevel::O2;
opts.emit = EmitKind::Object;
opts.debug = false;

let compiler = Compiler::new(opts);
// Note: currently returns NativeJsError::Unimplemented.
compiler.compile()?;
```

> Note: the long-term typechecked/HIR backend is not implemented yet.
> `native_js::codegen` currently contains the minimal `parse-js`-driven emitter
> used by `compile_typescript_to_llvm_ir`. `native_js::emit` is a placeholder
> module for future artifact emission helpers.

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

Example (compiling TS to textual IR with the minimal emitter):

```rust
use native_js::{compile_typescript_to_llvm_ir, CompileOptions};

let ir = compile_typescript_to_llvm_ir("console.log(1 + 2);", CompileOptions::default())?;
std::fs::write("out.ll", ir)?;
```

## GC stack walking (current invariant)

The native runtime is expected to perform **precise GC** using LLVM statepoints.
Even with LLVM stack maps, the runtime still needs to reliably walk stack frames
and recover return addresses.

`native-js` currently enforces a conservative invariant on generated functions:

- `frame-pointer="all"` (keep frame pointers so a frame-chain walker can work)
- `disable-tail-calls="true"` (avoid tail-call elimination collapsing frames)

See [`docs/gc_stack_walking.md`](./docs/gc_stack_walking.md) for details.

All generated functions are also marked with the LLVM GC strategy attribute
(`gc "coreclr"`). See [`docs/llvm_gc_strategy.md`](./docs/llvm_gc_strategy.md) for
the rationale and how to change it.

For the broader runtime ABI + GC/statepoints integration plan, see:

- [`docs/runtime-native.md`](../docs/runtime-native.md)
- [`runtime-native/README.md`](../runtime-native/README.md)

## Diagnostics (codes)

When `native-js` reports user-facing diagnostics, they use stable code strings
with the `NJS####` prefix (see [`docs/diagnostic-codes.md`](../docs/diagnostic-codes.md)
for the repo-wide policy).

The intended place to define new native-js diagnostics is
[`src/codes.rs`](./src/codes.rs).

## Minimal LLVM IR emitter (`compile_typescript_to_llvm_ir`)

`compile_typescript_to_llvm_ir` currently implements a very small, `parse-js`-only
compiler that lowers a single TypeScript module to textual LLVM IR.

It exists to make it easy to debug the LLVM plumbing and basic lowering logic,
and is the backend used by `native-js-cli`.

The input is always parsed as a **TypeScript module**:

- `parse-js` `Dialect::Ts`
- `parse-js` `SourceType::Module`

Only `CompileOptions::builtins` is currently honored by this path; the remaining
fields are reserved for the eventual LLVM-backed backend.

### Supported subset (current)

- Top-level statements:
  - empty statements (`;`)
  - expression statements (`expr;`)
  - variable declarations (`const`/`let`/`var`) with simple identifier bindings
- Expressions:
  - number / boolean / string / null literals
  - identifiers:
    - local bindings introduced by `const`/`let`/`var`
    - globals: `undefined`, `NaN`, `Infinity`
  - unary operators:
    - `-` / `+` (numbers only)
    - `!` (booleans only)
  - numeric `+` (numbers only)
  - `===` (numbers / booleans / `null` / `undefined`; both sides must be the same type)
  - builtin calls (when `CompileOptions { builtins: true, .. }`):
    - `console.log(...)` / `print(...)`
    - `assert(cond, msg?)`
    - `panic(msg?)`
    - `trap()`

### Builtin printing behavior (current)

- `console.log(...)` / `print(...)` accept 0+ arguments (spread args are rejected).
- Arguments are printed space-separated with a trailing newline.
- Printing always flushes stdout (`fflush(NULL)`) after the call to make debugging output visible
  even if the program later traps/aborts.
- Numbers use libc formatting for finite values, but `NaN`/`Infinity`/`-Infinity` are printed in
  a JS-friendly form (instead of libc `nan`/`inf` strings).

Everything else currently fails with a coarse `native_js::codegen::CodegenError`
(`unsupported statement`, `unsupported expression`, `unsupported operator: ...`,
etc).

## Strict TypeScript subset (`native_js::strict`)

`typecheck-ts` implements TypeScript’s semantics (including unsafe escape hatches
like `any`, `eval`, and type assertions). The native pipeline is stricter, so
`native-js` provides an additional validation pass:

```rust
pub fn validate(program: &Program, files: &[FileId]) -> Vec<Diagnostic>
```

This validator is intended to be run on a `typecheck-ts` program and is **not**
currently invoked by the minimal `compile_typescript_to_llvm_ir` emitter (and
therefore not by `native-js-cli` either).

### Rejected constructs (enforced today)

The validator emits hard errors with stable `NJS####` codes:

- `NJS0001`: `any` type (explicit or inferred)
  - also covers exported `any` (e.g. `export function f(): any`)
- `NJS0002`: type assertions (`x as T`, `<T>x`)
- `NJS0003`: non-null assertions (`x!`)
- `NJS0004`: `eval()`
- `NJS0005`: `new Function()`
- `NJS0006`: `with` statements
- `NJS0007`: computed property access with non-literal keys
  - only literal string/number keys are allowed (`obj["x"]`, `arr[0]`)
- `NJS0008`: use of the `arguments` identifier/object

This list is expected to expand over time as the native backend’s supported
subset grows.

Everything else is currently accepted by the strict validator, but note that
`native-js` codegen is still a skeleton and does not yet “support” any
particular feature end-to-end.

### Allowed (design target; codegen is still a skeleton)

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

1. Emit textual IR (`.ll`) from the compiler (or from the current minimal
   emitter).
   - In-process, you can always use `inkwell` (or `native_js::CodeGen`) directly:

     ```rust
     // Given an inkwell::module::Module
     let ir = module.print_to_string().to_string();
     std::fs::write("out.ll", ir)?;
     ```
   - Or use the current CLI to dump IR:

     ```bash
     bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- \
       --emit-llvm out.ll \
       path/to/main.ts
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
