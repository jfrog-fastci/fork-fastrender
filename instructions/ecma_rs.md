# `ecma-rs` integration (vendored)

> **⚠️ DEPRECATED**: This integration guide has been superseded:
>
> - For **FastRender JS engine work**, see [`js_engine.md`](js_engine.md)
> - For **ecma-rs workstreams**, see [`vendor/ecma-rs/AGENTS.md`](../vendor/ecma-rs/AGENTS.md)
>
> FastRender **owns** ecma-rs and modifies it directly. All JS engine work should go through the
> appropriate workstream above. This file is kept for historical reference.

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

`ecma-rs` parses and executes JavaScript — a Turing-complete language designed to run untrusted code. **Any test can trigger infinite loops, exponential parsing, or unbounded allocations.** Parser bugs can cause hangs. Conformance tests (test262) exercise edge cases that stress the engine.

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL (SIGTERM alone can be ignored)
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

If something exceeds limits, that's a **bug to investigate**, not a limit to raise.

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh ...` for ecma-rs workspace
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries
- Scope ALL test runs (`-p <crate>`, `--test <name>`, `--lib`) — NEVER run unscoped tests

**FORBIDDEN — will destroy the host:**
- ANY command without `timeout -k` (can hang forever)
- `cargo build` / `cargo test` / `cargo check` without wrapper scripts
- `cargo test --all-features` or unscoped `cargo test`

---

FastRender uses `ecma-rs` as the JS/TS language implementation and will evolve it as needed for
browser-grade JavaScript execution.

`ecma-rs` is **vendored** at:

- `vendor/ecma-rs/` (originally from `https://github.com/wilsonzlin/ecma-rs.git`)

The vendored copy is part of the FastRender repository and can be modified directly.

## Test data submodules

`ecma-rs` has test corpora that are tracked as git submodules in the FastRender repo:

- `vendor/ecma-rs/test262/data` — tc39/test262-parser-tests (parser conformance)
- `vendor/ecma-rs/test262-semantic/data` — tc39/test262 (semantic conformance)
- `vendor/ecma-rs/parse-js/tests/TypeScript` — microsoft/TypeScript (TypeScript parsing tests)

Initialize them when needed:

```bash
# All test data submodules:
git submodule update --init vendor/ecma-rs/test262/data vendor/ecma-rs/test262-semantic/data vendor/ecma-rs/parse-js/tests/TypeScript

# Or individually:
git submodule update --init vendor/ecma-rs/test262-semantic/data
```

## Making changes to `ecma-rs`

Since `ecma-rs` is vendored, changes are made directly in `vendor/ecma-rs/` and committed to the
FastRender repository:

1. Edit files in `vendor/ecma-rs/`
2. Commit changes as part of your FastRender commit

No submodule pointer updates needed—the code is directly part of the repo.

## Common integration gotcha: `vm-js` host ABI changes

`vm-js` occasionally changes the embedding surface in ways that ripple into FastRender:

- `Vm::call` / `Vm::construct` may change how host integration is threaded (for example: adding or
  removing an explicit `VmHostHooks` parameter, and/or moving embedder state between an explicit
  argument vs `Vm::user_data`). `NativeCall` / `NativeConstruct` signatures change accordingly.
- `webidl-vm-js` sometimes calls `Vm::call` internally (e.g. iterator helpers). When the embedder
  host becomes required, those internal calls must switch to the corresponding "no host" helper
  (often `Vm::{call_without_host,construct_without_host}`), or be threaded with a real host context
  / hook implementation.
- `vm-js::spec_ops` (small spec-shaped helpers used by Promise/builtins) can also call into
  `Vm::{call,construct}`. When the host parameter becomes required, these helpers should use the same
  approach (`*_without_host` or explicit host threading) and avoid referencing removed internal APIs
  (for example older `Heap::get_function_call_id`/`get_function_construct_id` helpers).

If changes to `vendor/ecma-rs` break compilation with errors around `Vm::call` arity or native call
handler signatures, update both FastRender's native handlers and any engine-internal callers like
`webidl-vm-js`.

## FastRender workspace-local copy: `crates/webidl-vm-js`

Upstream `ecma-rs` includes a `webidl-vm-js` crate under `vendor/ecma-rs/webidl-vm-js`, but
FastRender uses a **workspace-local copy** at `crates/webidl-vm-js`.

This avoids ambiguity about which adapter FastRender should depend on (and keeps FastRender’s Cargo
workspace decoupled from the vendored `ecma-rs` workspace), while still allowing small FastRender
patches (for example: using `Vm::call_without_host` from within iterator helpers).

When updating `vendor/ecma-rs`, sync any relevant upstream changes into `crates/webidl-vm-js` and
validate with:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p webidl-vm-js
```

## Common integration gotcha: `vm-js` Promise job / microtask GC safety (FastRender requirement)

FastRender's HTML-shaped `EventLoop` owns the microtask queue. That queue is **not traced** by the
`vm-js` GC, so queued Promise jobs must be GC-safe: any `vm_js::Value` captured by a queued job must
be kept alive until the job runs (or is discarded).

Ensure the `vm-js` code includes **both**:

1. **GC-safe jobs** (persistent roots):
   - `vm_js::Job` supports owning persistent roots (e.g. `Job::add_root`, `Job::run`, `Job::discard`),
     and Promise job constructors root captured handles so they survive `Heap::collect_garbage()`
     between enqueue and execution.
2. **Canonical host hook API surface**:
   - Promise jobs are scheduled through `vm_js::VmHostHooks::host_enqueue_promise_job(job, realm)`
     (avoid older/duplicate job-queue traits/APIs).

FastRender encodes this expectation via:

- a compile-time API guard in `src/js/vmjs/window_timers.rs` (fails fast if `vm-js` regresses)
- regression tests:
  - `src/js/vmjs/window_timers.rs`: `vm_js_promise_jobs_root_captured_values_until_run`
  - `tests/misc/vm_js_promise_job_rooting.rs`

## Running `ecma-rs` commands safely (resource limits)

JS conformance workloads can be pathological. **Always use OS caps AND time limits** when running
Cargo commands for ecma-rs crates.

Example pattern:

```bash
# Always wrap with timeout -k (SIGKILL fallback) + run_limited.sh (RAM cap)
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash vendor/ecma-rs/scripts/cargo_agent.sh test -p parse-js
```

For LLVM-heavy crates (e.g. `native-js`, `runtime-native`, once they exist) prefer the LLVM wrapper (higher RAM cap + LLVM env):

```bash
# If wrapping with run_limited.sh, set --as >= the LLVM wrapper's limit (defaults to 96G).
timeout -k 10 900 bash scripts/run_limited.sh --as 96G -- \
  bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib
```

For builds/tests, avoid multi-agent cargo stampedes (same principle as FastRender):

- **Always use `timeout -k 10 <seconds>`** — pathological code can ignore SIGTERM
- Don't run unscoped `cargo test` across the entire workspace unless necessary.
- Prefer scoping: `-p <crate>`, `--test <name>`, `--example <name>`.

## Where engine work should live

`ecma-rs` already has strong parsing/IR/semantics infrastructure. For browser execution we will
likely add new crate(s) such as:

- `vm-js` (runtime/GC/object model/execution)
- `host-web` (host hooks for web embedding: timers, module loading, fetch glue)

Keep the boundaries clean:

- `ecma-rs` owns JS language semantics and execution primitives.
- FastRender owns DOM/layout/paint and the browser embedding logic.
