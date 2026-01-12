# FastRender internal docs (wiki)

This `docs/` directory is the canonical “wiki” for this repository. It is internal-only and should reflect **repo reality** (current code, current tools, current behavior), not historical plans or abandoned file layouts.

If a document can’t be kept accurate, delete it and replace it with something smaller and true.

## Start here

- Common agent rules & resource safety: [`AGENTS.md`](../AGENTS.md)
- **Philosophy & culture** (hard-won lessons, mindset): [philosophy.md](philosophy.md)
- **Triage & operations** (priority order, failure classification, operating model): [triage.md](triage.md)
- **Accuracy workflow** (how to fix rendering issues): [accuracy_workflow.md](accuracy_workflow.md)

## Rendering engine workstreams

- Capability buildout (spec-first primitives): [`instructions/capability_buildout.md`](../instructions/capability_buildout.md)
- Pageset page loop (fix pages one-by-one): [`instructions/pageset_page_loop.md`](../instructions/pageset_page_loop.md)
- Page-loop tooling playbook: [page_loop_tooling.md](page_loop_tooling.md)

## Browser application workstreams

- Browser chrome (tabs, navigation, address bar): [`instructions/browser_chrome.md`](../instructions/browser_chrome.md)
- Browser UX & visual design: [`instructions/browser_ux.md`](../instructions/browser_ux.md)
- Browser page interaction (forms, focus): [`instructions/browser_interaction.md`](../instructions/browser_interaction.md)
- Desktop browser app (`browser` binary): [browser.md](browser.md)
- Running the desktop browser UI locally: [browser_ui.md](browser_ui.md)

## JavaScript workstreams

- JS engine (vm-js core): [`instructions/js_engine.md`](../instructions/js_engine.md)
- JS DOM bindings: [`instructions/js_dom.md`](../instructions/js_dom.md)
- JS Web APIs (fetch, timers, etc.): [`instructions/js_web_apis.md`](../instructions/js_web_apis.md)
- JS HTML integration (script loading, modules): [`instructions/js_html_integration.md`](../instructions/js_html_integration.md)
- JavaScript integration architecture: [javascript.md](javascript.md)
- LLVM StackMaps / statepoint metadata decoding: [llvm_stackmaps.md](llvm_stackmaps.md)
- LLVM 18 statepoint StackMap ABI assumptions (regression-tested): [llvm_statepoint_stackmap_abi.md](llvm_statepoint_stackmap_abi.md)
- HTML `<script>` processing model (spec-mapped): [html_script_processing.md](html_script_processing.md)
- Import maps (spec-mapped parsing + merge/register + resolution): [import_maps.md](import_maps.md)
- WebIDL bindings/codegen pipeline: [webidl_bindings.md](webidl_bindings.md)
- Running the renderer: [running.md](running.md)
- CLI tools & workflows: [cli.md](cli.md)
- Evidence loop (spec → code → regression): see `AGENTS.md` plus [testing.md](testing.md) (WPT harness) and [cli.md](cli.md) (fixture tooling).
- Library API: [api.md](api.md)
- Architecture overview: [architecture.md](architecture.md)
- Conformance targets & support matrix: [conformance.md](conformance.md)
- Debugging renders: [debugging.md](debugging.md)
- Profiling & perf logging: [perf-logging.md](perf-logging.md)
- Profiling on Linux (perf/samply/flamegraph): [profiling-linux.md](profiling-linux.md)
- Resource limits (RAM/CPU/time): [resource-limits.md](resource-limits.md)
- Runtime environment variables: [env-vars.md](env-vars.md)
- Instrumentation patterns: [instrumentation.md](instrumentation.md)
- Testing: [testing.md](testing.md)
- Vendoring / dependency patches: [vendoring.md](vendoring.md)
- CSS loading & URL resolution: [css-loading.md](css-loading.md)

## Research & notes

- Research notes: [research/index.md](research/index.md)
- Durable internal notes: [notes/index.md](notes/index.md)

## Conventions

- Prefer linking to actual repo paths (e.g. `src/api.rs`) over describing hypothetical files.
- Avoid “phase/wave/task” planning docs in the wiki; keep planning outside the repo.
- Keep docs scoped: a small accurate doc beats a large drifting one.
