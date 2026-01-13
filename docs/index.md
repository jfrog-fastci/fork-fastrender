# FastRender internal docs (wiki)

This `docs/` directory is the canonical “wiki” for this repository. It is internal-only and should reflect **repo reality** (current code, current tools, current behavior), not historical plans or abandoned file layouts.

If a document can’t be kept accurate, delete it and replace it with something smaller and true.

## Start here

- Common agent rules & resource safety: [`AGENTS.md`](../AGENTS.md)
- **Build performance** (why builds are slow, how to be fast): [build_performance.md](build_performance.md)
- **Philosophy & culture** (hard-won lessons, mindset): [philosophy.md](philosophy.md)
- **Triage & operations** (priority order, failure classification, operating model): [triage.md](triage.md)
- **Accuracy workflow** (how to fix rendering issues): [accuracy_workflow.md](accuracy_workflow.md)

## Core rendering engine workstreams

- Capability buildout (spec-first primitives): [`instructions/capability_buildout.md`](../instructions/capability_buildout.md)
- Pageset page loop (fix pages one-by-one): [`instructions/pageset_page_loop.md`](../instructions/pageset_page_loop.md)
- **Live rendering (dynamic browser, not static)**: [`instructions/live_rendering.md`](../instructions/live_rendering.md) ⭐
- Video support: [`instructions/video_support.md`](../instructions/video_support.md)
- Media clocking & A/V sync model (audio master clock): [media_clocking.md](media_clocking.md)
- Page-loop tooling playbook: [page_loop_tooling.md](page_loop_tooling.md)

## Browser application workstreams

- Browser chrome (tabs, navigation, address bar): [`instructions/browser_chrome.md`](../instructions/browser_chrome.md)
- Browser responsiveness (performance): [`instructions/browser_responsiveness.md`](../instructions/browser_responsiveness.md)
- Browser page interaction (forms, focus): [`instructions/browser_interaction.md`](../instructions/browser_interaction.md)
- Desktop browser app (`browser` binary): [browser.md](browser.md)
- Running the desktop browser UI locally: [browser_ui.md](browser_ui.md)
- Manual chrome test matrix (quick): [chrome_test_matrix.md](chrome_test_matrix.md)
- Manual chrome regression checklist (full): [browser_chrome_manual_test_matrix.md](browser_chrome_manual_test_matrix.md)
- Internal `about:` pages (debugging surfaces like `about:gpu` / `about:processes`): [about_pages.md](about_pages.md)
- Chrome accessibility (AccessKit + debugging): [chrome_accessibility.md](chrome_accessibility.md)
- Page accessibility (a11y tree, bounds, screen reader integration): [page_accessibility.md](page_accessibility.md)

## Architecture & security workstreams

- Multiprocess & security: [`instructions/multiprocess_security.md`](../instructions/multiprocess_security.md)
- Multiprocess threat model (renderer IPC trust boundary): [multiprocess_threat_model.md](multiprocess_threat_model.md)
- Site isolation process model (per-origin, OOPIF; `file://` semantics): [site_isolation.md](site_isolation.md)
- Linux IPC (shared memory + FD passing checklist): [ipc_linux_fd_passing.md](ipc_linux_fd_passing.md)
- Renderer sandbox entrypoint: [renderer_sandbox.md](renderer_sandbox.md)
- Sandboxing overview (renderer process): [sandboxing.md](sandboxing.md)
- Linux sandbox design (rlimits, fd hygiene, namespaces, Landlock + seccomp): [security/sandbox.md](security/sandbox.md)
- Windows renderer sandboxing: [windows_sandbox.md](windows_sandbox.md)
- Windows renderer sandbox quick reference: [security/windows_renderer_sandbox.md](security/windows_renderer_sandbox.md)
- Linux seccomp allowlist maintenance: [seccomp_allowlist.md](seccomp_allowlist.md)
- macOS Seatbelt sandboxing (overview + probe tool): [macos_sandbox.md](macos_sandbox.md)
- macOS renderer sandboxing (Seatbelt now, App Sandbox later): [security/macos_renderer_sandbox.md](security/macos_renderer_sandbox.md)
- IPC transport invariants (framing, `SCM_RIGHTS`, `memfd`): [ipc.md](ipc.md)
- Network process & IPC surface: [network_process.md](network_process.md)
- IPC protocol: shared-memory frame transport (framing + buffer lifecycle): [ipc_frame_transport.md](ipc_frame_transport.md)
- Renderer chrome (future): [`instructions/renderer_chrome.md`](../instructions/renderer_chrome.md)
- Renderer-chrome internal schemes (`chrome://` assets, `chrome-action:` actions): [renderer_chrome_schemes.md](renderer_chrome_schemes.md)
- Renderer-chrome without JS (`chrome-action:` roadmap): [renderer_chrome_non_js.md](renderer_chrome_non_js.md)
- Chrome JS bridge (trusted UI API surface): [chrome_js_bridge.md](chrome_js_bridge.md)

## JavaScript workstreams

- **ecma-rs ownership principle**: [ecma_rs_ownership.md](ecma_rs_ownership.md) — READ THIS FIRST
- JS engine (vm-js core): [`instructions/js_engine.md`](../instructions/js_engine.md)
- JS DOM bindings: [`instructions/js_dom.md`](../instructions/js_dom.md)
- JS Web APIs (fetch, timers, etc.): [`instructions/js_web_apis.md`](../instructions/js_web_apis.md)
- JS HTML integration (script loading, modules): [`instructions/js_html_integration.md`](../instructions/js_html_integration.md)
- WebIDL stack (crate layout + boundaries): [webidl_stack.md](webidl_stack.md)
- JavaScript integration architecture: [javascript.md](javascript.md)
- Runtime stacks (Document vs DOM2 vs Tab vs JS): [runtime_stacks.md](runtime_stacks.md)
- Driving a live `BrowserTab` loop (`tick_frame`, `run_until_stable`): [live_rendering_loop.md](live_rendering_loop.md)
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
- **Test architecture**: [test_architecture.md](test_architecture.md) — target: 2 integration test binaries
- Vendoring / dependency patches: [vendoring.md](vendoring.md)
- CSS loading & URL resolution: [css-loading.md](css-loading.md)

## Research & notes

- Research notes: [research/index.md](research/index.md)
- Durable internal notes: [notes/index.md](notes/index.md)

## Conventions

- Prefer linking to actual repo paths (e.g. `src/api.rs`) over describing hypothetical files.
- Avoid “phase/wave/task” planning docs in the wiki; keep planning outside the repo.
- Keep docs scoped: a small accurate doc beats a large drifting one.
