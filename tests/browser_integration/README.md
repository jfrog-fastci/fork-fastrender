# Browser integration tests (headless + feature-gated)

This directory contains integration tests that exercise *browser-adjacent* code paths (document
rendering, UI worker protocol plumbing, and the `browser` binary startup hooks) **without requiring a
real window or GPU**.

The tests here are **not** a standalone Cargo integration-test target. They are Rust modules under
`tests/browser_integration/` that are included by the main integration test harness:

- Harness entrypoint: `tests/integration.rs` (includes this module via `mod browser_integration;`)
- Module aggregator: `tests/browser_integration/mod.rs`

This keeps test compilation/linking fast and avoids reintroducing per-category `tests/*.rs` test
binaries.

## Adding new tests (required structure)

**Do not add new top-level `tests/*.rs` files** for browser integration coverage. Each top-level
file becomes a separate Cargo integration-test binary and is expensive to build/link.

Instead:

1. Add a new Rust module under `tests/browser_integration/` (for example:
   `tests/browser_integration/worker_protocol_smoke.rs`).
2. Register it in `tests/browser_integration/mod.rs` with `mod worker_protocol_smoke;`.
3. Ensure `tests/integration.rs` includes `mod browser_integration;` (it should already).

## Running safely (scoped + feature-gated)

Per `AGENTS.md`, **never** run unscoped `cargo test`. Always use the wrapper and scope to a single
test target.

Run the default (headless) subset:

```bash
bash scripts/cargo_agent.sh test -p fastrender --test integration browser_integration::
```

Run tests that are gated behind the UI feature (optionally filtered to a specific test name):

```bash
bash scripts/cargo_agent.sh test -p fastrender --test integration --features browser_ui browser_integration::<test-name>
```

Notes:

- Some tests are feature-gated behind `--features browser_ui` (the `browser` binary and the
  UI↔worker protocol types are `browser_ui`-only). If a test needs to spawn `browser` or drive the
  headless worker loop via `UiToWorker`/`WorkerToUi`, run with `--features browser_ui`.
- Rust's test harness runs tests in parallel threads by default. Many browser integration tests
  share process-global state. To avoid cross-test interference while still allowing the overall
  integration test suite to run with default libtest parallelism, tests that touch shared global
  state should acquire `stage_listener_test_lock()` (see below) or the shared integration-test lock
  in `tests/common/global_state.rs` (`crate::common::global_test_lock()`).
- If you need fully single-threaded execution (e.g. when debugging a flaky global-state
  interaction), run with:

  ```bash
  bash scripts/cargo_agent.sh test -p fastrender --test integration browser_integration:: -- --test-threads 1
  ```
- Browser integration tests should be hermetic with respect to fonts: prefer
  `support::deterministic_renderer()` / `support::deterministic_factory()` (or pass an explicit
  `FontConfig`, such as `FontConfig::bundled_only()`) so text rendering does not depend on
  system-installed fonts. Under `--features browser_ui`, `FontConfig::default()` already prefers
  bundled fonts by default, so the test harness does not need process-wide env var overrides.

## Headless constraints (no winit/wgpu/egui)

These tests must remain runnable on headless CI and developer machines without a display server.

Rules:

- **Do not** create a `winit` event loop/window.
- **Do not** initialise `wgpu` (e.g. `request_adapter`) or `egui` renderer state.
- Prefer testing headless components:
  - `BrowserDocument` behaviour (DOM mutation → re-render, scroll state, referrer propagation, …)
  - The headless UI worker loop and message routing (`UiToWorker`/`WorkerToUi`, tab scoping, stage
    forwarding, …). See the `spawn_ui_worker(...)` helper used throughout the `ui_worker_*` tests.
    `spawn_test_browser_worker(...)` is a convenience wrapper that returns a `BrowserWorkerHandle`.
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
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser
```

Headless-smoke example:

```bash
FASTR_TEST_BROWSER_HEADLESS_SMOKE=1 \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser
```

## Shared test helpers

New tests should prefer the shared helpers in `tests/browser_integration/support.rs` rather than
re-implementing common patterns.

It provides (among other things):

- Consistent timeout helpers for channel receives (`recv_until`, `drain_for`, `DEFAULT_TIMEOUT`).
- `TempSite` for creating temporary `file://` fixtures and getting correct `file://` URLs.
- Deterministic renderer/factory helpers (`deterministic_renderer`, `deterministic_factory`) so tests
  do not depend on system-installed fonts.
