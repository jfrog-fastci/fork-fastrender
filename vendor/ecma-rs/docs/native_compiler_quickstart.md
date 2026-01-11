# Native compiler quickstart (strict-native + VM oracle)

This is a practical guide for the **native compiler** track described in [`EXEC.plan.md`](../EXEC.plan.md).
It’s aimed at developers/agents working on:

- the **strict-native** TypeScript dialect (a strict subset we can compile/optimize reliably), and
- the **TS → JS → `vm-js`** oracle harness used to validate native output.

For the full rationale and long-form plan, read [`EXEC.plan.md`](../EXEC.plan.md) (source of truth).

---

## 0) Verify system dependencies

From the **repo root** (the checkout that contains `vendor/ecma-rs/`), run:

```bash
bash vendor/ecma-rs/scripts/check_system.sh
```

If you’re working inside `vendor/ecma-rs/` directly:

```bash
bash scripts/check_system.sh
```

Expected output:

- A list of `✓` / `✗` checks for `rustc`, `cargo`, `flock`, `prlimit`, LLVM, etc.
- A final summary ending in either:
  - `All checks passed`, or
  - `OK with N warning(s)`.

If the script exits non-zero, it prints the missing packages to install.

---

## 1) What “strict-native” means

**Strict-native** is the TypeScript dialect we accept for ahead-of-time native compilation. The goal is to keep code:

- statically analyzable (no hidden dynamic behavior),
- type-driven (types are trusted enough to optimize aggressively),
- and predictable (no “escape hatches” that bypass checks).

This is **stricter than** `tsc --strict`: code that TypeScript accepts can still be rejected here.

### Compile errors (rejected constructs)

Strict-native rejects (hard error, not warning):

**Strict-native design** (see [`EXEC.plan.md`](../EXEC.plan.md) for rationale):

- `any` (explicit or inferred)
- Unsafe type assertions that bypass checking (`x as T`, `<T>x`)
- Non-null assertions on values that might be null/undefined (`x!`)
- Dynamic code execution: `eval()`, `Function()`, `new Function()`
- `with`
- `arguments`
- Computed property access / computed property names with non-constant keys (e.g. `obj[key]`, `{ [key]: ... }`, `{ [key]: v } = ...`)
- Prototype mutation after construction (e.g. patching `Foo.prototype.*` at runtime)
- `Proxy` (disallowed or extremely restricted)

**Enforced today** by `typecheck-ts` when strict-native is enabled (`--native-strict` or `--strict-native`):

- `TC4000`: `any` (explicit or inferred)
- `TC4001`: `eval(...)` (incl `globalThis.eval(...)`)
- `TC4002`: `Function(...)` / `new Function(...)`
- `TC4003`: `with`
- `TC4004`: `arguments` (use sites and binding sites)
- `TC4005`: unsafe type assertions
- `TC4006`: non-null assertions on maybe-nullish values
- `TC4007`: computed property access / computed property names with non-constant keys
- `TC4008`: `Proxy` (incl `Proxy.revocable(...)`)
- `TC4009`: prototype mutation (`__proto__` assignments, `Object/Reflect.setPrototypeOf`, etc.)

> Strict-native enforcement is intentionally incremental. Expect this list to grow as native compilation work
> proceeds.

**Also enforced today** by `native-js`’s strict subset validator (`native_js::validate::validate_strict_subset`, diagnostics use the `NJS####` prefix):

- `NJS0009`: unsupported syntax in the strict subset (e.g. `with`, `try`/`throw`, `eval`, `super`, `yield`, `await`, object/array literals, property access, classes, …)
- `NJS0010`: unsupported types in the strict subset (e.g. `any`, `unknown`, unions/intersections, object/callable/reference types, `bigint`, `symbol`, …)

> Note: TS-only **runtime-inert** expression wrappers such as `satisfies`, type assertions (`as`),
> and non-null assertions (`!`) are allowed by this strict subset validator, but the wrapped runtime
> expressions are still validated.

The older `native_js::strict::validate` API remains as a legacy validator (used in tests/tooling) and
emits `NJS0001`–`NJS0008`.

See [`native-js/README.md`](../native-js/README.md) for the canonical `native_js::strict` list (with diagnostic codes).

### Restricted constructs (allowed with constraints)

- Union types: allowed, but lower to tag-checked code. Prefer **discriminated unions** for performance and clarity.
- `unknown`: allowed, but must be narrowed before use.
- Dynamic property access: may be routed to a slow path (and can be diagnosed). Prefer known shapes and direct property access.

See [`EXEC.plan.md`](../EXEC.plan.md) → “Our TypeScript Dialect” for the canonical list and rationale.

---

## 2) Typecheck in strict-native mode

### Inside the ecma-rs workspace

