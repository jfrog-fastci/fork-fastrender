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

- `any` (explicit or inferred)
- Type assertions that “lie” (casts that the checker can’t justify)
- Non-null assertions on potentially-null/undefined values (`x!`)
- Dynamic code execution: `eval()`, `new Function()`
- `with`
- `arguments` (use rest parameters instead)
- Prototype mutation after construction (e.g. patching `Foo.prototype.*` at runtime)
- Computed property access with non-constant keys in strict paths (`obj[key]` where `key` isn’t a constant)
- `Proxy` (disallowed or extremely restricted)

### Restricted constructs (allowed with constraints)

- Union types: allowed, but lower to tag-checked code. Prefer **discriminated unions** for performance and clarity.
- `unknown`: allowed, but must be narrowed before use.
- Dynamic property access: may be routed to a slow path (and can be diagnosed). Prefer known shapes and direct property access.

See [`EXEC.plan.md`](../EXEC.plan.md) → “Our TypeScript Dialect” for the canonical list and rationale.

---

## 2) Typecheck in strict-native mode

### Raw cargo command (inside the ecma-rs workspace)

If you’re in `vendor/ecma-rs/`:

```bash
cargo run -p typecheck-ts-cli -- typecheck --strict-native path/to/file.ts
```

### Recommended (agent-safe wrapper)

Use the repo’s concurrency/RAM-limiting wrapper for the vendored ecma-rs workspace:

```bash
# From the repo root:
bash vendor/ecma-rs/scripts/cargo_agent.sh run -p typecheck-ts-cli -- \
  typecheck --strict-native typecheck-ts-cli/fixtures/basic.ts

# Or, if you're already in vendor/ecma-rs/:
bash scripts/cargo_agent.sh run -p typecheck-ts-cli -- \
  typecheck --strict-native typecheck-ts-cli/fixtures/basic.ts
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
