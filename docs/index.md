# FastRender internal docs (wiki)

This `docs/` directory is the canonical “wiki” for this repository. It is internal-only and should reflect **repo reality** (current code, current tools, current behavior), not historical plans or abandoned file layouts.

If a document can’t be kept accurate, delete it and replace it with something smaller and true.

## Start here

- Common agent rules & resource safety: [`AGENTS.md`](../AGENTS.md)
- Capability buildout workstream: [`instructions/capability_buildout.md`](../instructions/capability_buildout.md)
- Pageset-by-page fixing loop: [`instructions/pageset_page_loop.md`](../instructions/pageset_page_loop.md)
- Browser UI / chrome work: [`instructions/browser_ui.md`](../instructions/browser_ui.md)
- Running the desktop browser UI locally: [browser_ui.md](browser_ui.md)
- JavaScript support workstream: [`instructions/javascript_support.md`](../instructions/javascript_support.md)
- `ecma-rs` submodule workflow: [`instructions/ecma_rs.md`](../instructions/ecma_rs.md)
- JavaScript integration architecture: [javascript.md](javascript.md)
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