- Pixmap sampling helpers (`rgba_at`) for rendering assertions.
- `WorkerToUi` debug formatting (`format_messages`) for clearer assertion failures.

## Global integration test lock

Some browser integration tests use process-global test hooks (for example
`render_control::set_test_render_delay_ms`) and other shared state. To avoid cross-test
interference, acquire the global lock for the duration of the test:

```rust
let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
```

## Test render delays (cancellation determinism)

Some cancellation/timeout tests need to slow down render stages to make races deterministic.

Prefer the scoped programmatic delay helpers over mutating the process environment variable:

- `spawn_ui_worker_for_test(..., Some(ms))`
- `spawn_browser_worker_for_test(Some(ms))`

These helpers use `render_control::set_test_render_delay_ms`, which is **process-global**. Keep the
delay scoped to the worker lifetime and serialize these tests with `stage_listener_test_lock` to
avoid leaking the setting across unrelated tests.

Avoid mutating the legacy render-delay environment variable from within these integration tests:
these tests run in-process inside the shared `integration` test binary, so changing a process env
var can slow down unrelated tests running in the same process and cause flakiness under parallel
execution.

## Timeouts and cleanup (avoid hangs)

Rust tests have no global timeout by default. A hung test will stall CI indefinitely.

Best practices:

- Use `recv_timeout()` / explicit `Duration` deadlines when waiting on channels.
- Join spawned threads (or provide a shutdown signal + join) before returning from the test.
- Ensure global listeners/singletons are unregistered/reset on completion (or scope them to the job)
  to avoid cross-test interference.

## ScrollStateUpdated vs FrameReady (ordering + presence)

The UI worker can report scroll state in two places:

- `WorkerToUi::ScrollStateUpdated { tab_id, scroll }` — may be sent *immediately* on scroll input so
  UIs can async-scroll the last painted texture while waiting for repaint.
- `WorkerToUi::FrameReady { frame }` — always carries the canonical *painted* scroll state as
  `frame.scroll_state`.

Important implications for tests:

- Do **not** assume ordering between `ScrollStateUpdated` and `FrameReady` (either can arrive first).
- Do **not** assume a standalone `ScrollStateUpdated` is always emitted:
  - navigation-driven scroll changes (e.g. same-document fragment jumps) may only be observable via
    the next `FrameReady`,
  - clamped/no-op scroll requests may schedule a repaint without changing scroll state (so no scroll
    update is sent).
- If a test needs to associate a `ScrollStateUpdated` with the corresponding painted frame, prefer
  `support::wait_for_frame_and_scroll_state_updated(...)` (pairs by viewport + element scroll
  offsets).

## Existing / expected modules

Existing modules:

- `document.rs`: `BrowserDocument` behavioural tests (mutation, scroll state, referrers).
- `document2.rs`: `BrowserDocument2` behavioural tests (DOM mutation → rerender).
- `browser_mem_limit_env.rs` (`browser_ui`, linux): exercises `FASTR_BROWSER_MEM_LIMIT_MB` parsing in
  `src/bin/browser.rs` via the `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1` hook.
- `browser_headless_smoke_test.rs` (`browser_ui`, linux): spawns `browser` in
  `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` mode and asserts the `HEADLESS_SMOKE_OK` marker is printed.
- `ui_render_worker_thread_builder_test.rs` (`browser_ui`): asserts the UI render worker thread is
  spawned via `std::thread::Builder` (name + large stack size).
- `ui_worker_*` (`browser_ui`): headless UI↔worker protocol tests (prefer `spawn_browser_worker()`).
- `ui_stage_heartbeat_forwarding.rs` (`browser_ui`): validates stage heartbeat forwarding and cleanup
  is tab-scoped.

Expected future additions (keep headless):

- Browser startup smoke tests (feature-gated) using `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1`.
- Expand worker protocol coverage for tabs/history/navigation state.
