# JavaScript DOM bindings workstream (`js_dom`)

This workstream owns the **DOM surface exposed to JavaScript**, including Web IDL bindings and DOM-event plumbing. The goal is to provide spec-shaped DOM APIs backed by FastRender’s DOM2 representation.

## Owns

- `src/js/webidl/` (FastRender-side bindings integration)
- `src/dom2/` (DOM2 data structures used by the JS-capable runtime stack)
- DOM API implementations exposed to JS (e.g. `document`, `Element`, `Node`, events)

## Does NOT own

- The ECMAScript engine itself (`vendor/ecma-rs/vm-js/`), see `instructions/js_engine.md`
- Non-DOM Web APIs like `fetch`, `URL`, timers, etc. (see `instructions/js_web_apis.md`)
- HTML script loading/scheduling/event loop semantics (see `instructions/js_html_integration.md`)

## Invariants / constraints

- Must be spec-shaped: partial is OK, incorrect is not.
- No process-global toggles in tests; configure behavior through `FastRenderConfig` / runtime toggles passed per instance.