If you’re in `vendor/ecma-rs/`:

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- typecheck --native-strict path/to/file.ts
```

### Recommended wrapper (agent-safe)

Use the repo’s concurrency/RAM-limiting wrapper for the vendored ecma-rs workspace:

```bash
# From the repo root (recommended):
bash vendor/ecma-rs/scripts/cargo_agent.sh run -p typecheck-ts-cli -- \
  typecheck --native-strict typecheck-ts-cli/fixtures/basic.ts

# Or, if you're already in vendor/ecma-rs/:
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- \
  typecheck --native-strict typecheck-ts-cli/fixtures/basic.ts
```

Expected behavior:

- On success: no output, exit code `0`.
- On failure: diagnostics are printed and the process exits non-zero.

Tip: add `--json` to emit structured diagnostics/output for tooling.

### Enable strict-native via `tsconfig.json` (project mode)

If you’re using `--project` to load a `tsconfig.json`, you can also enable strict-native in the config:

```jsonc
{
  "compilerOptions": {
    "nativeStrict": true
  }
}
```

`compilerOptions.strictNative` is also accepted as a legacy key.

### Native strict-subset validator tests (`native-js`)

In addition to the checker’s `--native-strict` / `--strict-native` mode, `native-js` has a (currently broader) strict-subset validator
with `NJS####` diagnostics. To run its regression tests:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --test strict_subset

# Or, if you're already in vendor/ecma-rs/:
bash scripts/cargo_llvm.sh test -p native-js --test strict_subset
```

---

## 3) Run the VM oracle harness

The “native compiler” work needs a correctness backstop. We use a **VM oracle**:

- Compile/erase TypeScript to JavaScript,
- Run the JS under our deterministic interpreter, [`vm-js`](../vm-js/),
- (Eventually) compare oracle behavior against the native pipeline output.

### Inside the ecma-rs workspace

```bash
bash scripts/cargo_agent.sh test -p native-oracle-harness
```

### Recommended (agent-safe wrapper)

This crate is not LLVM-heavy today, so use the standard agent wrapper:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p native-oracle-harness

# Or, if you're already in vendor/ecma-rs/:
bash scripts/cargo_agent.sh test -p native-oracle-harness
```

To run just the fixture comparison test (useful when iterating on TS→JS erasure):

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p native-oracle-harness --test fixtures

# Or, if you're already in vendor/ecma-rs/:
bash scripts/cargo_agent.sh test -p native-oracle-harness --test fixtures
```

There is also a small binary (`native-oracle-harness/src/main.rs`) that runs the same `// EXPECT:` / `*.out` TS fixture comparisons:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_agent.sh run -p native-oracle-harness

# Or, if you're already in vendor/ecma-rs/:
bash scripts/cargo_agent.sh run -p native-oracle-harness
```

Expected output is standard `cargo test` output for the test invocations; the binary prints `ok ...` / `FAIL ...`
lines and exits non-zero on failure.
Today the harness runs:

- **TypeScript/TSX fixtures**: erase TS→JS and execute them in the oracle runtime, then (for fixtures that declare an
  expectation) compare `String(globalThis.__native_result)` against `// EXPECT:` (or a sibling `*.out` file).
- **JavaScript fixtures**: execute `.js` promise/microtask fixtures directly under `vm-js` and assert on returned output.

Native-vs-oracle comparison is expected to be added later.

> Note: the oracle fixture corpus intentionally includes some **TypeScript-only expression wrappers**
> (e.g. `as`, `!`, `satisfies`, instantiation/type arguments). These are useful for hardening the
> TS→JS erasure pipeline even though the current native backend / strict-subset validator may reject them for native AOT.

#### Optional: enable the `optimize-js` TS→JS fallback

If TS→JS erasure fails (common causes: `ts-erase` rejects TypeScript *runtime* constructs like `enum`/`namespace` in strict-native mode,
or `emit-js` reports unsupported syntax during emission), you can enable the heavier fallback:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p native-oracle-harness --features optimize-js-fallback
```

This switches the erasure step to:

- try `ts-erase` + `emit-js` first, then
- compile + decompile via `optimize-js` when needed.

### Related native pipeline smoke tests (LLVM)

In addition to the oracle harness, there are two native bring-up CLIs:

- `native-js-cli`: minimal `parse-js` → LLVM IR emitter (no typechecked codegen; uses `typecheck-ts`
  only for module graph discovery in the multi-file ES module subset)
- `native-js`: experimental **typechecked AOT** pipeline (`typecheck-ts` + strict validation + HIR → LLVM)

Both require LLVM; use the LLVM wrapper:

```bash
# Minimal emitter:
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- /tmp/main.ts

