# native-js-cli

`native-js-cli` is a small, developer-facing CLI package for the `native-js` crate.

This crate currently ships two binaries:

- `native-js-cli` (default): a minimal `parse-js`-driven LLVM IR emitter used for
  builtin smoke tests and IR debugging.
- `native-js`: an experimental **typechecked AOT** pipeline (`typecheck-ts` +
  `native_js::validate::validate_strict_subset` + HIR → LLVM + object emission +
  `clang` link).

If you have the `native-js` binary installed on your `PATH`, common commands are:

```bash
native-js check input.ts
native-js run input.ts
```

## `native-js-cli` (minimal emitter)

The `native-js-cli` binary is intentionally narrow in scope: it compiles a
**TypeScript module entry file** (plus a small subset of ES module imports) to
textual LLVM IR (via a small `parse-js`-driven IR emitter in `native-js`),
then uses `native-js`’s artifact pipeline to emit an object file via LLVM and
link a temporary executable (via `clang`/`lld`), and then runs it.

> Note: this describes the default `--pipeline project` mode, which is still
> **parse-js-driven** and does not use TypeScript's type system for code
> generation. However, it *does* invoke `typecheck-ts` to discover the module
> graph and export maps (so it can compile multi-file projects with
> `import { ... } from "./mod"`).
>
> In `--pipeline project` mode, the CLI does not treat TypeScript type errors as
> fatal for code generation; `typecheck-ts` is only used for
> reachability/module metadata.
>
> For a typechecked pipeline, use `--pipeline checked` (or the `native-js`
> binary).
>
> The input is parsed as a TypeScript **module** (`Dialect::Ts` +
> `SourceType::Module`).

This binary is primarily useful for:

- smoke-testing the native pipeline end-to-end (TS → LLVM IR → native executable)
- iterating on the IR emitter and builtin lowering
- dumping IR for debugging (`--emit-llvm`)

## Usage

> LLVM builds are memory-hungry. In this repo, prefer the LLVM wrapper:
>
> ```bash
> # From the repo root (recommended):
> bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- <args...>
>
> # Or, from within vendor/ecma-rs/:
> bash scripts/cargo_llvm.sh run -p native-js-cli -- <args...>
> ```

`native-js-cli` uses `native-js` to compile the generated `.ll` into an object
file in-process via LLVM, and then shells out to `clang`/`lld` for linking.
Using `cargo_llvm.sh` is the easiest way to ensure the LLVM 18 toolchain is on
`PATH`.

If you are setting up LLVM locally, see [`native-js/README.md`](../native-js/README.md)
for required packages and the `LLVM_SYS_180_PREFIX` environment variable.

The CLI supports a small set of subcommands (`check`/`build`/`run`/`emit-ir`) and
also a default mode (no subcommand) that compiles + runs a project.

In all modes, the entry file is a TypeScript module path:

```text
native-js-cli [--pipeline <project|checked>] [--entry-fn <NAME>] [--no-builtins] [--emit-llvm <PATH>] [<ENTRY.ts>]
```

The input file is read as UTF-8 text; invalid UTF-8 will cause the CLI to exit
with an error before parsing.

### Run a TypeScript file

Create a small program:

```bash
cat > /tmp/main.ts <<'TS'
console.log(1 + 2);
TS
```

Compile + run it:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- /tmp/main.ts
```

Expected output:

```text
3
```

### Run a small multi-file ES module project

```bash
cat > /tmp/math.ts <<'TS'
export function add(a: number, b: number) { return a + b }
TS

cat > /tmp/main.ts <<'TS'
import { add } from './math';
export function main() { console.log(add(1, 2)); }
TS

bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- \
  /tmp/main.ts
```

By default, if the entry module exports `main()`, it will be invoked after all module initializers
run. Pass `--entry-fn <NAME>` to call a different exported function (or to call a function when the
export name isn’t `main`).

## Options

### `--pipeline <project|checked>`

Select which compilation pipeline to use:

- `project` (default): the legacy `parse-js`-driven textual LLVM IR emitter.
  - keeps compiling even when `typecheck-ts` reports type errors
  - supports `--entry-fn` (and auto-calls an exported `main()` when present)
- `checked`: typechecked `native_js::compile_program` pipeline.
  - fails on TypeScript type errors
  - enforces the `native_js::validate::validate_strict_subset` checks (`NJS0009` / `NJS0010`)
  - expects the entry module to export `main()` (like the `native-js` binary)
  - does **not** support `--entry-fn`
  - rejects cyclic module dependencies (`NJS0146`)
  - see below (`native-js` section) for the current checked/HIR backend subset
 
### `--entry-fn <name>`

After initializing the module graph (executing top-level statements in dependency
order), call an **exported** function from the entry module.

By default, if the entry module exports `main()` (with zero parameters), it is
invoked automatically after module initialization. Use `--entry-fn` to call a
different exported function.

The entry function must take **zero** parameters; its return value is ignored.

This flag is only supported with `--pipeline project`.

### `--emit-llvm <path>`

Write the generated LLVM IR (`.ll`) to a file for debugging:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- \
  --emit-llvm /tmp/out.ll \
  /tmp/main.ts

opt-18 -verify -disable-output /tmp/out.ll
```

