# Sandboxing (renderer process)

FastRender is moving toward a **multiprocess** architecture where untrusted page content is rendered
in a separate OS-sandboxed *renderer process*. This document captures the intended sandbox behavior
and the practical debugging knobs.

**Status / repo reality (today):**

- The windowed `browser` app still runs the “renderer” on a worker thread, not a separate OS
  process.
- The Windows sandbox code in [`src/sandbox/windows.rs`](../src/sandbox/windows.rs) currently
  implements *sandbox selection* plus a debug escape hatch. Spawn-time enforcement is expected to be
  wired up as the renderer process model lands.

Related docs (other platforms / tooling):

- Linux seccomp allowlist workflow: [seccomp_allowlist.md](seccomp_allowlist.md)
- macOS sandbox probe tool: [macos_sandbox.md](macos_sandbox.md)

## Windows sandbox implementation

Windows sandbox code lives in [`src/sandbox/windows.rs`](../src/sandbox/windows.rs).

### Primary mode: AppContainer (zero capabilities)

When available, the renderer is intended to be spawned inside an **AppContainer** with **no
capabilities**:

- **No network**: no `INTERNET_CLIENT` capability is granted, so outbound network access is blocked.
- **Restricted filesystem**: access is limited to what AppContainer policy permits (no arbitrary
  access to the user profile / system directories).

AppContainer is the preferred mode because it provides a strong, OS-supported isolation boundary.

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

### Windows version constraints

- **AppContainer requires Windows 8+**.
- For best results and modern sandbox behavior, **Windows 10+ is recommended**.

### Debug escape hatch (Windows)

When debugging sandbox-related issues locally, you can disable the Windows renderer sandbox:

- `FASTR_DISABLE_RENDERER_SANDBOX=1`, or
- `FASTR_WINDOWS_RENDERER_SANDBOX=off`

When sandboxing is disabled, FastRender prints a **warning to stderr** so insecure runs are not
silent.

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
