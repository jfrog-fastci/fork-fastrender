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

Related docs (other platforms / tooling):

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
2. Grant the AppContainer SID (or, as a fallback, **ALL APPLICATION PACKAGES**) read/execute ACLs on
   the copied file,
3. Retry the AppContainer spawn.

Use `FASTR_LOG_SANDBOX=1` to see detailed logs for this path.

### Defense in depth: Job object guardrails

In addition to the token/AppContainer sandbox, the renderer process is placed in a Windows **Job
Object** configured with:

- **Kill-on-close**: if the parent (browser/orchestrator) dies or drops the job handle, the OS kills
  the renderer process tree (`KILL_ON_JOB_CLOSE`).
- **Active-process limit**: a hard cap on the number of processes the renderer can create (helps
  contain fork bombs / runaway child spawning).

These constraints are intended as *defense in depth* and lifecycle hygiene, even when the renderer
itself crashes or misbehaves.

### Fallback mode: restricted token + low integrity (+ job object)

If AppContainer is unavailable (or creation fails), the renderer is intended to fall back to
spawning with a **restricted token** and **low integrity**, still under the same Job Object
constraints.

Limitations of the fallback:

- **Network may still be available** depending on system policy and what the restricted token
  removes; do not treat this mode as equivalent to AppContainer.

If both AppContainer and restricted-token sandboxing fail, `spawn_sandboxed(...)` falls back to an
unsandboxed spawn (still inside a Job Object) and prints a warning to stderr.

### Windows version constraints

- **AppContainer requires Windows 8+**.
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

To debug why AppContainer/restricted-token spawning is failing, enable verbose sandbox logs:

- `FASTR_LOG_SANDBOX=1`

Value parsing details:

- `FASTR_DISABLE_RENDERER_SANDBOX`: any non-empty value **other than** `0`/`false`/`no`/`off` disables
  sandboxing.
- `FASTR_WINDOWS_RENDERER_SANDBOX`: `off`/`0`/`false`/`no` disables sandboxing; any other value keeps
  sandboxing enabled.

## Linux sandbox implementation

Linux sandbox code lives in:

- `seccomp-bpf`: [`src/sandbox/linux_seccomp.rs`](../src/sandbox/linux_seccomp.rs) (installed via
  [`src/sandbox/mod.rs`](../src/sandbox/mod.rs))
- Optional filesystem defense-in-depth: [`src/sandbox/linux_landlock.rs`](../src/sandbox/linux_landlock.rs)

Repo reality (today): the Linux seccomp sandbox is designed to:

- deny path-based filesystem opens (`open`, `openat`, `openat2`, `creat`)
- deny network socket operations (and restrict `socket()` to `AF_UNIX`)
- deny process execution (`execve`, `execveat`)

Maintaining the syscall allowlist is a moving target; use the workflow in
[seccomp_allowlist.md](seccomp_allowlist.md).

IPC implication: prefer **inherited IPC endpoints** (e.g. `socketpair()`) and browser-allocated
shared memory (`memfd_create`) passed to the renderer before the sandbox is installed; see
[ipc_linux_fd_passing.md](ipc_linux_fd_passing.md).

## macOS sandbox notes

Renderer sandboxing on macOS uses Seatbelt profiles. For iteration tooling and IPC capability
expectations, see [macos_sandbox.md](macos_sandbox.md) (and the launcher helper in
[`src/sandbox/macos_spawn.rs`](../src/sandbox/macos_spawn.rs)).

### Debug escape hatches (macOS)

- Disable renderer sandboxing (INSECURE): `FASTR_DISABLE_RENDERER_SANDBOX=1` or
  `FASTR_MACOS_RENDERER_SANDBOX=off`.
- Select the relaxed "system fonts" profile for bring-up:
  `FASTR_MACOS_RENDERER_SANDBOX=system-fonts`.
- Opt into wrapping spawns with Apple’s deprecated `sandbox-exec` wrapper when using `macos_spawn`
  helpers: `FASTR_MACOS_USE_SANDBOX_EXEC=1`.

When FastRender is eventually packaged as a macOS `.app`, the renderer helper process may also be
sandboxed via App Sandbox entitlements embedded in the code signature; see
[security/macos_renderer_sandbox.md](security/macos_renderer_sandbox.md).
