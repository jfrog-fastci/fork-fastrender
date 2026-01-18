# JavaScript Web APIs workstream (`js_web_apis`)

This workstream owns **Web APIs exposed to JavaScript that are not “DOM proper”** (Fetch, URL, timers, storage, crypto, etc.).

## Owns

- Web API implementations under `src/js/` and `src/web/`
- Integration with the resource loader/networking stack for `fetch()`
- Timer APIs (`setTimeout`, `setInterval`, `queueMicrotask` helpers, etc.)

## Does NOT own

- The JS engine core (`instructions/js_engine.md`)
- DOM/Web IDL bindings surface (`instructions/js_dom.md`)
- HTML `<script>` scheduling / event loop integration (`instructions/js_html_integration.md`)

## Invariants / constraints

- APIs must be bounded (CPU + memory), especially for hostile content.
- Prefer deterministic, offline-capable tests (fixtures) over live network dependencies.

