# native-js

`native-js` is the LLVM-backed code generation crate for `ecma-rs` that compiles a **strict subset
of TypeScript** to native code via LLVM.

The crate can emit **LLVM IR** and (on Linux) can produce **object files** / a **native executable**
by shelling out to `clang`/`lld` for linking.

This crate is still early. The high-level AOT entrypoints are not wired up yet
(`native_js::compile` returns `NativeJsError::UnsupportedFeature` and
`Compiler::compile` returns `NativeJsError::Unimplemented`), but the crate
already contains:

- a minimal `parse-js`-driven **textual** LLVM IR emitter (`compile_typescript_to_llvm_ir`)
  used by the `native-js-cli` binary for smoke tests and IR debugging
- an early **HIR-driven** backend used by the typechecked `native-js` binary
  (`native-js-cli --bin native-js`)

This crate is not a general-purpose JavaScript engine and it does not try to
support the full JavaScript/TypeScript language.

## What it does

At a high level, the **planned** native pipeline looks like:

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

The **current** minimal pipeline (used by `native-js-cli`) is:

```
TypeScript source
  → parse-js (parser)
  → native-js `compile_typescript_to_llvm_ir` (minimal parse-js-only emitter)
  → clang (compile IR to native executable)
```

> Note: `compile_typescript_to_llvm_ir` is **parse-js-only** and does not perform
> type checking.

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
the process memory limit and auto-detects LLVM 18. It also forces Rust frame
pointers (`RUSTFLAGS=-C force-frame-pointers=yes`), which is required for the
current stack-walking approach used by the native runtime.

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p native-js
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib
#
# Or, from within vendor/ecma-rs/:
bash scripts/cargo_llvm.sh build -p native-js
bash scripts/cargo_llvm.sh test -p native-js --lib
```

## Quickstart

### Runnable example (in-memory source → native executable)

This crate ships a small runnable example that compiles an in-memory TypeScript snippet to a native
executable (TS → textual LLVM IR → `clang` → native executable), runs it, and prints stdout:

```bash
# From repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js --example compile_and_run

# Or from within vendor/ecma-rs/:
bash scripts/cargo_llvm.sh run -p native-js --example compile_and_run
```

### Library (in-memory source)

If you want to embed the pipeline from Rust code, you can compile a TypeScript string into an
on-disk native executable via `compiler::compile_typescript_to_artifact`:

```rust
use native_js::compiler::compile_typescript_to_artifact;
use native_js::{CompileOptions, EmitKind};
use std::process::Command;

let mut opts = CompileOptions::default();
opts.emit = EmitKind::Executable;

