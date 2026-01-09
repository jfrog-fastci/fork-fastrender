# `ecma-rs` integration + submodule workflow

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

AGENTS.md is the law. These rules are not suggestions. Violating them destroys host machines, wastes hours of compute, and blocks other agents. Non-compliance is unacceptable.

**MANDATORY (no exceptions):**
- Use `scripts/cargo_agent.sh` for ALL cargo commands (build, test, check, clippy)
- Use `scripts/run_limited.sh --as 64G` when executing ANY renderer binary
- Scope ALL test runs (`-p <crate>`, `--test <name>`, `--lib`) — NEVER run unscoped tests

**FORBIDDEN — will destroy the host:**
- `cargo build` / `cargo test` / `cargo check` without wrapper scripts
- `cargo test --all-features` or `cargo check --all-features --tests`
- Unscoped `cargo test` (compiles 300+ test binaries and blows RAM)

If you do not understand these rules, re-read AGENTS.md. There are no exceptions. Ignorance is not an excuse.

---

FastRender uses `ecma-rs` as the JS/TS language implementation and will evolve it as needed for
browser-grade JavaScript execution.

`ecma-rs` is a **git submodule** at:

- `engines/ecma-rs/` (`https://github.com/wilsonzlin/ecma-rs.git`)

CI uses HTTPS so it can fetch the submodule without SSH credentials. If you prefer SSH locally, use
a Git URL rewrite:

```bash
git config --global url."git@github.com:".insteadOf "https://github.com/"
```

If you already initialized the submodule before the HTTPS switch, sync the stored URL:

```bash
git submodule sync -- engines/ecma-rs
```

## Initializing the submodule

From the FastRender repo root:

```bash
git submodule update --init engines/ecma-rs
```

Note: `ecma-rs` itself contains optional nested submodules (large test corpora). Only initialize
those when you intend to run those conformance suites:

```bash
git -C engines/ecma-rs submodule update --init --recursive
```

## Making changes in `ecma-rs` (and updating the pointer here)

Work happens in two repos:

1. **Commit + push inside `ecma-rs`**
2. **Update the submodule pointer in FastRender**

### 1) Edit + commit in `ecma-rs`

Inside the submodule, make your change, commit it, and push it to GitHub.

Policy:

- Prefer **rebase**, not merge, when syncing with upstream.
- Keep commits small and reviewable.
- Run focused checks/tests where possible (but always under resource caps; see below).

### 2) Update the parent pointer (FastRender)

After pushing in `ecma-rs`, the FastRender repo will show `engines/ecma-rs` as “modified” (the
checked-out SHA changed). Record that pointer update by committing it in FastRender:

```bash
git add engines/ecma-rs
git commit -m "chore(js): bump ecma-rs"
```

## Running `ecma-rs` commands safely (resource limits)

JS conformance workloads can be pathological. Use OS caps from the FastRender repo when running
`cargo` inside the submodule.

Example pattern:

```bash
scripts/run_limited.sh --as 64G -- bash -lc 'cd engines/ecma-rs && cargo test -p parse-js'
```

For builds/tests, avoid multi-agent cargo stampedes (same principle as FastRender):

- Don’t run unscoped `cargo test` across the entire workspace unless necessary.
- Prefer scoping: `-p <crate>`, `--test <name>`, `--example <name>`.

## Where engine work should live

`ecma-rs` already has strong parsing/IR/semantics infrastructure. For browser execution we will
likely add new crate(s) such as:

- `vm-js` (runtime/GC/object model/execution)
- `host-web` (host hooks for web embedding: timers, module loading, fetch glue)

Keep the boundaries clean:

- `ecma-rs` owns JS language semantics and execution primitives.
- FastRender owns DOM/layout/paint and the browser embedding logic.