This is especially useful when LLVM rejects the generated IR (verification /
codegen) or when linking fails (the CLI normally writes IR to a temporary
directory that is deleted on exit).

Notes:

- `--emit-llvm` does not change execution: in the default mode (no subcommand)
  and `run`, the CLI still builds and runs the program after writing the IR.
- If you only want the `.ll` (no build/run), use the `emit-ir` subcommand:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- \
  emit-ir /tmp/main.ts -o /tmp/out.ll
```

If you want to build an executable without running it, use `build` (you can
combine it with `--emit-llvm`):

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- \
  --emit-llvm /tmp/out.ll \
  build /tmp/main.ts -o /tmp/out
```

### `--no-builtins`

Disable recognition of small builtin APIs. By default, the IR emitter recognizes
and lowers a handful of builtins:

- `console.log(...)` and `print(...)` (prints values to stdout)
- `assert(cond, msg?)` (aborts on failure, optionally printing `msg`; uses JS truthiness for supported types)
- `panic(msg?)` (prints message and aborts)
- `trap()` (emits `llvm.trap`)

Passing `--no-builtins` makes these calls fail with `builtins disabled` so you
can test the non-builtin path.

For `--pipeline checked`, this flag disables the `print(...)` intrinsic (it is
rejected with `NJS0012` when builtin intrinsics are disabled). Other builtins
(`assert` / `panic` / `trap`) are currently only supported by `--pipeline project`.

`console.log` / `print` formatting (current):

- arguments are printed left-to-right, separated by a single space
- a trailing newline is always printed
- numbers are formatted similar to `printf("%.15g")`, but special-cased for
  `NaN` / `Infinity` / `-Infinity`

## Supported language subset (`--pipeline project`, current)

This CLI exercises the **minimal** IR emitter in `native-js` (it is not the
future typechecked/HIR-based backend yet). Supported today:

> Note: this list mirrors the `native-js` documentation for
> the minimal `parse-js` emitters (`compile_typescript_to_llvm_ir` /
> `compile_project_to_llvm_ir`; see [`native-js/README.md`](../native-js/README.md)).

- Top-level statements:
  - empty statements (`;`)
  - block statements (`{ ... }`)
  - expression statements (`expr;`)
  - variable declarations (`const`/`let`/`var`) with simple identifier bindings
    - initializer is optional; missing initializers default to `undefined`
  - `if (cond) { ... } else { ... }` (uses JS truthiness for supported primitive types)
  - `while (cond) { ... }` (uses JS truthiness for supported primitive types)
  - function declarations (top-level only; no nesting):
    - can be named `main` (the minimal multi-file path namespaces user fns in LLVM)
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
    - `!` (uses JS truthiness for supported primitive types)
  - numeric `+` / `-` / `*` / `/` (numbers only)
  - numeric comparisons: `<` / `<=` / `>` / `>=` (numbers only)
  - logical `&&` / `||` (booleans only; short-circuit evaluation)
  - assignment:
    - `x = expr` (identifier targets only; allows changing the binding type in the minimal emitter)
    - `x += expr` (number variables only)
  - `===` (numbers / booleans / strings / `null` / `undefined`; different types return `false` like JS)
  - `!==` (same types as `===`; additionally, different types return `true` like JS)
  - calls:
    - builtin calls listed above (unless `--no-builtins`)
    - direct calls to user-defined functions by identifier:
      - exact arity (no varargs)
      - no optional chaining / spread arguments
      - arguments are checked against the callee’s declared parameter types

- ES module subset (multi-file projects):
  - `export function foo(...) { ... }`
  - `import { foo } from "./mod"` and `import { foo as bar } from "./mod"`
  - side-effect imports (`import "./mod"`) for module initialization ordering
  - module initializers run in dependency order (matching source import/re-export order for siblings)
  - type-only imports/re-exports do not trigger module evaluation
  - re-exports (`export { foo } from "./mod"`, `export * from "./mod"`) for module initialization ordering

