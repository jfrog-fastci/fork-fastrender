# Renderer sandboxing

This is a short entrypoint doc for FastRender’s renderer sandbox design.

Core contract (applies to all platforms):

- Treat the renderer as **untrusted** (it parses/executes attacker-controlled HTML/CSS/JS).
- The renderer must not have direct **filesystem** or **network** access; it should fetch resources
  via IPC (an IPC-backed `ResourceFetcher`) so the browser/network process mediates I/O.
- For renderer-process builds, prefer disabling `direct_*` features (see `docs/security/sandbox.md`).

Start here:

- Cross-platform overview and debug knobs: [`docs/sandboxing.md`](sandboxing.md)
- Linux deep dive (rlimits/fd hygiene/namespaces/Landlock/seccomp): [`docs/security/sandbox.md`](security/sandbox.md)
- Windows renderer sandbox quick reference: [`docs/security/windows_renderer_sandbox.md`](security/windows_renderer_sandbox.md)
- macOS renderer sandbox quick reference: [`docs/security/macos_renderer_sandbox.md`](security/macos_renderer_sandbox.md)

Environment variables / escape hatches: [`docs/env-vars.md`](env-vars.md) (`FASTR_DISABLE_RENDERER_SANDBOX`,
`FASTR_RENDERER_SECCOMP`, ...).
