# Docs

The canonical entry point for this repo’s internal documentation is [`docs/index.md`](index.md).

If you are looking for the current “which document/tab API runs JS + event loop?” map, start with:

- [`docs/runtime_stacks.md`](runtime_stacks.md)
- [`docs/live_rendering_loop.md`](live_rendering_loop.md) (driving a JS-enabled `BrowserTab` loop)

Browser UI development:

- Running + architecture overview: [`docs/browser_ui.md`](browser_ui.md)
- Desktop browser app overview: [`docs/browser.md`](browser.md)
- Manual chrome test matrix (quick parity checklist): [`docs/chrome_test_matrix.md`](chrome_test_matrix.md)
- Manual chrome regression checklist (full, end-to-end): [`docs/browser_chrome_manual_test_matrix.md`](browser_chrome_manual_test_matrix.md)
- Internal `about:` pages (offline UI + debugging surfaces): [`docs/about_pages.md`](about_pages.md)
- Page accessibility workflow (a11y tree + bounds mapping + screen reader testing): [`docs/page_accessibility.md`](page_accessibility.md)

Multiprocess architecture & security:

- Renderer IPC trust boundary: [`docs/multiprocess_threat_model.md`](multiprocess_threat_model.md)
- IPC transport invariants (framing + size caps + shared memory safety): [`docs/ipc.md`](ipc.md)
- Linux IPC checklist (shared memory + FD passing): [`docs/ipc_linux_fd_passing.md`](ipc_linux_fd_passing.md)
- Renderer sandbox entrypoint (links to all platform docs): [`docs/renderer_sandbox.md`](renderer_sandbox.md)
- Sandboxing overview (renderer process): [`docs/sandboxing.md`](sandboxing.md)
- Linux renderer sandbox deep dive (rlimits/fd hygiene/namespaces/Landlock/seccomp): [`docs/security/sandbox.md`](security/sandbox.md)
- Windows renderer sandboxing (Job objects + AppContainer + restricted token): [`docs/windows_sandbox.md`](windows_sandbox.md)
- macOS Seatbelt sandboxing (overview + probe tool): [`docs/macos_sandbox.md`](macos_sandbox.md)
- macOS renderer sandboxing (Seatbelt now, App Sandbox later): [`docs/security/macos_renderer_sandbox.md`](security/macos_renderer_sandbox.md)
- Site isolation process model (per-origin + OOPIF): [`docs/site_isolation.md`](site_isolation.md)
- Chrome JS bridge (trusted UI pages): [`docs/chrome_js_bridge.md`](chrome_js_bridge.md)
- Renderer-chrome internal schemes (`chrome://` assets, `chrome-action:` actions): [`docs/renderer_chrome_schemes.md`](renderer_chrome_schemes.md)
- Chrome accessibility (AccessKit): [`docs/chrome_accessibility.md`](chrome_accessibility.md)
