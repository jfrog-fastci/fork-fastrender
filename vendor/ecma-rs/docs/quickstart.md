# Local quickstart (dev + conformance)

This repo is a Rust workspace (toolchain pinned via [`rust-toolchain.toml`](../rust-toolchain.toml)) plus optional JS/TS corpora and Node tooling for differential tests.

If you only want to build and run the core crates/CLIs, you **do not** need Node or submodules.

If you're working on the **native compiler** track described in [`EXEC.plan.md`](../EXEC.plan.md), see
[`docs/native_compiler_quickstart.md`](./native_compiler_quickstart.md) for strict-native + VM-oracle guidance.

## Workspace defaults (baseline vs full workspace)

This workspace uses Cargo's `default-members` so a plain `cargo test` stays lightweight:

- `cargo test` / `cargo check` runs the **default members** (core crates that do not require LLVM or external corpora).
- `cargo test --workspace` runs **everything** in the workspace (includes optional harnesses + the LLVM-backed native compiler crates).

For LLVM-backed crates (e.g. `native-js`, `runtime-native`), use [`scripts/cargo_llvm.sh`](../scripts/cargo_llvm.sh) and ensure LLVM 18 is installed (`scripts/check_system.sh`).

## Nested-workspace note (when vendored in FastRender)

In the FastRender mono-repo, `ecma-rs` lives under `vendor/ecma-rs/` and is **not** part of the
top-level Cargo workspace.

If you're running commands from the FastRender repo root, prefer the nested wrapper so `cargo`
runs against the correct workspace *and* picks up the pinned toolchain:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh check -p parse-js
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib
```

> Note: the native backend crates (`native-js`, `runtime-native`) require LLVM 18
> to be installed. See [`native-js/README.md`](../native-js/README.md) (and
> `scripts/check_system.sh`) for setup instructions.

## One-command bootstrap (recommended)

If you're setting up a checkout for TypeScript conformance / differential testing and you have [`just`](https://github.com/casey/just) + Node.js installed:

```bash
just setup
```

`just setup` is safe to run multiple times. It:

- Fetches the required git submodules (TypeScript + test262 data)
- Installs npm dependencies for `typecheck-ts-harness` (`npm ci`)
- Generates an untracked workspace `Cargo.lock`
- Runs a small sanity check (`cargo check -p typecheck-ts-harness --locked`)
- Verifies the pinned `typescript` npm package is usable

## 0) Generate a lockfile (Cargo.lock is untracked)

CI generates `Cargo.lock` on the fly and then uses `--locked` for reproducible resolution. Locally:

```bash
cargo generate-lockfile
```

(Or `just lockfile` if you have `just` installed.)

## 1) Run the local CI shorthand

If you have [`just`](https://github.com/casey/just):

```bash
just ci
```

This runs the main `lint`/`check`/`test` suite and regenerates [`docs/deps.md`](./deps.md).
CI runs the underlying `cargo` commands with `--locked` after generating `Cargo.lock`.

## 2) Run the in-repo examples (no filesystem I/O)

The repository includes compiled examples that demonstrate the public APIs of the
core crates. See [`docs/examples.md`](./examples.md) for the full list.

TypeScript checker examples:

```bash
cargo run -p typecheck-ts --example memory_host_basic
cargo run -p typecheck-ts --example json_snapshot
```

## 3) Optional submodules (TypeScript + test262)

Two test corpora live in submodules (see [`.gitmodules`](../.gitmodules)):

```bash
# Upstream TypeScript repo (for conformance suites)
git submodule update --init --recursive parse-js/tests/TypeScript

# test262 parser corpus (for test262 runner)
git submodule update --init test262/data
```

(Or `just submodules` if you have `just` installed.)

### Running the test262 parser job locally

This matches the GitHub Actions `test262-parser` job:

```bash
mkdir -p reports
cargo run -p test262 --release --locked -- \
  --data-dir test262/data \
  --manifest test262/manifest.toml \
  --report-path reports/test262-parser-report.json \
  --fail-on new
```

## 4) TypeScript conformance + difftsc locally (Node required)

The full “run against upstream conformance suites” flow requires:

1. The TypeScript submodule checked out at `parse-js/tests/TypeScript/`
2. Node.js installed
3. The `typescript` npm package installed next to the harness

The harness has a pinned `package-lock.json`, so `npm ci` is the recommended way:

```bash
git submodule update --init --recursive parse-js/tests/TypeScript
cd typecheck-ts-harness && npm ci
```

(Or `just node-deps` to install npm dependencies; `just setup` does everything.)

Then run a shard of conformance tests (this is similar to `.github/workflows/nightly.yaml`):

```bash
cargo run -p typecheck-ts-harness --release --locked -- \
  conformance \
  --root parse-js/tests/TypeScript/tests/cases/conformance \
  --shard 0/16 \
  --compare tsc \
  --timeout-secs 20 \
  --json > conformance-0.json
```

For differential testing (`difftsc`), you can either:

- compare against stored baselines (no Node required), or
- run against a live `tsc` install (requires Node + `typescript`).

See [`typecheck-ts-harness/README.md`](../typecheck-ts-harness/README.md) for:

- `difftsc` modes (`--compare-rust`, `--use-baselines`, `--update-baselines`)
- profiling and trace output (`--profile`, `--trace`)
- corpus discovery rules and fixtures layout
