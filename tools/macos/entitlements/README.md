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

## Validation helper (optional)

You can sanity-check that the entitlement files are valid XML plists (and that the renderer
entitlements remain deny-by-default) with:

```bash
python tools/macos/entitlements/validate_entitlements.py
```

## Editing notes

These `*.entitlements` files are XML plists. If you add or edit comments inside them, remember that
XML comments cannot contain a **double-hyphen** sequence (two consecutive `-` characters), or the
file becomes invalid for strict plist/XML parsers.
