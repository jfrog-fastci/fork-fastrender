# macOS renderer sandbox (App Sandbox entitlements)

When FastRender eventually ships as a macOS `.app`, we want the **renderer
helper process** (untrusted web content) to run with **App Sandbox** enabled,
with a deny-by-default posture:

- no direct network access
- no direct filesystem access
- all OS I/O brokered by the trusted browser/UI process (or a dedicated network
  process) over IPC

This repository includes **placeholder entitlement files** for that future
packaging step:

- `tools/macos/entitlements/browser.entitlements`
  - Intended for the trusted browser/UI process.
  - **Does not enable** `com.apple.security.app-sandbox` (i.e. not sandboxed in
    the first `.app` iteration).
- `tools/macos/entitlements/renderer.entitlements`
  - Intended for the untrusted renderer helper process.
  - Enables `com.apple.security.app-sandbox`.
  - Intentionally does **not** request network or file entitlements.

## How these would be used (future `.app` bundling)

On macOS, App Sandbox is enforced via **entitlements embedded in the code
signature**. When we have a real `.app` bundle layout with separate executables,
the build/packaging step would sign each executable with the appropriate
entitlements, e.g.:

```bash
# Example paths only — the real bundle layout may differ.
codesign --force --sign "<identity>" \
  --entitlements tools/macos/entitlements/browser.entitlements \
  FastRender.app/Contents/MacOS/browser

codesign --force --sign "<identity>" \
  --entitlements tools/macos/entitlements/renderer.entitlements \
  FastRender.app/Contents/MacOS/renderer

# Useful for verification/debugging:
codesign -d --entitlements :- FastRender.app/Contents/MacOS/renderer
```

## Why this is not active in dev builds yet

Today, local development typically runs binaries produced by `cargo build` /
`cargo run`, which are **not code-signed as `.app` bundle executables**.
Without a signature embedding the entitlements, App Sandbox is not applied.

Additionally, the current codebase is not yet structured so that the renderer
does *zero* network/file I/O; turning on App Sandbox prematurely would break
common development workflows (and would complicate debugging tools that rely on
broader process permissions).

These files are therefore **preparatory**: they exist so that when `.app`
packaging lands, we have a reviewed, deny-by-default entitlements baseline ready
to wire into the signing step.

