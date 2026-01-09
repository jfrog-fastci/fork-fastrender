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

- The `browser` binary (`src/bin/browser.rs`) is itself `required-features = ["browser_ui"]`, so any
  tests that need to compile/spawn it must be `#[cfg(feature = "browser_ui")]` as well.

## Headless constraints (no winit/wgpu/egui)

These tests must remain runnable on headless CI and developer machines without a display server.

Rules:

- **Do not** create a `winit` event loop/window.
- **Do not** initialise `wgpu` (e.g. `request_adapter`) or `egui` renderer state.
- Prefer testing headless components:
  - `BrowserDocument` behaviour (DOM mutation → re-render, scroll state, referrer propagation, …)
  - `ui::worker::RenderWorker` and message routing (`WorkerToUi`, tab scoping, stage forwarding, …)
- Prefer `file://` fixtures and `tempdir()`-backed assets over network fetches to keep tests
  deterministic and fast.

## Spawning the `browser` binary headlessly

If a test needs to ensure the `browser` binary starts up (e.g. env parsing / early hooks), it must
do so without opening a window.

Supported test hooks:

- `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1` — exits before creating a window or initialising wgpu.
- `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` — reserved for a future headless smoke path (use only if/when
  implemented).

Example:

```bash
FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1 \
  scripts/cargo_agent.sh run --features browser_ui --bin browser
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
- `ui_render_worker_thread_builder_test.rs` (`browser_ui`): asserts UI worker threads are spawned via
  `std::thread::Builder`.
- `ui_stage_heartbeat_forwarding.rs` (`browser_ui`): validates stage heartbeat forwarding and cleanup
  is tab-scoped.

Expected future additions (keep headless):

- Browser startup smoke tests (feature-gated) using `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1`.
- Worker protocol smoke tests for tabs/history/navigation state.
- Scroll/input translation tests that exercise the UI worker loop without needing `winit`.