# Typechecked AOT pipeline (expects `export function main()`):
cat > /tmp/aot.ts <<'TS'
export function main(): number { return 0; }
TS
bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli --bin native-js -- run /tmp/aot.ts
```

See [`native-js-cli/README.md`](../native-js-cli/README.md) for details and flags.

---

## 4) TS → JS → `vm-js` oracle flow (what’s happening)

### Target flow (from `EXEC.plan.md`)

At a high level, each oracle test case should:

1. **Input**: a strict-native `.ts` entry file (plus optional imported modules in the same fixture).
2. **Typecheck**: run `typecheck-ts` in strict-native mode; rejected constructs fail the test early.
3. **Type erasure / JS emission**: produce runnable JS that preserves runtime semantics (types removed, TS-only constructs lowered).
4. **Oracle execution**: run that JS in [`vm-js`](../vm-js/) to obtain a deterministic reference result.
5. **Native execution**: compile/run the same input through the native pipeline.
6. **Comparison**: compare native vs oracle:
   - returned value vs thrown exception,
   - and (when relevant) captured stdout/stderr.

The important property is that `vm-js` is deterministic and spec-oriented, so the oracle result is stable across machines and CI.

Note: `vm-js` executes **ECMAScript** (`Dialect::Ecma`) scripts, not TypeScript. The oracle flow therefore depends on a TS → JS “type erasure” step.

### What exists today in this repo

The current `native-oracle-harness` crate provides the TS → JS erasure step as a library function:

- `native_oracle_harness::erase_typescript_to_js(&str) -> Result<String, TsToJsError>`

It:

1. Parses the input as TypeScript/TSX (`parse-js`, `Dialect::Ts` with a `Dialect::Tsx` fallback, `SourceType::Script`).
   - This is a **syntax-only** parse (no `typecheck-ts` run).
2. Erases TypeScript-only syntax using the shared `ts-erase` pipeline (`ts_erase::erase_types_strict_native` / `TsEraseMode::StrictNative`).
3. Emits JavaScript using `emit-js` (`emit_js::emit_top_level_diagnostic`).
4. Optionally falls back to `optimize-js` decompilation when built with the `optimize-js-fallback` feature.
5. Executes the erased JS using `vm-js`.

Today the harness includes:

- a unit test that asserts `*.ts` fixtures successfully erase to JS and execute in the oracle runtime.
- a fixtures test (`native-oracle-harness/tests/fixtures.rs`) that runs `*.ts`/`*.tsx` fixtures with `// EXPECT:` (or `*.out`) and compares the observed output.
- promise/microtask tests (under `native-oracle-harness/tests/`) that run `*.js` fixtures directly.

It does **not** currently run the TypeScript checker in strict-native mode; run `typecheck-ts-cli --native-strict` (alias: `--strict-native`)
as a separate step when you want strict-native enforcement.

Native execution + result comparison is expected to be layered in as the native pipeline matures.

---

## 5) Fixture layout (oracle harness)

Fixtures live under:

```
vendor/ecma-rs/fixtures/native_oracle/
```

There are two kinds of fixtures:

- `*.ts` / `*.tsx`: standalone **TypeScript scripts** (not modules) that should be erasable to JS and runnable under the oracle VM.
- `*.js`: standalone **JavaScript scripts** that are executed directly by `vm-js` (bypassing TS→JS erasure).
  This keeps some oracle tests (notably Promises/microtasks) independent of `emit-js` feature coverage.

Guidelines for fixtures:

- Keep them **deterministic**: avoid real time, randomness, networking, and filesystem access unless explicitly mocked.
- Avoid host APIs: `vm-js` does not provide browser/Node globals like `console` by default.
- For `*.ts` / `*.tsx` fixtures that want output comparison, set `globalThis.__native_result` and declare the expected output:
  - `// EXPECT: ...` anywhere in the file, or
  - a sibling `*.out` file with the same basename.
  The harness evaluates `String(globalThis.__native_result)` after a microtask checkpoint and compares it to the expected output.
- For `*.js` fixtures intended to be used with `native_oracle_harness::run_fixture*`, ensure the script completion value is a
  `string` or `Promise<string>` (the harness does not currently provide a macro-task/event loop).

To add a new fixture:

1. Create a new `*.ts`/`*.tsx` (TS→JS erasure) or `*.js` (direct `vm-js`) file under `vendor/ecma-rs/fixtures/native_oracle/`.
2. Ensure it parses as a **script** (no top-level `import`/`export`).
3. Run the harness tests (see section 3).

For exact execution rules, see `native-oracle-harness/src/lib.rs` (TS→JS erasure + `run_fixture` logic) and
`native-oracle-harness/tests/*.rs` for fixture discovery and output expectations.

---

## Appendix: interpreting strict-native diagnostics

Strict-native checks can come from multiple layers:

- `TC40xx` codes: emitted by `typecheck-ts` when strict-native is enabled (`--native-strict` / `--strict-native`).
  - Today this is `TC4000`–`TC4009` and is expected to grow.
- `NJS####` codes: emitted by `native-js` strict subset validation.
  - `native_js::validate::validate_strict_subset` is used by the typechecked `native-js` AOT CLI.
  - `native_js::strict::validate` is a legacy validator that still emits `NJS####` codes for tests/tooling.

See [`docs/diagnostic-codes.md`](./diagnostic-codes.md) for the repo-wide prefix registry.
