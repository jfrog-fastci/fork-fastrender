# Windows renderer sandboxing (AppContainer + Job Objects)

FastRender’s long-term multiprocess model assumes **renderer processes are untrusted** and must run
inside a strong OS sandbox boundary.

Detailed Windows sandbox boundary doc (recommended reading): [`docs/windows_sandbox.md`](../windows_sandbox.md).

Canonical sandboxing overview (all platforms): [`docs/sandboxing.md`](../sandboxing.md).

Key Windows code entrypoints:

- Windows sandbox spawn helper: [`src/sandbox/windows.rs`](../../src/sandbox/windows.rs)
  - `spawn_sandboxed(...)` (preferred)
  - `requested_renderer_sandbox_level()` (debug escape hatch parsing)

---

## Default Windows sandbox model

This is a quick reference. The full Windows sandbox design is layered as:

1. **AppContainer (preferred)** with **zero capabilities** (blocks outbound network + most filesystem).
   - Defense in depth: remove the broad `ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) from the
     created AppContainer token via `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`
     (`SpawnConfig::all_application_packages_hardened`; enabled by default in `SpawnConfig::default()`;
     best-effort / retried without on unsupported Windows builds).
2. **Job object** limits (kill-on-close + active process limit; optional memory cap in `crates/win-sandbox`).
3. **Handle inheritance allowlisting** (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`) to prevent capability leaks.
4. **Process mitigations** (Win32k lockdown, dynamic code prohibition, etc.) when enabled.
   - Applied at process creation time via `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` (best-effort; if
     the OS rejects the attribute, the spawn helper retries without mitigations).
   - Debug escape hatch: `FASTR_DISABLE_WIN_MITIGATIONS=1` disables mitigation policies only (it does
     not disable AppContainer/restricted-token sandboxing, job limits, or handle allowlisting).
5. **Fallback mode**: restricted token + Low IL (weaker; network not reliably blocked).

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

If AppContainer is unavailable (or creation fails), the spawner **fails closed** by default and
returns an error that explains which capability is missing.

To opt in to running without the full Windows sandbox (developer convenience on unsupported Windows
versions / unusual CI setups), set:

- `FASTR_ALLOW_UNSANDBOXED_RENDERER=1`

When this opt-in is enabled, the renderer may fall back to spawning with:

- a **restricted token** (max privileges removed), and
- **low integrity**, still under the same Job Object constraints when job assignment succeeds.

Limitations:

- **Network may still be available** in this mode (depending on what the restricted token removes and
  system policy). Do not treat it as equivalent to AppContainer.

### Last resort fallback: unsandboxed spawn (+ job object)

If both AppContainer and restricted-token spawning fail (and
`FASTR_ALLOW_UNSANDBOXED_RENDERER=1` is set), `spawn_sandboxed(...)` falls back to an unsandboxed
spawn (still attempts to apply the Job Object limits; if job assignment fails due to host job
restrictions it may run jobless) and logs a warning to stderr.

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

### Allow running without the full Windows sandbox (opt-in)

On unsupported Windows versions (or when sandbox setup fails due to host job restrictions), you can
opt in to running without the full sandbox by setting:

- `FASTR_ALLOW_UNSANDBOXED_RENDERER=1`

This is intended for development/debugging only: it can enable weaker sandboxing or an unsandboxed
spawn.

### Disable mitigation policies only (debug/compatibility)

If a particular Windows build has compatibility issues with process mitigation policies, you can
disable only that layer by setting:

- `FASTR_DISABLE_WIN_MITIGATIONS=1`

This keeps the primary sandbox boundary intact (AppContainer / restricted token + job limits + handle
allowlisting).

---

## Windows version constraints

- **AppContainer requires Windows 8+**.
- Some hardened Windows environments expose the AppContainer exports but still refuse profile creation
  (e.g. `CreateAppContainerProfile` fails due to system policy); treat this as “AppContainer unavailable”.
- For best results and modern sandbox behavior, **Windows 10+ is recommended**.

---

## Tests

Windows sandbox/security regression tests live under:

- `tests/sandbox/` (e.g. process handle escape, job-object process creation limits)
- `tests/sandbox/windows_sandbox_appcontainer_spawn.rs` (AppContainer spawn + opt-out behaviour)
- `tests/sandbox/windows_all_application_packages_hardening.rs` (AppContainer token omits `ALL APPLICATION PACKAGES` group when hardening is enabled/supported)
- `tests/sandbox/windows_renderer_smoke.rs` (end-to-end: sandboxed child can initialize FastRender + render minimal HTML)
- `tests/sandbox/windows_renderer_sandbox_test.rs` (end-to-end: token state + filesystem/network denial + job kill-on-close)
- `crates/win-sandbox/tests/` (AppContainer/network/filesystem/restricted-token invariants + helper spawners)
