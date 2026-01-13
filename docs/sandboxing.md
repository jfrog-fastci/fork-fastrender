# Sandboxing (renderer process)

FastRender is moving toward a **multiprocess** architecture where untrusted page content is rendered
in a separate OS-sandboxed *renderer process*. This document captures the intended sandbox behavior
and the practical debugging knobs.

**Status / repo reality (today):**

- The windowed `browser` app still runs the “renderer” on a worker thread, not a separate OS
  process.
- The OS sandbox helpers in `src/sandbox/` are used primarily by **tests/tooling** today and are
  intended to be reused by the future renderer *process* model.
  - Windows: [`src/sandbox/windows.rs`](../src/sandbox/windows.rs) exposes a `spawn_sandboxed(...)`
    helper that applies AppContainer/restricted-token sandboxing plus a Job Object.
- Policy knobs and status reporting are surfaced via `RendererSandboxConfig` / `SandboxStatus` in
  [`src/sandbox/mod.rs`](../src/sandbox/mod.rs).

Related docs (other platforms / tooling):

- Renderer sandbox entrypoint (links to all platform docs): [renderer_sandbox.md](renderer_sandbox.md)
- Linux renderer sandbox deep dive (rlimits/fd hygiene/namespaces/Landlock/seccomp): [security/sandbox.md](security/sandbox.md)
- Linux seccomp allowlist workflow: [seccomp_allowlist.md](seccomp_allowlist.md)
- Windows renderer sandbox boundary (Job/AppContainer details): [windows_sandbox.md](windows_sandbox.md)
- Windows renderer sandbox quick reference: [security/windows_renderer_sandbox.md](security/windows_renderer_sandbox.md)
- macOS sandbox probe tool: [macos_sandbox.md](macos_sandbox.md)
- macOS renderer sandboxing (Seatbelt now, App Sandbox later): [security/macos_renderer_sandbox.md](security/macos_renderer_sandbox.md)

## Windows sandbox implementation

Windows sandbox code lives in [`src/sandbox/windows.rs`](../src/sandbox/windows.rs).

### Primary mode: AppContainer (zero capabilities)

When available, the renderer is intended to be spawned inside an **AppContainer** with **no
capabilities**:

- **No network**: no `INTERNET_CLIENT` capability is granted, so outbound network access is blocked.
- **Restricted filesystem**: access is limited to what AppContainer policy permits (no arbitrary
  access to the user profile / system directories).

AppContainer is the preferred mode because it provides a strong, OS-supported isolation boundary.

#### Developer note: executing the renderer binary under AppContainer

On Windows, un-packaged dev/test binaries may not be executable by an AppContainer token if the
binary’s directory does not grant the derived AppContainer SID read/execute access. In that case,
`spawn_sandboxed(...)` may:

1. Copy the renderer image to a temporary directory, and
2. Grant the AppContainer SID read/execute ACLs on the copied file (narrowest),
   - Compatibility fallback: if granting to the AppContainer SID fails unexpectedly, it may fall
     back to granting **ALL APPLICATION PACKAGES**. Note that when
     `SpawnConfig::all_application_packages_hardened` is enabled, the token does **not** contain
     that group, so this fallback only helps when hardening is disabled or unsupported on the host.
3. Retry the AppContainer spawn.

Use `FASTR_LOG_SANDBOX=1` to see detailed logs for this path.

#### Defense in depth: remove `ALL APPLICATION PACKAGES` from the AppContainer token

