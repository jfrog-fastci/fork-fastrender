# ecma-rs

`ecma-rs` is a Rust workspace for a **deterministic** JavaScript/TypeScript toolchain.

Today the repo contains:

- **Parser**: `parse-js` (TS/TSX/JS/JSX → `Node<TopLevel>` + `Loc`)
- **HIR**: `hir-js` (lowered representation with stable-ish IDs for defs/bodies/exprs)
- **Semantics**: `semantic-js` (scopes/symbols/exports; JS mode used by minifier/optimizer)
- **TypeScript checker**: `typecheck-ts` (binder + checker + public `Program` API)
- **Native codegen (in progress)**: `native-js` (strict TS subset → LLVM IR/object code)
- **Native runtime (in progress)**: `runtime-native` (GC + async runtime ABI for `native-js`)
- **Emitter/printer**: `emit-js` (AST → text, deterministic formatting)
- **TypeScript erasure/lowering**: `ts-erase` (shared TS→JS erasure for tooling)
- **Minifier**: `minify-js` (+ `minify-js-cli`, + `minify-js-nodejs` package)
- **Optimizer**: `optimize-js` (SSA-based optimizer and decompiler)
- **VM oracle harness**: `native-oracle-harness` (erase TS fixtures → JS and execute under `vm-js`)

## Architecture docs (start here)

- **North-star design**: [`AGENTS.md`](./AGENTS.md) — authoritative architecture + implementation playbook.
- **Current crate boundaries**: [`docs/architecture.md`](./docs/architecture.md) — what exists today and how the crates connect.
- **Local setup for conformance + difftsc**: [`docs/quickstart.md`](./docs/quickstart.md) (also links into [`typecheck-ts-harness/README.md`](./typecheck-ts-harness/README.md)).
- **Runnable examples**: [`docs/examples.md`](./docs/examples.md) — copy/paste `bash scripts/cargo_agent.sh run` examples for the core crate APIs.
- **Native TS→LLVM docs**: [`native-js/README.md`](./native-js/README.md) (crate API + LLVM setup) and [`native-js-cli/README.md`](./native-js-cli/README.md) (CLI usage + current supported subset).
- **Native runtime ABI**: [`docs/runtime-native.md`](./docs/runtime-native.md) and [`runtime-native/README.md`](./runtime-native/README.md).
- **Native codegen notes**: [`docs/native_stackmaps.md`](./docs/native_stackmaps.md) — preserving LLVM GC stackmaps in release binaries.

The workspace dependency graph in [`docs/deps.md`](./docs/deps.md) is generated; run `just docs` to refresh it.

## Native compiler (EXEC.plan)

This repository also includes a detailed execution plan for a future **TypeScript → native** compiler (and a strict TS dialect intended to make AOT compilation feasible).

- [`docs/native_compiler_quickstart.md`](./docs/native_compiler_quickstart.md) — strict-native rules, system checks, strict-native typecheck CLI usage, and the `vm-js` oracle harness flow.

The source of truth for requirements and scope is [`EXEC.plan.md`](./EXEC.plan.md).

## Quick start

If you're setting up a checkout for TypeScript conformance / differential testing and you have [`just`](https://github.com/casey/just) + Node.js installed:

```bash
just setup
```

This bootstraps submodules + `typecheck-ts-harness` npm deps, generates `Cargo.lock`, and runs a small sanity check. See [`docs/quickstart.md`](./docs/quickstart.md) for details.

This workspace intentionally does **not** commit `Cargo.lock` (it is gitignored). To run commands that match CI’s `--locked` behaviour, generate it first:

```bash
bash scripts/cargo_agent.sh generate-lockfile
```

