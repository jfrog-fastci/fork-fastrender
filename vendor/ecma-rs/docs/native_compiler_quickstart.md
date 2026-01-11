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

**Enforced today** by `native-js`’s strict validator (`native_js::strict::validate`):

- `any` (explicit or inferred) (`NJS0001`)
- Type assertions (`x as T`, `<T>x`) (`NJS0002`)
- Non-null assertions (`x!`) (`NJS0003`)
- `eval()` (`NJS0004`)
- `new Function()` (`NJS0005`)
- `with` statements (`NJS0006`)
- Computed property access with non-literal keys (`obj[key]` where `key` is not a string/number literal) (`NJS0007`)
- Use of the `arguments` identifier/object (`NJS0008`)

See [`native-js/README.md`](../native-js/README.md) for the canonical “enforced today” list (with diagnostic codes).

**Also rejected by the overall strict-native design** (in [`EXEC.plan.md`](../EXEC.plan.md); enforcement may land later):

- Prototype mutation after construction (e.g. patching `Foo.prototype.*` at runtime)
- `Proxy` (disallowed or extremely restricted)

### Restricted constructs (allowed with constraints)

- Union types: allowed, but lower to tag-checked code. Prefer **discriminated unions** for performance and clarity.
- `unknown`: allowed, but must be narrowed before use.
- Dynamic property access: may be routed to a slow path (and can be diagnosed). Prefer known shapes and direct property access.

See [`EXEC.plan.md`](../EXEC.plan.md) → “Our TypeScript Dialect” for the canonical list and rationale.

---

## 2) Typecheck in strict-native mode

### TypeScript typechecking (tsc-like semantics)

If you’re in `vendor/ecma-rs/`:

```bash
cargo run -p typecheck-ts-cli -- typecheck path/to/file.ts
```

### Strict-native validation (native-js strict subset)

Strict-native is currently enforced by `native-js`’s strict validator (see `native-js/tests/strict_validator.rs` for examples).

To run the strict validator regression tests:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --test strict_validator

# Or, if you're already in vendor/ecma-rs/:
bash scripts/cargo_llvm.sh test -p native-js --test strict_validator
```

> Note: a standalone CLI flag is expected to exist eventually (as described in `EXEC.plan.md`).
> Once implemented, it should look like:
>
> ```bash
> cargo run -p typecheck-ts-cli -- typecheck --strict-native path/to/file.ts
> ```
>
> Or from the repo root with the wrapper:
>
> ```bash
> bash scripts/cargo_agent.sh run \
>   --manifest-path vendor/ecma-rs/Cargo.toml \
>   -p typecheck-ts-cli -- \
>   typecheck --strict-native path/to/file.ts
> ```

### Recommended wrapper (agent-safe)

Use the repo’s concurrency/RAM-limiting wrapper for the vendored ecma-rs workspace:

```bash
# From the repo root (recommended):
bash vendor/ecma-rs/scripts/cargo_agent.sh run -p typecheck-ts-cli -- \
  typecheck typecheck-ts-cli/fixtures/basic.ts

# Or, if you're already in vendor/ecma-rs/:
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- \
  typecheck typecheck-ts-cli/fixtures/basic.ts
```

Expected behavior:

- On success: no output, exit code `0`.
- On failure: diagnostics are printed and the process exits non-zero.

Tip: add `--json` to emit structured diagnostics/output for tooling.

---

## 3) Run the VM oracle harness

The “native compiler” work needs a correctness backstop. We use a **VM oracle**:

- Compile/erase TypeScript to JavaScript,
- Run the JS under our deterministic interpreter, [`vm-js`](../vm-js/),
- Compare its result against the native pipeline output.

### Raw cargo command (inside the ecma-rs workspace)

```bash
cargo test -p native-oracle-harness
```

> Note: if `native-oracle-harness` does not exist in your checkout yet, the closest “native pipeline smoke test”
> today is `native-js-cli` (TS → LLVM IR → native executable) for a tiny expression-only subset:
>
> ```bash
> bash vendor/ecma-rs/scripts/cargo_llvm.sh run -p native-js-cli -- /tmp/main.ts
> ```

### Recommended (LLVM-heavy wrapper)

Native compilation and codegen are LLVM-heavy; use the LLVM wrapper (higher RAM limit + LLVM env auto-detect):

```bash
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-oracle-harness
```

Expected output is standard `cargo test` output; any mismatch between native output and the `vm-js` oracle should show up as a failing test referencing the fixture name.

---

## 4) TS → JS → `vm-js` oracle flow (what’s happening)

At a high level, each oracle test case does:

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

---

## 5) Fixture layout (oracle harness)

Fixtures are organized as on-disk directories so they can be versioned, reviewed, and bisected easily.

Expected layout:

```
vendor/ecma-rs/native-oracle-harness/fixtures/
  <case-name>/
    main.ts
    # optional additional modules imported by main.ts:
    dep.ts
    nested/other.ts
    # optional per-case notes / configuration:
    README.md
    tsconfig.json
```

Guidelines for fixtures:

- Keep them **deterministic**: avoid real time, randomness, networking, and filesystem access unless explicitly mocked.
- Prefer returning a value from `main()` (or exporting a value the harness reads) over relying on side effects.
- If you need multiple files, use relative imports within the fixture directory (the harness treats it as an isolated mini-project).

For the exact discovery rules and result schema, see the `native-oracle-harness` crate sources/tests once you’re modifying fixtures.