Limitations:

- Default exports and namespace imports are not supported.
- `tsconfig.json` is not loaded (so `baseUrl`/`paths` are ignored); module resolution
  uses a Node-style resolver for the supported import forms.

Type annotations in function declarations (current):

- Supported: `number`, `boolean`, `string`, `void`, `null`, `undefined`
- If omitted, parameters and return types default to `number`
  (this is a convenience for the minimal emitter; it is not TypeScript semantics)

All other statements/expressions/operators currently fail compilation with a
simple error (e.g. `unsupported statement`, `unsupported expression`, or
`unsupported operator: ...`).

## Diagnostics / errors (`native-js-cli`)

- In `--pipeline project` mode (default), errors are printed to stderr using the
  `Display` formatting of `native-js` error types:
  - parse errors come from `parse-js` (syntax errors)
  - codegen failures come from `native-js::codegen` (`unsupported statement`, etc)
- In `--pipeline checked` mode, failures from `native_js::compile_program` are
  rendered as source-context diagnostics (file/line caret spans), similar to the
  `native-js` binary.

Exit codes:

- `0` on success
- non-zero if parsing/codegen fails, if linking fails, or if the compiled program
  exits non-zero (the CLI forwards the program’s exit code).

## `tsconfig.json` support

The `native-js-cli` binary takes a single entry file and (when using the
multi-file ES module subset) performs Node-style module resolution to discover
reachable imports.

However, it does **not** load `tsconfig.json`, so project settings like
`baseUrl`/`paths` are not applied.

This is true for both `--pipeline project` and `--pipeline checked`: the
`native-js-cli` binary always uses a small filesystem host with default compiler
options (it does not read project settings from disk).

For a typechecked pipeline that loads `tsconfig.json` (including
`baseUrl`/`paths`),
use the `native-js` binary.

Example:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
  --project path/to/tsconfig.json \
  build path/to/entry.ts -o /tmp/out
```

## `native-js` (typechecked AOT pipeline)

The `native-js` binary is an early proof-of-concept for a typechecked AOT path.
It expects the entry module to export `main()`.

### Usage

Preflight-check the entry file (typecheck + strict subset validation + entrypoint checks), without
producing an executable:

 ```bash
 # From the repo root:
 bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
   check path/to/entry.ts
 ```

> Note: `native-js check` performs the same typechecking + subset validation as
> `native-js build`/`run`, but stops before producing/linking an executable.

Pass `--extra-strict` to also run the legacy `native_js::strict::validate` checks (this rejects
TypeScript "escape hatches" like type assertions (`as`) and non-null assertions (`!`), in addition
to other unsafe constructs like `any` and `eval()`).

If you have the binary installed on your `PATH`, the equivalent invocation is:

```bash
native-js check input.ts
native-js run input.ts

native-js check path/to/entry.ts
native-js run path/to/entry.ts
```

Build a TypeScript file into a native executable:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
  build path/to/entry.ts -o /tmp/out
```

Also emit LLVM IR (for debugging):

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
  build path/to/entry.ts -o /tmp/out --emit-ir /tmp/out.ll
```

Run immediately:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
  run path/to/entry.ts
```

Pass arguments to the generated executable by putting them after `--`:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
  run path/to/entry.ts -- arg1 arg2
```

### Options (selected)

- `--project/-p <tsconfig.json>`: load a TypeScript project and apply `baseUrl`/`paths`
  for module resolution.
- `--json`: emit versioned JSON diagnostics to stdout (`schema_version = 1`).
  - not supported with `run` (it would mix with program stdout)
- `build --emit-ir <PATH.ll>`: also write the emitted LLVM IR (for debugging).
- `emit-ir -o <PATH.ll>`: write LLVM IR without producing an executable.
- `--opt=0|1|2|3`: set the LLVM target machine optimization level.
- `--debug`: best-effort debug build (passes `-g` to the system linker).
- `--extra-strict`: also run the legacy strict validator (`native_js::strict::validate`).

### Diagnostics

Unlike the minimal `native-js-cli` binary, the typechecked `native-js` pipeline
renders source-context diagnostics (file/line caret spans) from:

- `typecheck-ts` (TypeScript type errors)
- `native-js` validators (`NJS####` codes):
  - backend subset (`native_js::validate::validate_strict_subset`): `NJS0009` / `NJS0010`
  - entrypoint checks (`native_js::strict::entrypoint`): `NJS0108..NJS0111`
  - legacy strict validator (`native_js::strict::validate`, only with `--extra-strict`): `NJS0001..NJS0008`
