# Browser integration tests (headless + feature-gated)

This directory contains integration tests that exercise *browser-adjacent* code paths (document
rendering, UI worker protocol plumbing, and the `browser` binary startup hooks) **without requiring a
real window or GPU**.

The tests here are compiled into a single integration test binary:

- Harness entrypoint: `tests/browser_integration_tests.rs`
- Module aggregator: `tests/browser_integration/mod.rs`

This keeps test compilation/linking fast and avoids spawning hundreds of separate Rust test
executables.

## Adding new tests (required structure)

**Do not add new top-level `tests/*.rs` files** for browser integration coverage. Each top-level
file becomes a separate Cargo integration-test binary and is expensive to build/link.

Instead:

1. Add a new Rust module under `tests/browser_integration/` (for example:
   `tests/browser_integration/worker_protocol_smoke.rs`).
2. Register it in `tests/browser_integration/mod.rs` with `mod worker_protocol_smoke;`.

## Running safely (scoped + feature-gated)

Per `AGENTS.md`, **never** run unscoped `cargo test`. Always use the wrapper and scope to a single
test target.

Run the default (headless) subset:

```bash
scripts/cargo_agent.sh test --test browser_integration_tests
```

Run tests that are gated behind the UI feature (optionally filtered to a specific test name):

```bash
scripts/cargo_agent.sh test --test browser_integration_tests --features browser_ui <test-name>
```

Notes:

- Some tests are feature-gated behind `--features browser_ui` (the `browser` binary and the
  UI↔worker protocol types are `browser_ui`-only). If a test needs to spawn `browser` or drive the
  headless worker loop via `UiToWorker`/`WorkerToUi`, run with `--features browser_ui`.

## Headless constraints (no winit/wgpu/egui)

These tests must remain runnable on headless CI and developer machines without a display server.

Rules:

- **Do not** create a `winit` event loop/window.
- **Do not** initialise `wgpu` (e.g. `request_adapter`) or `egui` renderer state.
- Prefer testing headless components:
  - `BrowserDocument` behaviour (DOM mutation → re-render, scroll state, referrer propagation, …)
  - The headless UI worker loop and message routing (`UiToWorker`/`WorkerToUi`, tab scoping, stage
    forwarding, …). See the `spawn_ui_worker(...)` helper used throughout the `ui_worker_*` tests.
- Prefer `file://` fixtures and `tempdir()`-backed assets over network fetches to keep tests
  deterministic and fast.

## Spawning the `browser` binary headlessly

If a test needs to ensure the `browser` binary starts up (e.g. env parsing / early hooks), it must
do so without opening a window.

Supported test hooks:

- `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1` — exits before creating a window or initialising wgpu.
- `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` — runs a minimal end-to-end headless smoke test (UI↔worker
  wiring) without creating a window or initialising winit/wgpu. On success it prints a
  `HEADLESS_SMOKE_OK` marker to stdout and exits.

Example:

```bash
FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1 \
  scripts/cargo_agent.sh run --features browser_ui --bin browser
```

Headless-smoke example:

```bash
FASTR_TEST_BROWSER_HEADLESS_SMOKE=1 \
  scripts/cargo_agent.sh run --features browser_ui --bin browser
```

## Shared test helpers

New tests should prefer the shared helpers in `tests/browser_integration/support.rs` rather than
re-implementing common patterns.

It provides (among other things):

- Consistent timeout helpers for channel receives (`recv_until`, `drain_for`, `DEFAULT_TIMEOUT`).
- `TempSite` for creating temporary `file://` fixtures and getting correct `file://` URLs.
- Pixmap sampling helpers (`rgba_at`) for rendering assertions.
- `WorkerToUi` debug formatting (`format_messages`) for clearer assertion failures.

## Global stage listener locking

Stage heartbeats (`WorkerToUi::Stage`) are delivered via a process-global stage listener, so tests
that rely on them must not run concurrently within the same integration test binary.

If your test expects stage heartbeats (or registers a stage listener), acquire the global lock for
the duration of the test:

```rust
let _lock = crate::browser_integration::stage_listener_test_lock();
```

## Timeouts and cleanup (avoid hangs)

Rust tests have no global timeout by default. A hung test will stall CI indefinitely.

Best practices:

- Use `recv_timeout()` / explicit `Duration` deadlines when waiting on channels.
- Join spawned threads (or provide a shutdown signal + join) before returning from the test.
- Ensure global listeners/singletons are unregistered/reset on completion (or scope them to the job)
  to avoid cross-test interference.

## Existing / expected modules

Existing modules:

- `document.rs`: `BrowserDocument` behavioural tests (mutation, scroll state, referrers).
- `document2.rs`: `BrowserDocument2` behavioural tests (DOM mutation → rerender).
- `browser_mem_limit_env.rs` (`browser_ui`, linux): exercises `FASTR_BROWSER_MEM_LIMIT_MB` parsing in
  `src/bin/browser.rs` via the `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1` hook.
- `browser_binary_headless_smoke.rs` (`browser_ui`, linux): spawns `browser` in
  `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` mode and asserts the `HEADLESS_SMOKE_OK` marker is printed.
  (Task 76)
- `ui_render_worker_thread_builder_test.rs` (`browser_ui`): asserts the UI render worker thread is
  spawned via `std::thread::Builder` (name + large stack size).
- `ui_worker_*` (`browser_ui`): headless UI↔worker protocol tests using `spawn_ui_worker(...)`.
- `ui_stage_heartbeat_forwarding.rs` (`browser_ui`): validates stage heartbeat forwarding and cleanup
  is tab-scoped (requires the global stage listener lock).

Expected future additions (keep headless):

- Browser startup smoke tests (feature-gated) using `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1`.
- Expand worker protocol coverage for tabs/history/navigation state.
