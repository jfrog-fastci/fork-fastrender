# JavaScript + HTML integration workstream (`js_html_integration`)

This workstream owns the **HTML-defined integration points for JavaScript**: `<script>` processing, module loading, task/microtask checkpoints, and event loop semantics as they relate to documents/tabs.

## Owns

- HTML script scheduling/pipeline in `src/js/html_script_scheduler.rs` and `src/js/html_script_pipeline.rs`
- `src/js/event_loop.rs` and microtask/task queue semantics
- Loader plumbing for classic/module scripts and import maps

## Does NOT own

- The JS engine itself (`instructions/js_engine.md`)
- DOM API surface (`instructions/js_dom.md`)
- Web APIs like fetch/timers/url (`instructions/js_web_apis.md`)

## Invariants / constraints

- Script execution must be bounded and cancellable.
- Ordering must follow spec points (parser-inserted vs dynamic, async/defer, microtask checkpoints).

