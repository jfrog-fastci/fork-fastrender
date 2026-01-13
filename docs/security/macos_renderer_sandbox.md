# macOS renderer sandboxing (Seatbelt now, App Sandbox later)

FastRender’s long-term multiprocess model assumes **renderer processes are untrusted** and must run
inside an OS sandbox.

On macOS we rely on two related mechanisms:

- **Seatbelt** (what we use today for dev/CI and unsigned binaries): `sandbox_init(3)` /
  `/usr/bin/sandbox-exec` (**deprecated by Apple**; debug/legacy spawn wrapper only)
- **App Sandbox** (future `.app` distribution): entitlement-based sandbox enforced by codesigning

Canonical macOS sandbox guide (more detail + rationale): [`docs/macos_sandbox.md`](../macos_sandbox.md).

Renderer sandbox entrypoint doc (links to all platforms): [`docs/renderer_sandbox.md`](../renderer_sandbox.md).

Key code entrypoints:

- Seatbelt profiles + `sandbox_init` wrappers: [`src/sandbox/macos.rs`](../../src/sandbox/macos.rs)
- `/usr/bin/sandbox-exec` spawn helpers (debug/legacy; deprecated by Apple; opt-in via
  `FASTR_MACOS_USE_SANDBOX_EXEC=1`): [`src/sandbox/macos_spawn.rs`](../../src/sandbox/macos_spawn.rs)

---

## Seatbelt basics (`sandbox_init` / `sandbox-exec`)

On macOS, the kernel sandbox system is commonly referred to as **Seatbelt**. It can be applied:

- in-process (`sandbox_init(3)`), or
- at spawn time (`/usr/bin/sandbox-exec`).

Why Seatbelt is the current baseline:

- It does **not** require App Sandbox entitlements.
- It does **not** require codesigning.
- It works for unsigned local builds and CI.

---

## Default renderer profile (Seatbelt)

FastRender’s strict baseline is macOS’s `pure-computation` Seatbelt profile.

Implementation detail: FastRender applies a small SBPL wrapper that **imports**
`pure-computation.sb` and layers extra defense-in-depth denies (for example, blocking `mach-lookup`
to `com.apple.nsurlsessiond` so “no network” cannot be bypassed via system XPC daemons).

Repo reality detail: some macOS versions do not ship `pure-computation.sb` (or reject it as invalid),
so FastRender falls back to an embedded SBPL profile string. See:

- `src/sandbox/macos.rs` (`apply_strict_sandbox` / `PURE_COMPUTATION_HARDENED_PROFILE` / `STRICT_FALLBACK_PROFILE`)

There is also a renderer-friendly bring-up mode:

- `MacosSandboxMode::RendererSystemFonts` (blocks network + user filesystem reads, allows limited
  read-only access to system font/framework paths)

---

## Denied vs allowed surface (high level)

Under `pure-computation` the renderer should be treated as:

- **No network** (no socket create/bind/connect, even localhost).
- **No filesystem** (no reads/writes, including system fonts).
- **No process spawning** (`exec` should fail).

Even strict sandboxes should still allow:

- writes to inherited `stdout`/`stderr` (useful for crash/debug logs),
- anonymous IPC primitives that do not require filesystem writes (`pipe()`, `socketpair()`), and
- mapping inherited shared-memory file descriptors (even if `shm_open` is denied post-sandbox).

For an exact IPC/shared-memory capability matrix, use `macos_sandbox_probe` (see below) or consult
[`docs/macos_sandbox.md`](../macos_sandbox.md).

---

## Developer overrides (debug only)

The renderer sandbox is a security boundary. These knobs exist to unblock local debugging and
bring-up; do not rely on them in production.

### Configure renderer sandbox mode (recommended)

Set:

- `FASTR_RENDERER_SANDBOX=strict|relaxed|off`
  - Backwards-compatible spellings: `0` = `off`, `1` = `strict`.

Notes:

- On macOS, the default (when unset) is `strict` for renderer processes.
- `strict` maps to Seatbelt `pure-computation` (FastRender uses a hardened wrapper profile with a
  fallback embedded SBPL profile for older macOS versions).
- `relaxed` maps to the renderer-friendly “system fonts” profile (still blocks network).
- `off` disables sandboxing (debugging only; insecure).
- Legacy aliases: `1` = `strict`, `0` = `off`.

Advanced override:

- `FASTR_RENDERER_MACOS_SEATBELT_PROFILE=pure-computation|no-internet|renderer-default|<path>` can be
  used to override the underlying Seatbelt profile when sandboxing is enabled. This is intended for
  experimentation; it overrides the `strict`/`relaxed` profile mapping.

### Disable the Seatbelt renderer sandbox

Set:

- For multiprocess renderer sandbox entrypoints in `src/sandbox/mod.rs`:
  - `FASTR_RENDERER_SANDBOX=off` (preferred)
