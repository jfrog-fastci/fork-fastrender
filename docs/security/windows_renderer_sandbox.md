# Windows renderer sandboxing (AppContainer + Job Objects)

FastRender’s long-term multiprocess model assumes **renderer processes are untrusted** and must run
inside a strong OS sandbox boundary.

Canonical sandboxing overview (all platforms): [`docs/sandboxing.md`](../sandboxing.md).

Key Windows code entrypoints:

- Windows sandbox spawn helper: [`src/sandbox/windows.rs`](../../src/sandbox/windows.rs)
  - `spawn_sandboxed(...)` (preferred)
  - `requested_renderer_sandbox_level()` (debug escape hatch parsing)

---

## Default Windows sandbox model

### Primary mode: AppContainer (zero capabilities)

When available, the renderer should run in a Windows **AppContainer** with **no capabilities**:

- **No network**: no `INTERNET_CLIENT` capability is granted.
- **Restricted filesystem**: the token is confined to AppContainer policy (no arbitrary user profile
  / system directory access).

AppContainer is the preferred sandbox because it provides a strong, OS-enforced isolation boundary.

### Defense in depth: Job Object guardrails

Regardless of token/AppContainer mode, the renderer is placed in a **Job Object** configured with:

- **Kill-on-close**: dropping the job handle (or parent process death) kills the renderer process
  tree.
- **Active-process limit**: caps the number of processes the renderer can create (blocks fork bombs
  and limits damage if the renderer is compromised).

These limits are intended as defense-in-depth and lifecycle hygiene.

### Fallback mode: restricted token + low integrity (+ job object)

If AppContainer is unavailable (or creation fails), the renderer falls back to spawning with:

- a **restricted token** (max privileges removed), and
- **low integrity**, still under the same Job Object constraints.

Limitations:

- **Network may still be available** in this mode (depending on what the restricted token removes and
  system policy). Do not treat it as equivalent to AppContainer.

### Last resort fallback: unsandboxed spawn (+ job object)

If both AppContainer and restricted-token spawning fail, `spawn_sandboxed(...)` falls back to an
unsandboxed spawn **still inside the Job Object**, and logs a warning to stderr. This is intended to
avoid silent “it didn’t start” failures in developer environments while still preserving process
lifecycle limits.

---

## Debugging / escape hatch

### Disable the Windows renderer OS sandbox (debug only)

For local debugging you can disable token/AppContainer sandboxing:

- `FASTR_DISABLE_RENDERER_SANDBOX=1` (any non-empty value other than `0`/`false`/`no`/`off`), or
- `FASTR_WINDOWS_RENDERER_SANDBOX=off`

This is **INSECURE** and removes the primary OS sandbox boundary. FastRender prints a warning to
stderr when this is enabled.

Note: disabling sandboxing does **not** necessarily remove Job Object guardrails (kill-on-close /
active-process limit).

### Verbose sandbox spawn logs

Set `FASTR_LOG_SANDBOX=1` to enable verbose Windows sandbox spawn logs (useful for debugging
AppContainer ACL/workdir issues in dev/test environments).

---

## Windows version constraints

- **AppContainer requires Windows 8+**.
- For best results and modern sandbox behavior, **Windows 10+ is recommended**.

---

## Tests

Windows sandbox/security regression tests live under:

- `tests/sandbox/` (e.g. process handle escape, job-object process creation limits)
- `tests/windows_sandbox_appcontainer_spawn.rs` (AppContainer spawn + opt-out behaviour)

