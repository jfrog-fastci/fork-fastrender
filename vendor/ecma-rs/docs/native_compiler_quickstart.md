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
- Type assertions (`x as T`, `<T>x`)
- Non-null assertions (`x!`)
- Dynamic code execution: `eval()`, `Function()`, `new Function()`
- `with`
- `arguments`
- Computed property access with non-constant keys in strict paths (`obj[key]` where `key` is not a constant)
- Prototype mutation after construction (e.g. patching `Foo.prototype.*` at runtime)
- `Proxy` (disallowed or extremely restricted)

**Enforced today** by `typecheck-ts` when you pass `--native-strict` (or legacy `--strict-native`):

- `TC4000`: `any` (explicit or inferred)
- `TC4001`: `eval(...)` (incl `globalThis.eval(...)`)
- `TC4002`: `Function(...)` / `new Function(...)`
- `TC4003`: `with`
- `TC4004`: `arguments` (in function scopes)
- `TC4005`: unsafe type assertions
- `TC4006`: non-null assertions on maybe-nullish values
- `TC4007`: computed property access with non-constant keys (`obj[key]`)
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

### Command (inside the ecma-rs workspace)

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

### Command (inside the ecma-rs workspace)

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

Expected output is standard test output.
Today the harness asserts that fixtures erase to JS and execute successfully in the oracle runtime; native-vs-oracle comparison is expected to be added later.

> Note: the oracle fixture corpus intentionally includes some **TypeScript-only expression wrappers**
> (e.g. `as`, `!`, `satisfies`, instantiation/type arguments). These are useful for hardening the
> TS→JS erasure pipeline even though the strict-native validator may reject them for native AOT.

#### Optional: enable the `optimize-js` TS→JS fallback

If TS→JS erasure fails (common causes: `ts-erase` rejects TypeScript *runtime* constructs like `enum`/`namespace` in strict-native mode,
or `emit-js` cannot emit a statement kind like `switch` yet), you can enable the heavier fallback:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p native-oracle-harness --features optimize-js-fallback
```

This switches the erasure step to:

- try `ts-erase` + `emit-js` first, then
- compile + decompile via `optimize-js` when needed.

### Related native pipeline smoke tests (LLVM)

In addition to the oracle harness, there are two native bring-up CLIs:

- `native-js-cli`: minimal `parse-js` → LLVM IR emitter (no typechecking)
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

1. Parses the input as TypeScript (`parse-js`, `Dialect::Ts`, `SourceType::Script`).
   - This is a **syntax-only** parse (no `typecheck-ts` run).
2. Erases TypeScript-only syntax using the shared `ts-erase` pipeline (`ts_erase::erase_types_strict_native` / `TsEraseMode::StrictNative`).
3. Emits JavaScript using `emit-js` (`emit_js::emit_top_level_diagnostic`).
4. Optionally falls back to `optimize-js` decompilation when built with the `optimize-js-fallback` feature.
5. Executes the erased JS using `vm-js`.

Today the harness test suite primarily asserts that fixtures:

- successfully erase to JS, and
- execute successfully in the oracle runtime.

It does **not** currently run the TypeScript checker in strict-native mode; run `typecheck-ts-cli --native-strict` (or legacy `--strict-native`)
as a separate step when you want strict-native enforcement.

Native execution + result comparison is expected to be layered in as the native pipeline matures.

---

## 5) Fixture layout (oracle harness)

Fixtures live under:

```
vendor/ecma-rs/fixtures/native_oracle/*.ts
```

Each file is a standalone **TypeScript script** (not a module) that should be erasable to JS and runnable under the oracle VM.

Guidelines for fixtures:

- Keep them **deterministic**: avoid real time, randomness, networking, and filesystem access unless explicitly mocked.
- Avoid host APIs: `vm-js` does not provide browser/Node globals like `console` by default.

To add a new fixture:

1. Create a new `*.ts` file under `vendor/ecma-rs/fixtures/native_oracle/`.
2. Ensure it parses as a **script** (no top-level `import`/`export`).
3. Run the harness tests (see section 3).

For exact discovery/execution rules, see `native-oracle-harness/src/lib.rs` (the crate has a self-test that discovers and runs these fixtures).

---

## Appendix: interpreting strict-native diagnostics

Strict-native checks can come from multiple layers:

- `TC40xx` codes: emitted by `typecheck-ts` when `--native-strict` (or legacy `--strict-native`) is enabled.
  - Today this is `TC4000`–`TC4009` and is expected to grow.
- `NJS####` codes: emitted by `native-js` strict subset validation.
  - `native_js::validate::validate_strict_subset` is used by the typechecked `native-js` AOT CLI.
  - `native_js::strict::validate` is a legacy validator that still emits `NJS####` codes for tests/tooling.

See [`docs/diagnostic-codes.md`](./diagnostic-codes.md) for the repo-wide prefix registry.