- For the low-level macOS Seatbelt wrappers in `src/sandbox/macos.rs` (and the `sandbox-exec` spawn
  helpers in `src/sandbox/macos_spawn.rs`):
  - `FASTR_DISABLE_RENDERER_SANDBOX=1`, or
  - `FASTR_MACOS_RENDERER_SANDBOX=off` (legacy alias).

On macOS this makes the in-process Seatbelt entrypoints in `src/sandbox/macos.rs`
(`apply_strict_sandbox`, `apply_pure_computation_sandbox`, `apply_renderer_sandbox`) return
`Ok(MacosSandboxStatus::Disabled)` without calling `sandbox_init`.

It also disables the spawn-time `sandbox-exec` helpers in `src/sandbox/macos_spawn.rs`
(`maybe_wrap_command_with_sandbox_exec`, `wrap_command_with_sandbox_exec`, `sandbox_exec_command`),
so the child is launched **without** `/usr/bin/sandbox-exec` even if `FASTR_MACOS_USE_SANDBOX_EXEC=1`
is set.

FastRender prints a warning to stderr once per process so insecure runs are not silent.

### Select a different renderer profile

Set:

- Prefer `FASTR_RENDERER_SANDBOX=strict|relaxed|off` for the multiprocess renderer sandbox entrypoints
  in `src/sandbox/mod.rs`.
- `FASTR_MACOS_RENDERER_SANDBOX=pure-computation|system-fonts|off` remains as a legacy macOS-only
  alias for the low-level Seatbelt wrappers in `src/sandbox/macos.rs` (ignored when
  `FASTR_RENDERER_SANDBOX` is explicitly set).

Notes:

- `pure-computation` is the strict default (aliases: `pure`, `strict`).
- `system-fonts` maps to `MacosSandboxMode::RendererSystemFonts` and is intended for temporary
  bring-up when bundled fonts are not yet viable (aliases: `fonts`, `relaxed`).
- `off` disables sandboxing (equivalent to `FASTR_DISABLE_RENDERER_SANDBOX=1`).

### `sandbox-exec` wrapper gate (debug/legacy)

Some tests and tooling can opt into launching a renderer already sandboxed using Apple’s deprecated
`/usr/bin/sandbox-exec` wrapper (see `src/sandbox/macos_spawn.rs`). This path is gated by:

- `FASTR_MACOS_USE_SANDBOX_EXEC=1`

Note: when sandboxing is disabled via `FASTR_DISABLE_RENDERER_SANDBOX=1` or
`FASTR_RENDERER_SANDBOX=off` or `FASTR_MACOS_RENDERER_SANDBOX=off`, the `sandbox-exec` wrappers become
no-ops.

---

## Example debug runs (sandboxed)

Render smoke test under the strict Seatbelt sandbox (`pure-computation`):

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration sandbox::macos_seatbelt_render_smoke -- --nocapture
```

Force the relaxed "system fonts" profile (useful for bring-up):

```bash
FASTR_RENDERER_SANDBOX=relaxed \
  timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration sandbox::macos_seatbelt_render_smoke -- --nocapture
```

Disable the renderer sandbox entirely (debug escape hatch; insecure):

```bash
FASTR_RENDERER_SANDBOX=off \
  timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration sandbox::macos_seatbelt_render_smoke -- --nocapture
```

Probe IPC allowances under a minimal SBPL-based renderer profile:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh run --bin macos_renderer_sandbox_ipc_probe -- pipes-only pipe-stdio
```

---

## Debugging Seatbelt denials

Sandbox denials typically surface as `EPERM` / `Operation not permitted`. The authoritative signal
is the macOS unified log (subsystem `com.apple.sandbox`).

```bash
# Replace <renderer-binary> with the renderer process name.
log stream --style syslog --level debug --predicate \
  'subsystem == "com.apple.sandbox" && process == "<renderer-binary>"'
```

To filter to denies:

```bash
log stream --style syslog --level debug --predicate \
  'subsystem == "com.apple.sandbox" && eventMessage CONTAINS[c] "deny"'
```

## When sandbox activation fails (`sandbox_init` errorbuf)

When Seatbelt fails to install a profile, `sandbox_init(3)` returns non-zero and (optionally) fills
an `errorbuf` C string describing the failure. FastRender propagates this string in the returned
`io::Error` (see `src/sandbox/macos.rs`, `sandbox_message(...)`).

Common cases:

- **Named profile missing/invalid**: messages containing “unknown profile” / “profile not found” /
  “invalid profile”.
  - FastRender treats this as “`pure-computation` is unavailable” and retries using the embedded
    SBPL fallback profile (`STRICT_FALLBACK_PROFILE`).
- **SBPL parse/validation errors**: syntax errors typically include a line/column or point at the
  unexpected token. This usually indicates a bug in an embedded/custom profile string.
- **Sandbox already applied**: calling `sandbox_init` more than once in the same process is not
  supported; errors here typically mean the process is already sandboxed.

---

## Tooling + CI expectations

