# macOS entitlements (future `.app` bundling)

This directory contains **placeholder** entitlements intended for the day FastRender ships as a
macOS `.app` bundle with separate executables (trusted browser/UI + sandboxed renderer helper).

- `browser.entitlements`
  - Trusted browser/UI process.
  - Intentionally **does not** enable App Sandbox in the first packaging iteration.
- `renderer.entitlements`
  - Untrusted renderer helper process.
  - Enables App Sandbox (`com.apple.security.app-sandbox`) with a deny-by-default posture
    (no `com.apple.security.network.*` or `com.apple.security.files.*` entitlements).

See [`docs/security/macos_renderer_sandbox.md`](../../../docs/security/macos_renderer_sandbox.md)
(“Seatbelt now, App Sandbox later”) for how these would be applied via `codesign` and why they are
not used in dev builds today.