If you have [`just`](https://github.com/casey/just) installed, the root `justfile` mirrors CI’s main checks:

```bash
just ci
```

Note: CI runs the same commands with `--locked` after generating `Cargo.lock`; the `just` recipes omit `--locked` for convenience.

### Nested-workspace builds (when this repo is vendored)

In this mono-repo, `ecma-rs` lives under `vendor/ecma-rs/` and is **not** part of the top-level
Cargo workspace. Use the nested wrapper to ensure Cargo uses `vendor/ecma-rs/Cargo.toml`:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p effect-js --lib
```

Or, if you're already inside `vendor/ecma-rs/`:

```bash
bash scripts/cargo_agent.sh test -p effect-js --lib
```

`just ci` runs:

- `bash scripts/cargo_agent.sh fmt --all --check`
- `bash scripts/check_utf8_apis.sh`
- `bash scripts/check_no_raw_cargo.sh` (guard against raw `cargo` usage in tooling; use the wrappers)
- `bash scripts/check_diagnostic_codes.sh`
- `bash scripts/cargo_agent.sh clippy …` (sharded; see `clippy-*` recipes in `justfile`)
- `bash scripts/cargo_agent.sh check …` (sharded; see `check-*` recipes in `justfile`)
- `bash scripts/cargo_agent.sh test …` (sharded; see `test-*` recipes in `justfile`)
- `bash scripts/gen_deps_graph.sh` (then verifies `docs/deps.md` is unchanged)

### Run the in-repo examples

The repository includes compiled examples demonstrating the public APIs of the
core crates (especially `typecheck-ts`):

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts --example memory_host_basic
bash scripts/cargo_agent.sh run -p typecheck-ts --example json_snapshot
```

See [`docs/examples.md`](./docs/examples.md) for the full list.

### Run the CLIs

All tools treat input source as **UTF-8 text** (see [UTF-8 policy](#utf-8--source-text-policy)).

#### Parser CLI (`parse-js-cli`)

Reads from stdin and prints a JSON AST to stdout:

```bash
echo 'let x = 1 + 2' | bash scripts/cargo_agent.sh run -p parse-js-cli --locked -- --timeout-secs 2 > ast.json
```

#### Minifier CLI (`minify-js-cli`)

Minifies a single file (or stdin) and writes to stdout:

```bash
echo 'function add(a, b) { return a + b; }' | bash scripts/cargo_agent.sh run -p minify-js-cli --locked -- --mode global
```

#### Typechecker CLI (`typecheck-ts-cli`)

Type-check a file:

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts-cli --locked -- typecheck fixtures/basic.ts

# Enforce additional strict-native checks (repo-specific; see EXEC.plan):
bash scripts/cargo_agent.sh run -p typecheck-ts-cli --locked -- typecheck --native-strict fixtures/basic.ts
```

Query types/symbols by **byte offset** (UTF-8):

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts-cli --locked -- typecheck fixtures/basic.ts --type-at fixtures/basic.ts:0
```

#### Native LLVM CLIs (`native-js-cli` / `native-js`)

The `native-js-cli` package currently builds **two experimental** tools:

- `native-js-cli`: compiles a TypeScript **entry module** (plus a small subset of ES module imports)
  to textual LLVM IR and runs it (TS → LLVM IR → `clang` → native executable).
  The default `--pipeline project` mode uses a small `parse-js`-driven IR emitter; it does not use
  TypeScript’s type system for code generation (it only uses `typecheck-ts` for module graph discovery).
  Use `--pipeline checked` (or the `native-js` binary) for the typechecked backend.
- `native-js`: proof-of-concept **typechecked AOT** pipeline (very small subset today):
  `typecheck-ts` + `native-js` strict validation + HIR → LLVM + object emission + `clang` link.

```bash
cat > /tmp/native_js_cli_demo.ts <<'TS'
console.log(1 + 2);
TS

# Prefer the LLVM wrapper (sets a higher memory limit and ensures LLVM tools are on PATH).
bash scripts/cargo_llvm.sh run -p native-js-cli --locked -- /tmp/native_js_cli_demo.ts

# Dump the generated IR for debugging.
bash scripts/cargo_llvm.sh run -p native-js-cli --locked -- \
  --emit-llvm /tmp/out.ll \
  /tmp/native_js_cli_demo.ts
```

Typechecked AOT demo (`native-js` expects an exported `main()`):

```bash
cat > /tmp/native_js_aot_demo.ts <<'TS'
export function main(): number { return 42; }
TS

# Compiles + runs the output. (The program exits with code 42.)
bash scripts/cargo_llvm.sh run -p native-js-cli --locked --bin native-js -- \
  run /tmp/native_js_aot_demo.ts
```

See [`native-js-cli/README.md`](./native-js-cli/README.md) for details on both
binaries, supported subsets, and flags.

#### Harness (`typecheck-ts-harness`)

Run the small “conformance-mini” suite used by CI (no submodule checkout required):

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts-harness --release --locked -- \
  conformance \
  --root typecheck-ts-harness/fixtures/conformance-mini \
  --compare snapshot \
  --manifest typecheck-ts-harness/fixtures/conformance-mini/manifest.toml \
  --json > typecheck-conformance-mini.json
```

Run `difftsc` against the stored baselines (no Node/`tsc` required):

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts-harness --release --locked -- \
  difftsc \
  --suite typecheck-ts-harness/fixtures/difftsc \
  --compare-rust \
  --use-baselines \
  --manifest typecheck-ts-harness/fixtures/difftsc/manifest.toml \
  --json > difftsc-report.json
```

## Submodules and test corpora

This repo has two optional-but-important submodules:

- `parse-js/tests/TypeScript` — upstream TypeScript repo (conformance corpus + baseline files)
- `test262/data` — test262 parser tests corpus

Init them as needed:

```bash
git submodule update --init --recursive parse-js/tests/TypeScript
git submodule update --init test262/data
```

(Or `just submodules`; `just setup` also does this.)

Which CI jobs use them:

- `test262/data` is fetched by the **`test262-parser`** job in [`.github/workflows/ci.yaml`](./.github/workflows/ci.yaml).
- `parse-js/tests/TypeScript` is used by the nightly **`ts-conformance`** workflow in [`.github/workflows/nightly.yaml`](./.github/workflows/nightly.yaml) (and by local `typecheck-ts-harness conformance` runs targeting the upstream suite).

## UTF-8 / source-text policy

All “source text” APIs in this workspace use `&str` / `Arc<str>` (valid UTF-8). This keeps:

- spans/offsets consistent (byte offsets in UTF-8),
- identifier handling correct,
- and prevents a split-brain between “bytes” and “text” entrypoints.

The repo enforces this with [`scripts/check_utf8_apis.sh`](./scripts/check_utf8_apis.sh) (also exercised by `diagnostics/tests/utf8_api_guard.rs`).

## Determinism, incremental queries, and profiling

High-level goals (see [`AGENTS.md`](./AGENTS.md) for the full playbook):

- **Determinism**: stable ordering/IDs/diagnostics regardless of parallelism.
- **Incremental-ready**: coarse-grained, query-based computation (parse → HIR → bind → check).

Profiling hooks:

- `typecheck-ts-harness conformance --profile --profile-out typecheck_profile.json` writes an aggregated profile report.
- `typecheck-ts-cli ... --profile` / `--trace` emits JSON tracing spans on stderr (redirect with `2> trace.jsonl`).