let out = compile_typescript_to_artifact(r#"console.log(1 + 2);"#, opts, None)?;
let output = Command::new(&out.path).output()?;
print!("{}", String::from_utf8_lossy(&output.stdout));
# Ok::<(), Box<dyn std::error::Error>>(())
```

### CLI (file input)

See `native-js-cli` for the CLI front-ends:

```bash
# Minimal parse-js-driven emitter (single file, no typecheck):
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- path/to/file.ts

# Typechecked AOT pipeline (expects entry file to export `main()`):
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- check path/to/entry.ts
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- run path/to/entry.ts
```

## Public API overview (current)

The API is intentionally small and currently consists of:

- `CodeGen`: a minimal façade around `inkwell` that enforces the stack-walking
  invariant (`frame-pointer="all"`, `disable-tail-calls="true"`) and marks
  generated functions with the default GC strategy (`gc "coreclr"`).
- `emit`: helpers for turning an `inkwell::module::Module` into build artifacts:
  - `emit::emit_llvm_ir(&Module) -> String`
  - `emit::emit_bitcode(&Module) -> Vec<u8>`
  - `emit::emit_object(&Module, TargetConfig) -> Vec<u8>`
  - `emit::emit_object_with_statepoints(&Module, TargetConfig) -> Result<Vec<u8>, EmitError>`
  - `emit::emit_asm(&Module, TargetConfig) -> Vec<u8>`
  - `emit::emit_asm_with_statepoints(&Module, TargetConfig) -> Result<Vec<u8>, EmitError>`
  - `emit::EmitError`: error type for the `_with_statepoints` helpers
- `link`: linking helpers for producing executables that preserve LLVM stack maps:
  - `link::link_object_buffers_to_elf_executable(...)`
  - `link::LinkOpts` (defaults to non-PIE on Linux to avoid stackmap relocation issues)
  - exported symbols: `link::FASTR_STACKMAPS_START_SYM` / `link::FASTR_STACKMAPS_END_SYM`
- `validate::validate_strict_subset(...)`: validator for the **strict compilation
  subset** currently supported by the native backend (syntax + type restrictions;
  used by the `native-js` binary in `native-js-cli`).
- `strict::validate(...)`: legacy strict validator that rejects unsafe constructs
  (`any`, `eval`, etc). This is still useful for tests and tooling, but the AOT
  pipeline prefers `validate_strict_subset` (which allows TS-only runtime-inert
  wrappers like `satisfies` / `as` / `!`).
- `strict::entrypoint(...)`: locate the exported `main()` entrypoint in a
  typechecked program (used by the early HIR-based backend).
- `compile_typescript_to_llvm_ir(&str, CompileOptions) -> Result<String, NativeJsError>`:
  compile a single TypeScript module to textual LLVM IR (very small subset; used
  by `native-js-cli`).
- `compile_project_to_llvm_ir(&Program, &dyn Host, FileId, CompileOptions, entry_export)`:
  compile a small multi-file ES module project (subset) to textual LLVM IR
  using `typecheck-ts` for module resolution + export maps (used by
  `native-js-cli`).
- `compile(&Program, &CompilerOptions) -> Result<CompilationOutput, NativeJsError>`:
  AOT compilation entrypoint for a fully typechecked `typecheck-ts` `Program`.
  (Currently returns `NativeJsError::UnsupportedFeature`.)
- `CompilerOptions` / `CompilationOutput`: options/output types for `compile(...)`.
- `Compiler`: entry point (configured with `CompileOptions`)
- `Compiler::compile() -> Result<(), NativeJsError>`: compilation entrypoint
  (currently unimplemented)
- `CompileOptions`: codegen configuration
- `OptLevel`: optimization level (`O0`/`O1`/`O2`/`O3`/`Os`/`Oz`)
- `EmitKind`: artifact kind (`LlvmIr`, `Object`, `Assembly`, `Executable`)
- `NativeJsError`: shared error type for the native pipeline (parse errors,
  codegen errors, typecheck diagnostics, missing toolchain/IO errors, and
  unimplemented/unsupported features)
- `compiler::compile_typescript_to_artifact(...)`: convenience helper that turns the textual LLVM IR
  emitted by `compile_typescript_to_llvm_ir` into an on-disk artifact (including a Linux executable)

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

> Note: the long-term typechecked/HIR backend is still under construction.
> `native_js::codegen` currently contains:
> - the minimal `parse-js`-driven emitter used by `compile_typescript_to_llvm_ir`, and
> - an early HIR-driven backend used by the `native-js` CLI binary.
>
> `native_js::emit` provides the artifact emission helpers used by the HIR-based
> backend and CLI.

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

## LLVM statepoint directive attributes (LLVM 18)

LLVM 18’s `rewrite-statepoints-for-gc` pass assigns a constant statepoint ID by default. `native-js`
supports overriding the emitted `gc.statepoint` ID and patch bytes via callsite string attributes
(`"statepoint-id"`, `"statepoint-num-patch-bytes"`).

See [`../docs/llvm_statepoint_directives.md`](../docs/llvm_statepoint_directives.md).

LLVM's statepoint pipeline requires functions to be annotated with a GC
strategy. `native-js` standardizes on `gc "coreclr"`.

`native_js::CodeGen` applies this automatically, and
`native_js::llvm::gc::set_default_gc_strategy` can be used for other
inkwell-generated functions/modules.

See [`docs/llvm_gc_strategy.md`](./docs/llvm_gc_strategy.md) for the rationale
and how to change it.

> Note: `native_js::CodeGen` marks functions with the LLVM GC strategy attribute
> (`gc "coreclr"`). Other inkwell-based codegen should explicitly call
> `native_js::llvm::gc::set_default_gc_strategy` when statepoint rewriting is
> desired. The minimal string-based emitter (`compile_typescript_to_llvm_ir`)
> does **not** set a GC strategy, but it does emit the stack-walking-related
> attributes (`frame-pointer="all"`, `disable-tail-calls="true"`).

### LLVM GC statepoints (LLVM 18)

The GC integration strategy is based on LLVM **statepoints** and **stack maps**.
In LLVM 18, manually constructing `llvm.experimental.gc.statepoint.*` is
error-prone due to intrinsic signature constraints (e.g. `immarg` parameters,
`elementtype(<fn-ty>)` requirements, and extra trailing `i32` fields).

`native-js` therefore prefers emitting ordinary `call`s in IR and then relying
on LLVM's `rewrite-statepoints-for-gc` pass to rewrite them into correct
statepoints, add the `"gc-live"` operand bundle, and insert `gc.relocate` calls.

The helper surface lives under `native_js::llvm`:

- `native_js::llvm::gc`
  - `gc_ptr_type(&Context) -> ptr addrspace(1)` for GC references
  - `set_default_gc_strategy(&FunctionValue)` to mark a function `gc "coreclr"`
- `native_js::llvm::passes`
  - `rewrite_statepoints_for_gc(&Module, &TargetMachine)` (runs via `llvm-sys`
    `LLVMRunPasses`, plus `verify<safepoint-ir>` in debug builds)

`rewrite-statepoints-for-gc` only rewrites call sites that occur inside
**GC-managed functions** (i.e. functions annotated with `gc "<strategy>"`), so
make sure to apply `gc "coreclr"` (via `CodeGen` or `set_default_gc_strategy`)
on any function that should participate in statepoint lowering.

See `native-js/tests/statepoint_stackmap.rs` for a minimal end-to-end example
that asserts statepoint/relocate rewriting and that the emitted object contains a
`.llvm_stackmaps` section.

The `.llvm_stackmaps` section can be composed differently depending on link mode
(object-file concatenation vs LTO merging). See [`docs/stackmaps.md`](./docs/stackmaps.md)
for the runtime parsing requirements.

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
and is the backend used by `native-js-cli` (see
[`native-js-cli/README.md`](../native-js-cli/README.md)).

The input is always parsed as a **TypeScript module**:

- `parse-js` `Dialect::Ts`
- `parse-js` `SourceType::Module`

Only `CompileOptions::builtins` is currently honored by this path; the remaining
fields are reserved for the eventual LLVM-backed backend.

### Supported subset (current)

- Top-level statements:
  - empty statements (`;`)
  - block statements (`{ ... }`)
  - expression statements (`expr;`)
  - variable declarations (`const`/`let`/`var`) with simple identifier bindings
    - initializer is optional; missing initializers default to `undefined`
  - `if (cond) { ... } else { ... }` (boolean conditions only)
  - `while (cond) { ... }` (boolean conditions only)
  - function declarations (top-level only; no nesting):
    - cannot be named `main` (reserved for the native entrypoint)
    - no `async` / generators
    - no optional/rest parameters
    - parameter patterns must be identifiers
    - `return` statements are supported inside function bodies
- Expressions:
  - number / boolean / string / null literals
  - identifiers:
    - local bindings introduced by `const`/`let`/`var`
    - globals: `undefined`, `NaN`, `Infinity`
  - unary operators:
    - `-` / `+` (numbers only)
    - `!` (booleans only)
  - numeric `+` / `-` / `*` / `/` (numbers only)
  - numeric comparisons: `<` / `<=` / `>` / `>=` (numbers only)
  - logical `&&` / `||` (booleans only; currently eager evaluation, not short-circuit)
  - assignment:
    - `x = expr` (identifier targets only; allows changing the binding type in the minimal emitter)
    - `x += expr` (number variables only)
  - `===` (numbers / booleans / strings / `null` / `undefined`; both sides must be the same type)
  - `!==` (same types as `===`; additionally, different types return `true` like JS)
  - calls:
    - builtin calls (when `CompileOptions { builtins: true, .. }`):
      - `console.log(...)` / `print(...)`
      - `assert(cond, msg?)`
      - `panic(msg?)`
      - `trap()`
    - direct calls to user-defined functions by identifier:
      - exact arity (no varargs)
      - no optional chaining / spread arguments
      - arguments are checked against the callee’s declared parameter types

Type annotations in function declarations (current):

- Supported: `number`, `boolean`, `string`, `void`, `null`, `undefined`
- If omitted, parameters and return types default to `number`
  (this is a convenience for the minimal emitter; it is not TypeScript semantics)

### Builtin printing behavior (current)

- `console.log(...)` / `print(...)` accept 0+ arguments (spread args are rejected).
- Arguments are printed space-separated with a trailing newline.
- Printing always flushes stdout (`fflush(NULL)`) after the call to make debugging output visible
  even if the program later traps/aborts.
- Numbers use libc formatting for finite values (currently `printf("%.15g")`), but
  `NaN`/`Infinity`/`-Infinity` are printed in a JS-friendly form (instead of libc `nan`/`inf`
  strings).
- `assert(cond, msg?)` aborts when `cond` is false:
  - prints `msg` if provided (any printable value)
  - otherwise prints a default `assertion failed` message

Everything else currently fails with a coarse `native_js::codegen::CodegenError`
(`unsupported statement`, `unsupported expression`, `unsupported operator: ...`,
etc).

## Strict compilation subset (`native_js::validate`)

The typechecked AOT pipeline (`native-js` binary) runs an additional validation
pass after successful type checking and before LLVM IR generation:

```rust
pub fn validate_strict_subset(program: &Program) -> Result<(), Vec<Diagnostic>>
```

It emits stable `NJS####` codes:

- `NJS0009`: unsupported syntax in the native-js strict subset
- `NJS0010`: unsupported type in the native-js strict subset

This validator is intentionally conservative and is expected to be relaxed
incrementally as more language features are lowered safely.

### Rejected constructs (enforced today)

The strict subset validator currently rejects (non-exhaustive, but directly
matching the validator’s checks):

- Unsupported syntax (`NJS0009`), including:
  - classes / class expressions
  - `async` / generator functions, `await`, `yield`
  - object literals, array literals, and destructuring patterns
  - property access (`obj.prop`, `obj["prop"]`)
  - template literals / tagged templates
  - `import()` expressions, `import.meta`
  - `super`, `new.target`
  - JSX
  - `with`, `try`, `throw`
  - `eval()` and `Function()` / `new Function()`
  - use of the `arguments` identifier/object
- Unsupported types (`NJS0010`):
  - anything other than the primitive types `number`/`boolean`/`string` plus
    `null`/`undefined`/`void`/`never` and their literal types
  - e.g. unions/intersections, object types, function types, nominal/reference
    types, `bigint`, `symbol`, template-literal types, etc.

> Note: TypeScript-only, runtime-inert expression wrappers such as `satisfies`,
> type assertions (`as`), and non-null assertions (`!`) are allowed by this
> validator, but the wrapped runtime expressions are still validated.

Even if the strict subset validator passes, note that the current HIR-based
backend is still minimal; some programs may still fail later during codegen.

## Legacy strict validator (`native_js::strict`)

`typecheck-ts` implements TypeScript’s semantics (including unsafe escape hatches
like `any`, `eval`, and type assertions). The native pipeline is stricter, so
`native-js` provides an additional validation pass:

```rust
pub fn validate(program: &Program, files: &[FileId]) -> Vec<Diagnostic>
```

This validator is intended to be run on a `typecheck-ts` program, but it is
**not** invoked by the minimal `compile_typescript_to_llvm_ir` emitter (and
therefore not by `native-js-cli`).

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
the typechecked/HIR-based backend is still extremely small and does not yet
support most features end-to-end.

### Allowed (design target; HIR codegen is still minimal)

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

### Run the native pipeline smoke tests

The `native-js` workspace also contains `native-js-cli`, which exercises the
current minimal IR emitter end-to-end (TS → LLVM IR → `clang` → run):

```bash
# From repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js-cli
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
