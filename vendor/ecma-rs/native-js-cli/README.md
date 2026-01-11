# native-js-cli

`native-js-cli` is a small, developer-facing CLI package for the `native-js` crate.

This crate currently ships two binaries:

- `native-js-cli` (default): a minimal `parse-js`-driven LLVM IR emitter used for
  builtin smoke tests and IR debugging.
- `native-js`: an experimental **typechecked AOT** pipeline (`typecheck-ts` +
  `native_js::validate::validate_strict_subset` + HIR → LLVM + object emission +
  `clang` link).

## `native-js-cli` (minimal emitter)

The `native-js-cli` binary is intentionally narrow in scope: it compiles a
**single TypeScript file** to textual LLVM IR (via a small `parse-js`-driven IR
emitter in `native-js`), invokes `clang` to produce a temporary executable, and
then runs it.

> Note: this path does **not** run the TypeScript checker; it parses the input
> and lowers a small subset of statements/expressions (and simple user-defined
> functions) directly to LLVM IR.
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

`native-js-cli` shells out to `clang` to compile the generated `.ll` into a
temporary executable. Using `cargo_llvm.sh` is the easiest way to ensure the
LLVM 18 `clang` is on `PATH`.

If you are setting up LLVM locally, see [`native-js/README.md`](../native-js/README.md)
for required packages and the `LLVM_SYS_180_PREFIX` environment variable.

The CLI takes a single positional input file plus flags:

```text
native-js-cli [--no-builtins] [--emit-llvm <PATH>] <INPUT.ts>
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

## Options

### `--emit-llvm <path>`

Write the generated LLVM IR (`.ll`) to a file for debugging:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- \
  --emit-llvm /tmp/out.ll \
  /tmp/main.ts

opt-18 -verify -disable-output /tmp/out.ll
```

This is especially useful when `clang` fails to compile the generated IR (the
CLI normally writes IR to a temporary directory that is deleted on exit).

Note: `native-js-cli` still runs the compiled program after writing the IR. If
you want to stop after compilation, compile the emitted IR yourself:

```bash
clang -x ir /tmp/out.ll -o /tmp/out
```

`native-js-cli` does not currently have a flag to keep the intermediate object
file, but you can produce one from the emitted IR:

```bash
clang -x ir -c /tmp/out.ll -o /tmp/out.o
```

### `--no-builtins`

Disable recognition of small builtin APIs. By default, the IR emitter recognizes
and lowers a handful of builtins:

- `console.log(...)` and `print(...)` (prints values to stdout)
- `assert(cond, msg?)` (aborts on failure, optionally printing `msg`)
- `panic(msg?)` (prints message and aborts)
- `trap()` (emits `llvm.trap`)

Passing `--no-builtins` makes these calls fail with `builtins disabled` so you
can test the non-builtin path.

`console.log` / `print` formatting (current):

- arguments are printed left-to-right, separated by a single space
- a trailing newline is always printed
- numbers are formatted similar to `printf("%.15g")`, but special-cased for
  `NaN` / `Infinity` / `-Infinity`

## Supported language subset (current)

This CLI exercises the **minimal** IR emitter in `native-js` (it is not the
future typechecked/HIR-based backend yet). Supported today:

> Note: this list mirrors the `native-js` documentation for
> `compile_typescript_to_llvm_ir` (see [`native-js/README.md`](../native-js/README.md)).

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
    - builtin calls listed above (unless `--no-builtins`)
    - direct calls to user-defined functions by identifier:
      - exact arity (no varargs)
      - no optional chaining / spread arguments
      - arguments are checked against the callee’s declared parameter types

Type annotations in function declarations (current):

- Supported: `number`, `boolean`, `string`, `void`, `null`, `undefined`
- If omitted, parameters and return types default to `number`
  (this is a convenience for the minimal emitter; it is not TypeScript semantics)

All other statements/expressions/operators currently fail compilation with a
simple error (e.g. `unsupported statement`, `unsupported expression`, or
`unsupported operator: ...`).

## Diagnostics / errors (`native-js-cli`)

The minimal `native-js-cli` binary prints errors to stderr using the `Display`
formatting of `native-js` error types:

- parse errors come from `parse-js` (syntax errors)
- codegen failures come from `native-js::codegen` (`unsupported statement`, etc)

This binary does not currently render source-context diagnostics (file/line
caret spans). For source-level debugging, consider using `parse-js-cli` or
`typecheck-ts-cli`.

Exit codes:

- `0` on success
- non-zero if parsing/codegen fails, if `clang` fails, or if the compiled program
  exits non-zero (the CLI forwards the program’s exit code).

## `tsconfig.json` support

The `native-js-cli` binary currently takes a single input file and does not load
`tsconfig.json` or perform module resolution.

For a typechecked pipeline with module resolution (including `baseUrl`/`paths`),
use the `native-js` binary.

Example:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
  --project path/to/tsconfig.json \
  build path/to/entry.ts -o /tmp/out
```

## `native-js` (typechecked AOT pipeline)

The `native-js` binary is an early proof-of-concept for a typechecked AOT path.
It expects the entry file to export a `main()` function.

### Usage

Preflight-check the entry file (typecheck + legacy strict validation + codegen), without producing
an executable:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
  check path/to/entry.ts
```

> Note: `native-js check` currently runs the legacy `native_js::strict::validate`
> pass, which rejects unsafe escape hatches (including TS-only wrappers like `as`
> and `!`). `native-js build`/`run` use the backend subset validator
> (`native_js::validate::validate_strict_subset`) instead.

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
  --emit llvm-ir --emit-path /tmp/out.ll \
  build path/to/entry.ts -o /tmp/out
```

Also emit the intermediate object file:

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- \
  --emit obj --emit-path /tmp/out.o \
  build path/to/entry.ts -o /tmp/out
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
- `--emit=llvm-ir|bc|obj|asm --emit-path <PATH>`: write an intermediate artifact.
- `--opt=0|1|2|3`: set the LLVM target machine optimization level.
- `--debug`: best-effort debug build (passes `-g` to the system linker).

### Diagnostics

Unlike the minimal `native-js-cli` binary, the typechecked `native-js` pipeline
renders source-context diagnostics (file/line caret spans) from:

- `typecheck-ts` (TypeScript type errors)
- `native-js` validators (`NJS####` codes):
  - strict subset (`native_js::validate::validate_strict_subset`): `NJS0009` / `NJS0010`
  - legacy strict validator (`native_js::strict::validate`): `NJS0001..NJS0008`
- `native-js` HIR-based code generation (when it fails)

Diagnostics are rendered via `diagnostics::render` in a rustc-like format:

```text
error[NJS0009]: property access is not supported by native-js yet
 --> entry.ts:1:43
  |
1 | export function main(): number { return foo.bar; }
  |                                   ^^^^^^^ property access is not supported by native-js yet
```

### Notes

- The HIR-based backend is still extremely small (enough for early smoke tests).
  `native-js-cli` remains the better tool for builtin/lowering debugging.
- `build`/`run` enforce the current native backend subset via
  `native_js::validate::validate_strict_subset` (`NJS0009` / `NJS0010`), which
  currently rejects many common JS/TS constructs (objects/arrays, property
  access, async/await, etc). See [`native-js/README.md`](../native-js/README.md)
  for the current list.

## Tests

`native-js-cli` has a small integration test suite that compiles and runs tiny
programs exercising both the builtin lowering (`native-js-cli`) and the
typechecked AOT pipeline (`native-js`).

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js-cli
```

These tests require a working LLVM 18 toolchain on `PATH` (for `clang`).