As a defense-in-depth hardening layer, the Windows spawner can remove the broad
`ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) from the created AppContainer token via
`PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`.

This is controlled by `SpawnConfig::all_application_packages_hardened` (enabled by default for the
renderer sandbox). On Windows builds that do not support the attribute, the spawner retries without
it.

### Defense in depth: Job object guardrails

In addition to the token/AppContainer sandbox, the renderer process is placed in a Windows **Job
Object** configured with:

- **Kill-on-close**: if the parent (browser/orchestrator) dies or drops the job handle, the OS kills
  the renderer process tree (`KILL_ON_JOB_CLOSE`).
- **Active-process limit**: a hard cap on the number of processes the renderer can create (helps
  contain fork bombs / runaway child spawning).

These constraints are intended as *defense in depth* and lifecycle hygiene, even when the renderer
itself crashes or misbehaves.

### Defense in depth: process mitigation policies (Windows)

When spawning a sandboxed renderer on Windows, we also apply a default set of *process mitigation
policies* at process creation time (best-effort) via `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`:

- Win32k lockdown (disable `win32k.sys` system calls; headless-only)
- prohibit dynamic code
- disable extension points (legacy injection)
- image load hardening (no remote / no low-mandatory-label images)
- strict handle checks

If the host OS rejects the mitigation attribute (for example on older/unusual Windows builds), the
spawn logic retries without mitigations rather than failing process creation.

Debug/compatibility escape hatch:

- `FASTR_DISABLE_WIN_MITIGATIONS=1` disables **mitigation policies only** (it does not disable
  AppContainer/restricted-token sandboxing, Job Object limits, or handle allowlisting).

### Fallback mode: restricted token + low integrity (+ job object)

If AppContainer is unavailable (or creation fails), the sandbox spawner **fails closed** by default
and returns an error describing the missing capability.

For developer convenience on unsupported Windows versions / unusual CI environments, you can opt in
to running without the full sandbox by setting:

- `FASTR_ALLOW_UNSANDBOXED_RENDERER=1`

When this opt-in is enabled, the spawner may fall back to a **restricted token** + **low integrity**
mode (still under the same Job Object constraints when job assignment succeeds) when AppContainer is
unavailable.

Limitations of the fallback:

- **Network may still be available** depending on system policy and what the restricted token
  removes; do not treat this mode as equivalent to AppContainer.

If both AppContainer and restricted-token sandboxing fail (and
`FASTR_ALLOW_UNSANDBOXED_RENDERER=1` is set), `spawn_sandboxed(...)` falls back to an unsandboxed
spawn (still attempts to apply the Job Object limits; if job assignment fails due to host job
restrictions, it may run jobless) and prints a warning to stderr.

### Windows version constraints

- **AppContainer requires Windows 8+**.
- Some hardened Windows environments expose the AppContainer exports but still refuse profile creation
  or token usage (e.g. `CreateAppContainerProfile` fails due to system policy). Treat this as “no
  AppContainer support”: the spawner fails closed by default unless explicitly opted into fallback
  mode (`FASTR_ALLOW_UNSANDBOXED_RENDERER=1`).
- For best results and modern sandbox behavior, **Windows 10+ is recommended**.

### Debug escape hatch (Windows)

When debugging sandbox-related issues locally, you can disable the Windows renderer sandbox:

- `FASTR_DISABLE_RENDERER_SANDBOX=1`, or
- `FASTR_WINDOWS_RENDERER_SANDBOX=off`

This is **for debugging only**: it removes the primary OS sandbox boundary for the renderer.

When sandboxing is disabled, FastRender prints a **warning to stderr** so insecure runs are not
silent.

Note: this escape hatch disables the *token/AppContainer* restrictions; the renderer may still be
run inside a Job Object (kill-on-close, active-process cap) for lifecycle safety.

Windows note: even when token/AppContainer sandboxing is disabled, the Windows spawn helper still
builds a sanitized environment block by default (no secret env inheritance; `TEMP`/`TMP` override).
Set `FASTR_WINDOWS_SANDBOX_INHERIT_ENV=1` if you need the child to inherit the full parent
environment (debug only).

To debug why AppContainer/restricted-token spawning is failing, enable verbose sandbox logs:

- `FASTR_LOG_SANDBOX=1`
- `FASTR_WINDOWS_SANDBOX_INHERIT_ENV=1`: opt into inheriting the full parent environment for the
  sandboxed child (disables environment sanitization and the default `TEMP`/`TMP` override; debug
  only).

Value parsing details:

- `FASTR_DISABLE_RENDERER_SANDBOX`: any non-empty value **other than** `0`/`false`/`no`/`off` disables
  sandboxing.
- `FASTR_WINDOWS_RENDERER_SANDBOX`: `off`/`0`/`false`/`no` disables sandboxing; any other value keeps
  sandboxing enabled.

## Linux sandbox implementation

Linux sandbox code lives in:

- `seccomp-bpf`: [`src/sandbox/linux_seccomp.rs`](../src/sandbox/linux_seccomp.rs) (installed via
  [`src/sandbox/mod.rs`](../src/sandbox/mod.rs))
- Optional filesystem defense-in-depth (Landlock): [`src/sandbox/linux_landlock.rs`](../src/sandbox/linux_landlock.rs)
- Optional namespace hardening (best-effort, before seccomp; disabled by default and controlled via
  `RendererSandboxConfig::linux_namespaces`): [`src/sandbox/linux_namespaces.rs`](../src/sandbox/linux_namespaces.rs)
- Spawn-time hardening prelude (Linux `CommandExt::pre_exec`): [`src/sandbox/spawn.rs`](../src/sandbox/spawn.rs)
  (`sandbox::spawn::configure_renderer_command`). Note: this cannot install the *full* renderer seccomp
  policy (it needs `execve(2)`); install the full sandbox early in the renderer process via
  `sandbox::apply_renderer_sandbox(...)`.

Repo reality (today): the Linux seccomp sandbox is designed to:

- disable dumpability (`prctl(PR_SET_DUMPABLE, 0)`) before installing the filter to reduce
  `ptrace`/`/proc` leakage and to disable core dumps (we intentionally do **not** relax ptrace via
  `PR_SET_PTRACER`)
- deny path-based filesystem opens (`open`, `openat`, `openat2`, `creat`)
- deny network socket creation by default (including `AF_UNIX`), and deny most socket-specific
  syscalls (`connect`/`sendmsg`/`recvmsg`/etc).
  - Pre-existing inherited socketpairs can still be used via `read(2)`/`write(2)` (they are just file
    descriptors at that point), but features like FD passing (`SCM_RIGHTS`) require allowing
    `sendmsg`/`recvmsg`.
 - When an embedding explicitly opts in (see `NetworkPolicy::AllowUnixSocketsOnly` in
    `src/sandbox/mod.rs`), `socket(AF_UNIX, ...)` / `socketpair(AF_UNIX, ...)` are allowed for local
    IPC while non-Unix socket creation remains denied.
- deny process execution (`execve`, `execveat`)

Landlock note:

- Landlock is **disabled by default** (`RendererLandlockPolicy::Disabled`).
- When enabled for the renderer (`RendererLandlockPolicy::RestrictWrites`), it is best-effort and is
  used as defense-in-depth (deny filesystem writes globally while still allowing reads so dynamic
  linking continues to work).

Maintaining the syscall allowlist is a moving target; use the workflow in
[seccomp_allowlist.md](seccomp_allowlist.md).

IPC implication: prefer **inherited IPC endpoints** (e.g. pipes, or a pre-created `socketpair()`)
and browser-allocated shared memory (`memfd_create`) passed to the renderer before the sandbox is
installed; see
[ipc.md](ipc.md) and [ipc_linux_fd_passing.md](ipc_linux_fd_passing.md).

### Debug / developer overrides (Linux)

FastRender exposes lightweight runtime knobs for iterating on Linux sandbox bring-up. These are
documented in [`docs/env-vars.md`](env-vars.md) and are primarily consumed by:

- the `sandbox_probe` tool (`src/bin/sandbox_probe.rs`), and
- Linux renderer spawn helpers (e.g. `fastrender::sandbox::spawn::configure_renderer_command`).

Key env vars:

- Master disable (debug escape hatch; **INSECURE**): `FASTR_DISABLE_RENDERER_SANDBOX=1`
- Disable seccomp: `FASTR_RENDERER_SECCOMP=0`
- Disable Landlock: `FASTR_RENDERER_LANDLOCK=0`
- FD hygiene toggle (currently used by `sandbox_probe`): `FASTR_RENDERER_CLOSE_FDS=0|1`

## macOS sandbox notes

Renderer sandboxing on macOS uses Seatbelt profiles. For iteration tooling and IPC capability
expectations, see [macos_sandbox.md](macos_sandbox.md) (and the launcher helper in
[`src/sandbox/macos_spawn.rs`](../src/sandbox/macos_spawn.rs)).

### Debug escape hatches (macOS)

- Control renderer sandbox mode (recommended):
  - `FASTR_RENDERER_SANDBOX=strict|relaxed|off`
  - Legacy aliases: `1` = `strict`, `0` = `off`.
- Disable renderer sandboxing (INSECURE): `FASTR_RENDERER_SANDBOX=off`,
  `FASTR_DISABLE_RENDERER_SANDBOX=1`, or `FASTR_MACOS_RENDERER_SANDBOX=off`.
- Select the relaxed "system fonts" profile for bring-up (still blocks network + user filesystem):
  - `FASTR_RENDERER_SANDBOX=relaxed` (preferred), or
  - `FASTR_MACOS_RENDERER_SANDBOX=system-fonts` (legacy alias).
- Advanced override (macOS Seatbelt profile selection): `FASTR_RENDERER_MACOS_SEATBELT_PROFILE=...`
  - Accepted values: `pure-computation`, `no-internet`, `renderer-default`, or a path to an SBPL file.
  - When set, this overrides the `strict`/`relaxed` profile mapping when sandboxing is enabled.
- Opt into wrapping spawns with Apple’s deprecated `sandbox-exec` wrapper when using `macos_spawn`
  helpers: `FASTR_MACOS_USE_SANDBOX_EXEC=1`.
  - Note: this is ignored when sandboxing is disabled via `FASTR_RENDERER_SANDBOX=off`,
    `FASTR_DISABLE_RENDERER_SANDBOX=1`, or `FASTR_MACOS_RENDERER_SANDBOX=off`.

When FastRender is eventually packaged as a macOS `.app`, the renderer helper process may also be
sandboxed via App Sandbox entitlements embedded in the code signature; see
[security/macos_renderer_sandbox.md](security/macos_renderer_sandbox.md).