Probe tool (macOS-only; built behind the non-default `macos_sandbox_probe` cargo feature):

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh run --features macos_sandbox_probe --bin macos_sandbox_probe -- --mode strict
```

macOS sandbox unit tests:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender sandbox::macos -- --nocapture
```

CI: GitHub Actions runs tests on **`macos-latest`** (see
[`./.github/workflows/ci.yml`](../../.github/workflows/ci.yml)). Seatbelt tests apply the sandbox
in a dedicated child process because it is irreversible per-process (`src/sandbox/macos.rs`).

---

## Future `.app` direction: App Sandbox entitlements

When FastRender ships as a macOS `.app`, we want the **untrusted renderer helper process** (web
content) to run with **App Sandbox** enabled, with a deny-by-default posture:

- no direct network access
- no filesystem entitlements granting access to arbitrary user/system paths (no
  `com.apple.security.files.*` entitlements)
- all OS I/O brokered by the trusted browser/UI process (or a dedicated network process) over IPC

This repository includes placeholder entitlement files for that future packaging step:

- [`tools/macos/entitlements/browser.entitlements`](../../tools/macos/entitlements/browser.entitlements)
  - Intended for the trusted browser/UI process.
  - Does **not** enable `com.apple.security.app-sandbox` for the initial `.app` iteration.
- [`tools/macos/entitlements/renderer.entitlements`](../../tools/macos/entitlements/renderer.entitlements)
  - Intended for the untrusted renderer helper process.
  - Enables `com.apple.security.app-sandbox`.
  - Intentionally does **not** request network or file entitlements.
  - Reminder: App Sandbox entitlements are *additive* grants; “denied” is achieved by leaving
    entitlements out (not by writing explicit deny rules).

Directory notes + a small validation helper live alongside these files:
[`tools/macos/entitlements/README.md`](../../tools/macos/entitlements/README.md).

Quick sanity check (optional):

```bash
python3 tools/macos/entitlements/validate_entitlements.py
```

Note: a sandboxed process can still read its own **app bundle resources** and can typically read/write
within its **App Sandbox container**; the goal here is to prevent the renderer from directly accessing
arbitrary user/system paths (and to keep network I/O brokered by a privileged process).

### How these would be used (future `.app` bundling)

On macOS, App Sandbox is enforced via entitlements embedded in the **code signature**.

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

### Why we can’t rely on App Sandbox for dev/CI builds

App Sandbox requires **codesigning** and `.app`-style packaging. Typical development runs (and many
CI harnesses) execute unsigned binaries (for example via `bash scripts/cargo_agent.sh build` /
`bash scripts/cargo_agent.sh run`), so entitlements are not available and App Sandbox is not applied.

Additionally, FastRender is not yet shipped as a real `.app` bundle with a separate renderer helper
executable to sign; the in-tree desktop `browser` binary still runs the “renderer” on a worker thread.
For developer/CI sandbox iteration we therefore rely on Seatbelt (`sandbox_init` / `sandbox-exec`),
which works for unsigned binaries and can be applied independently of packaging.

Even after `.app` distribution exists, we still expect to use Seatbelt as a fine-grained,
per-process sandbox layer for renderer isolation, because it works for unsigned binaries and can be
tailored more narrowly than coarse app-level entitlements.

---

## Renderer IPC mechanism allowances (Seatbelt SBPL)

The multiprocess renderer↔browser IPC transport choice affects which macOS Seatbelt operations must
be allowed. FastRender keeps IPC-related allowances behind a small enum so future IPC choices require
minimal SBPL churn.

Code: `src/security/macos_renderer_sandbox.rs`

```rust
use fastrender::security::macos_renderer_sandbox::{build_renderer_sbpl, RendererIpcMechanism};

let sbpl = build_renderer_sbpl(RendererIpcMechanism::PipesOnly);
```

### `PipesOnly`

**Primitive:** anonymous pipes / inherited file descriptors.

**Seatbelt:** typically no dedicated IPC-specific operation is required as long as the browser
creates the FDs before sandboxing.

### `PosixShm`

**Primitive:** POSIX shared memory (`shm_open`, `shm_unlink`) for large buffers.

**Seatbelt:** allow `ipc-posix-shm`.

### `UnixSocket`

**Primitive:** filesystem-path Unix domain sockets (`AF_UNIX`, `sockaddr_un`).

**Seatbelt:** allow outbound connects:

- `network-outbound (remote unix-socket)`

### `MachPort`

**Primitive:** Mach ports / bootstrap services (likely for `ipc-channel`-style transport).

**Seatbelt:** allow `mach-lookup` (ideally scoped to an allowlist of service names).

### Tests

macOS-only regression tests for these toggles live in:

- `tests/security/macos_renderer_sandbox_ipc.rs`

They spawn a dedicated probe process (`src/bin/macos_renderer_sandbox_ipc_probe.rs`) because applying
Seatbelt via `sandbox_init` is irreversible.
