# native-js-cli

`native-js-cli` is a small command-line frontend for the native TypeScript→LLVM
pipeline (`native-js`).

> Status: this crate/binary is not implemented yet in this repository. This
> README documents the intended CLI surface so it can be kept stable as the
> implementation lands.

It is intended as a developer tool:

- compile a TypeScript entrypoint to LLVM IR (`.ll`) or an object file (`.o`)
- load real-world projects via `tsconfig.json`
- render typechecking / compilation diagnostics in a human-friendly format

## Usage

> LLVM builds are memory-hungry. In this repo, prefer the LLVM wrapper:
>
> ```bash
> # From the repo root:
> bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- <args...>
>
> # Or, from within vendor/ecma-rs/:
> bash scripts/cargo_llvm.sh run -p native-js-cli -- <args...>
> ```
>
> The plain `cargo run -p native-js-cli -- ...` examples below assume your
> current working directory is `vendor/ecma-rs/` (the ecma-rs workspace root).

### Build

Once implemented, compile an entry file:

```bash
# Emit textual LLVM IR
cargo run -p native-js-cli -- build path/to/main.ts --emit=llvm-ir -o out.ll

# Emit an object file
cargo run -p native-js-cli -- build path/to/main.ts --emit=obj -o out.o
```

### Run

Once implemented, `run` compiles the program (typically via an object file) and executes it.
Arguments after `--` are forwarded to the compiled program:

```bash
cargo run -p native-js-cli -- run path/to/main.ts -- --help
```

## `--emit`

`--emit` controls what the compiler writes to disk during `build`:

- `--emit=llvm-ir`: write textual LLVM IR (`.ll`)
- `--emit=obj`: write a target object file (`.o`)

If `--emit` is omitted, the CLI defaults to a format appropriate for the chosen
subcommand (e.g. `obj` for `run`).

## Loading a project with `tsconfig.json`

Use `--project` / `-p` to point at a `tsconfig.json` and enable project-style
file discovery and module resolution (similar to `tsc -p`):

```bash
# Compile the project entrypoint using tsconfig settings
cargo run -p native-js-cli -- build --project ./tsconfig.json src/main.ts --emit=obj -o out.o
```

## Diagnostics

Diagnostics are printed to stderr with file/line context using the shared
`diagnostics` renderer (same style as `typecheck-ts-cli`):

```text
error[NJS0001]: `any` is not allowed in native-js strict mode
 --> main.ts:1:1
  |
1 | function f(): any { return 1; }
  |               ^^^
  = note: add a precise type annotation or refactor to avoid `any`
```

On compilation errors:

- the CLI exits with a non-zero exit code
- emitted outputs (IR/object) are not written (or are removed if partially written)

Diagnostics include:

- parse errors (`parse-js`)
- type errors (`typecheck-ts`)
- native codegen errors (`native-js`)