- `native-js` HIR-based code generation (when it fails)

Diagnostics are rendered via `diagnostics::render` in a rustc-like format:

```text
error[NJS0009]: property access is not supported by native-js yet
 --> entry.ts:1:41
  |
1 | export function main(): number { return "hi".length; }
  |                                         ^^^^^^^^^^^ property access is not supported by native-js yet
```

### Notes

- The HIR-based backend is still extremely small (enough for early smoke tests).
  `native-js-cli` remains the better tool for builtin/lowering debugging.
- `check`/`build`/`run` enforce the current native backend subset via
  `native_js::validate::validate_strict_subset` (`NJS0009` / `NJS0010`), which
  currently rejects many common JS/TS constructs (objects/arrays, property
  access, async/await, etc). See [`native-js/README.md`](../native-js/README.md)
  for the current list.
- Even after `validate_strict_subset` passes, the current HIR→LLVM lowering is
  still minimal and may fail later during codegen with `NJS0011` / `NJS01xx` diagnostics
  (see [`native-js/src/codegen/mod.rs`](../native-js/src/codegen/mod.rs) for the
  current list).

#### HIR codegen subset (current)

The current HIR-based code generator (used by `native-js`) is still a small
smoke-test subset intended for early end-to-end testing. It is currently an
`i32`-only backend (booleans are lowered to `0`/`1`).

It emits a single LLVM module for the entry file and all transitively imported
**runtime** ES modules (type-only imports are ignored). In addition to `import`
statements, **runtime** re-exports (e.g. `export { x } from "./dep"`, `export {}
from "./dep"`, `export * from "./dep"`) are treated as module dependencies:
re-export-only modules participate in module initialization ordering, and
imports can resolve through them.

Type-only re-exports (e.g. `export { type T } from "./dep"`) are ignored for
runtime (they are erased from JS output), so they do **not** execute the
dependency module (see the `type_only_reexport_does_not_execute_module`
integration test).

Module initializers run in dependency order (matching source request order for
sibling imports/re-exports) before calling the entry file’s exported `main()`.
Cyclic runtime module dependencies are not supported (they are rejected with
`NJS0146`).

- The entry file must export `main()`:
  - may be defined in the entry file or re-exported (e.g. `export { main } from "./impl"`)
  - no parameters
  - not `async` / not a generator
  - signature must be compatible with the current native ABI/codegen (`NJS0011`):
    - parameters: `number`/`boolean` (but `main` must have none)
    - return: `number`/`boolean`/`void`/`undefined`
- Numeric literals must be **32-bit signed integers** (decimal/hex/binary/octal;
  `_` separators allowed). Floats/`1e3`-style literals are rejected.
- Supported statements include:
  - blocks (`{ ... }`)
  - `if` / `else`
  - `while`, `do { ... } while`, `for`
  - `break` / `continue` (labeled and unlabeled; only loops can be labeled)
  - variable declarations (`const`/`let`/`var`) with identifier binding **and an initializer**
  - `return <expr>` (and `return;` when `main` returns `void`/`undefined`)
  - `print(<number>);` (intrinsic statement; prints decimal + `\n` to stdout)
- Supported expressions include:
  - boolean literals (`true`/`false`)
  - unary: `+x`, `-x`, `!x`, `~x`
  - binary arithmetic/bitwise: `+`, `-`, `*`, `/`, `%`, `&`, `|`, `^`, `<<`, `>>`, `>>>`
  - short-circuit logical: `&&`, `||`
  - comma operator: `(a, b)`
  - comparisons/equality: `<`, `<=`, `>`, `>=`, `==`, `!=`, `===`, `!==`
  - assignment to identifiers (`=`, `+=`, `-=`, `*=`, `/=`, `%=`)
  - updates (`++x`, `x++`, `--x`, `x--`)
  - direct calls to global functions by identifier (`foo(a, b)`), with limitations:
    - no optional calls / `new` / spreads
    - callee signatures must fit the current native ABI (params: `number|boolean`, ret: `number|boolean|void|undefined`)
- For `main` returning `number`/`boolean`, the returned `i32` value becomes the executable’s exit code (like C `main`).
- If `main` returns `void`/`undefined`, `return;` and falling off the end of the function are allowed and
  the process exit code is always `0` (return values are ignored).

## Tests

`native-js-cli` has a small integration test suite that compiles and runs tiny
programs exercising both the builtin lowering (`native-js-cli`) and the
typechecked AOT pipeline (`native-js`).

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js-cli
```

These tests require a working LLVM 18 toolchain on `PATH` (for `clang`).
