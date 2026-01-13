# Renderer sandboxing (Linux-focused)

This doc is a **Linux deep-dive** companion to:

- the renderer sandbox entrypoint doc: [`docs/renderer_sandbox.md`](../renderer_sandbox.md)
- the cross-platform sandbox overview: [`docs/sandboxing.md`](../sandboxing.md)

It explains:

- the threat model for a sandboxed renderer process,
- the Linux sandbox **layering and ordering constraints** (setup layers like rlimits/fd hygiene/namespaces/Landlock
  must run before seccomp; seccomp must be last),
- the current seccomp “hybrid allowlist + denylist” policy,
- and how to debug sandbox bring-up failures.

---

## Runtime configuration knobs (developer ergonomics)

FastRender exposes lightweight **runtime** toggles for iterating on sandbox behavior without
rebuilding. See [`docs/env-vars.md`](../env-vars.md) for the full list.

Linux-specific knobs (also consumed by the `sandbox_probe` tool and Linux renderer spawn helpers):

- Master disable (debug escape hatch; **INSECURE**): `FASTR_DISABLE_RENDERER_SANDBOX=1`
- Disable/enable layers:
  - `FASTR_RENDERER_SECCOMP=0|1`
  - `FASTR_RENDERER_LANDLOCK=0|1`
  - `FASTR_RENDERER_CLOSE_FDS=0|1` (currently used by `sandbox_probe`)

Repo entrypoints:

- Env parsing: `src/system/renderer_sandbox.rs` (std-only parsing helpers).
- Spawn-time sandboxing: `src/sandbox/spawn.rs` (`sandbox::spawn::configure_renderer_command`).
- In-process sandboxing: `src/sandbox/mod.rs` (`sandbox::apply_renderer_sandbox`).

## Threat model

Assume the attacker controls:

- HTML/CSS (including malformed inputs, parser bugs, quadratic corner cases).
- JavaScript (once enabled): arbitrary code execution inside the renderer’s JS VM.
- Subresource bytes (images/fonts/etc) served from the network.

Assume the renderer may contain vulnerabilities (including memory corruption, logic bugs, or unsafe
FFI in dependencies). Therefore **the renderer process must be treated as untrusted**.

---

## Security goals / guarantees (what the sandbox must enforce)

When the renderer sandbox is enabled, a compromised renderer process should be unable to:

1. **Read or write the host filesystem** (no path-based file opens or mutations).
2. **Perform network access** (no AF_INET/AF_INET6 sockets; no TCP connects).
3. **Execute new programs** (`execve`/`execveat`).
4. **Use high-risk kernel attack surfaces** (BPF/perf/ptrace/keyrings/etc).
5. **Abuse leaked file descriptors** (browser must not leak privileged fds into the renderer).

The renderer is still allowed to compute (CPU + memory) and to communicate over **explicit IPC
endpoints** provided by the browser process.

### Resource fetching contract (critical)

The sandbox assumes the renderer **does not fetch resources directly**:

- the renderer must not do HTTP(S) itself,
- and it must not open arbitrary `file://` paths.

Instead, the renderer should use an IPC-backed implementation of the library’s
[`ResourceFetcher`](../../src/resource.rs) trait, with the browser/network process mediating all I/O.

This is a security boundary:
if the renderer can open sockets or arbitrary files, the OS sandbox becomes “best effort” instead of
a hard isolation line.

### IPC + shared memory contract (critical)

The sandbox assumes the renderer communicates only via **explicit IPC endpoints** provided by the
browser process.

Practical implications on Linux:

- With the default `NetworkPolicy::DenyAllSockets`, the renderer should not create sockets at all
  (even `AF_UNIX`). IPC endpoints (pipes or socketpairs) must be created by the browser and
  inherited/passed into the renderer.
  - In this mode, inherited Unix sockets are still usable via `read(2)`/`write(2)`, but FD passing
    via `sendmsg`/`recvmsg` is not available after the sandbox is installed.
- Shared memory should be created by the browser (for example via `memfd_create` in the broker) and
  passed to the renderer. The seccomp allowlist does **not** include `memfd_create`, so calling it
  after the sandbox is installed would typically be a fatal seccomp violation.
- If you need to pass file descriptors at runtime (after sandbox), use Unix sockets and allow
  `sendmsg`/`recvmsg` (see `NetworkPolicy::AllowUnixSocketsOnly`) and follow the checklist in
  [`docs/ipc_linux_fd_passing.md`](../ipc_linux_fd_passing.md).

Related docs: [`docs/ipc.md`](../ipc.md), [`docs/ipc_linux_fd_passing.md`](../ipc_linux_fd_passing.md).

### Cargo feature implications (renderer builds)

The sandbox boundary assumes the renderer does not even *have* in-process network/filesystem stacks.

Repo reality:

- Default crate features include `direct_network`, `direct_websocket`, and `direct_filesystem` (see
  `Cargo.toml`). These are convenient for single-process/library use, but are **not** appropriate
  for an OS-sandboxed renderer process.
- For renderer-process builds, prefer disabling these features and using an IPC-backed fetcher /
  WebSocket bridge.
  - CI uses `--no-default-features --features renderer_minimal` to ensure the renderer can be built
    without linking reqwest/ureq/tungstenite.

---

## Linux sandbox layering and order of operations

On Linux we intentionally layer multiple mechanisms. **Order matters** because later layers can
prevent the syscalls needed to set up earlier layers.

**Order constraints (Linux):**

- If namespaces are enabled, they must be applied **before seccomp** (seccomp may block `unshare(2)` / `setns(2)`).
- If Landlock is enabled, it must be applied **before seccomp**.
- Install seccomp **last**.

Rlimits and fd hygiene are orthogonal guardrails and should be applied early; in the current
implementation, the optional namespace step runs first (when enabled), followed by rlimits/fd
hardening, then fd cleanup, then Landlock, and finally seccomp.

Repo reality:

- Landlock + seccomp are implemented in `src/sandbox/` and applied by
  `sandbox::apply_renderer_sandbox` (`src/sandbox/mod.rs`).
- `sandbox::apply_renderer_sandbox` can apply a **best-effort Linux namespace** hardening step
  (currently a network namespace when permitted) *before* seccomp, since seccomp may deny
  `unshare(2)` / `setns(2)`. This is controlled by `RendererSandboxConfig::linux_namespaces`
  (disabled by default). See `src/sandbox/linux_namespaces.rs`.
- Some rlimits/hardening are applied **in-process** by `sandbox::apply_renderer_sandbox` (via
  `linux_hardening`), before installing seccomp.
  - Default values live in `RendererSandboxConfig::default()` (`src/sandbox/mod.rs`) and include
    `RLIMIT_NOFILE=256` and `RLIMIT_CORE=0` (plus `PR_SET_DUMPABLE=0`).
  - `RLIMIT_AS` is supported but is not enabled by default (`address_space_limit_bytes: None`).
- File descriptor hygiene (closing unexpected inherited fds / setting `CLOEXEC`) is primarily the
  responsibility of the **process launcher** (especially for spawn-time sandboxing).
  - `sandbox::apply_renderer_sandbox` can also close unexpected fds in-process when
    `RendererSandboxConfig::close_extra_fds` is enabled (default), preserving fds 0-3 (stdio + the
    IPC bootstrap fd).
  - Helpers for launchers: `sandbox::close_fds_except(...)` and `sandbox::set_cloexec_on_fds_except(...)`
    (`src/sandbox/fd_sanitizer.rs`).
  - Prefer `close_fds_except` from a `Command::pre_exec` hook when spawning a dedicated renderer
    subprocess.
  - Prefer `set_cloexec_on_fds_except` when using `std::process::Command` without `pre_exec` and you
    want to avoid interfering with Cargo/std internal CLOEXEC pipes.

### 1) rlimits (resource guardrails)

Apply kernel-enforced rlimits early in renderer startup.

Common caps:

- `RLIMIT_AS` (virtual address space ceiling; hard memory cap)
- `RLIMIT_NOFILE` (fd cap)
- `RLIMIT_NPROC` (defense-in-depth against process spawning)
- `RLIMIT_CORE=0` (no core dumps)

Related: [`docs/resource-limits.md`](../resource-limits.md), `src/process_limits.rs`, and
`RendererSandboxConfig` defaults in `src/sandbox/mod.rs`.

### 2) Close file descriptors (no fd leaks)

Before sandboxing, ensure the renderer does not inherit privileged fds.

Policy:

- Keep only stdin/stdout/stderr (or explicit redirections) and the intended IPC fds.
- Close everything else (prefer `close_range` where available).
- Prefer `CLOEXEC` on IPC fds to prevent accidental leaks across exec boundaries.

Repo reality nuance:

- `sandbox::close_fds_except(...)` is designed to be used from a `pre_exec` hook where the launcher
  knows exactly which fds must remain open.
- `sandbox::apply_renderer_sandbox` also performs post-`exec` fd cleanup when
  `RendererSandboxConfig::close_extra_fds` is enabled (default), closing everything except fds 0-3.
- When using `std::process::Command`, there may be internal exec-error-reporting pipes that are hard
  to include in a strict keep-list. In those cases, it can be safer to use
  `sandbox::set_cloexec_on_fds_except(...)` (mark fds close-on-exec without closing them) and/or do
  post-`exec` fd cleanup inside the renderer entrypoint once IPC wiring is finalized.

### 3) Linux namespaces (best-effort hardening)

Linux namespaces can provide additional defense-in-depth isolation (for example, a fresh network
namespace with no interfaces configured).

Repo reality:

- Namespace isolation is best-effort and currently applied (when enabled) by:
  - `sandbox::apply_renderer_sandbox` (`src/sandbox/mod.rs`), and
  - the Linux spawn-time sandboxing path (`src/sandbox/spawn.rs`).
- This must run **before** seccomp, since the renderer seccomp policy may deny `unshare(2)`/`setns(2)`.
- In many environments (notably containers or hosts with user namespaces disabled), this may fail.
  That is expected; the sandbox still relies on seccomp as the primary syscall boundary.

Implementation: `src/sandbox/linux_namespaces.rs`.

### 4) Landlock (filesystem defense in depth)

Landlock is an LSM that enforces a **path-based** filesystem policy.

Implementation: `src/sandbox/linux_landlock.rs`.

Repo reality:

- `LandlockConfig::default()` / `LandlockConfig::deny_all()` installs a deny-all ruleset (no
  allowlisted paths). This is useful for tests and diagnostics (for example `sandbox_probe --mode
  landlock`).
- The renderer sandbox (`sandbox::apply_renderer_sandbox`) currently treats Landlock as optional and
  **disabled by default** (`RendererLandlockPolicy::Disabled`).
  - When enabled (`RendererLandlockPolicy::RestrictWrites`), it uses a best-effort policy that
    denies filesystem writes globally while still allowing reads (so pre-opened read-only FDs and
    dynamic linking remain usable).
  - If Landlock is unsupported, sandbox setup continues (seccomp is still applied).
  - If Landlock is supported but applying it fails, we fail closed.

Note: the “no path-based filesystem access” guarantee is enforced primarily by the **seccomp**
denylist (blocking `open/openat/openat2/statx/...`). Landlock is defense-in-depth.

Why Landlock at all if seccomp blocks `openat`?

- Landlock still provides defense-in-depth if the seccomp policy evolves or a filesystem-related
  syscall slips through the filter.

### 5) seccomp-bpf (syscall filter: hybrid allowlist + denylist)

Implementation: `src/sandbox/linux_seccomp.rs`.

Key properties:

- Uses `PR_SET_NO_NEW_PRIVS` (required for unprivileged seccomp).
- Installs `SECCOMP_MODE_FILTER` via the `seccomp()` syscall, attempting to use
  `SECCOMP_FILTER_FLAG_TSYNC` so the filter applies to all threads.
  - When TSYNC is unavailable, we fall back to installing without it and return
    `SandboxStatus::AppliedWithoutTsync` — callers must apply the sandbox **before** spawning any
    additional threads.
- Default action is **kill the process** for unexpected syscalls.

#### Why a hybrid policy?

The Linux renderer filter uses:

- an **allowlist** (syscalls the renderer may use in its steady state), and
- a small explicit **denylist** that returns `EPERM` for “expected to be denied” operations
  (filesystem/network/exec).

Returning `EPERM` (instead of killing the process) makes “forbidden capability” failures predictable
and testable (see `sandbox` module unit tests), while still keeping a strict kill-by-default posture
for unexpected syscalls.

#### Denylisted syscalls (Linux renderer)

The authoritative list lives in `src/sandbox/linux_seccomp.rs` (`deny = [...]`), but it currently
includes (non-exhaustive):

- **Filesystem open + mutation**: `open`, `openat`, `openat2`, `creat`, `unlink*`, `rename*`,
  `mkdir*`, `rmdir`, `link*`, `symlink*`, `chmod*`, `chown*`, `truncate*`
- **Namespace / mount escape**: `mount`, `umount2`, `pivot_root`, `chroot`, `unshare`, `setns`
- **Program exec**: `execve`, `execveat`
- **Network surface** (when `NetworkPolicy::DenyAllSockets`): `connect`, `bind`, `listen`, `accept*`,
  `send*`, `recv*`, `set/get sockopt`
- **High-risk kernel APIs**: `ptrace`, `bpf`, `perf_event_open`, `kexec_load`,
  `process_vm_{readv,writev}`, `userfaultfd`, `keyctl`/`add_key`/`request_key`

Additionally, `socket(2)` / `socketpair(2)` are special-cased via `NetworkPolicy`:

- Default (`NetworkPolicy::DenyAllSockets`): deny all socket creation (including `AF_UNIX`) and deny
  socket-specific operations (`connect`/`bind`/`sendmsg`/`recvmsg`/etc) with `EPERM`.
  - IPC must use **inherited file descriptors** created by the browser (pipes or pre-created Unix
    socketpairs) and should stick to `read(2)`/`write(2)` for steady-state messaging.
  - This mode intentionally blocks `sendmsg`/`recvmsg`, so **FD passing after sandbox install** is
    not available (do it before the sandbox, or switch to `AllowUnixSocketsOnly`).
- Optional (`NetworkPolicy::AllowUnixSocketsOnly`): allow `socket(AF_UNIX, ...)`,
  `socketpair(AF_UNIX, ...)`, and Unix-socket operations (including FD passing via `SCM_RIGHTS`)
  while denying creation of other domains (`AF_INET`, `AF_INET6`, …) with `EPERM`.
  - Security note: this mode assumes strict FD hygiene so the renderer does not inherit any
    pre-existing network sockets from the parent.

For allowlist maintenance workflow (when the renderer legitimately needs more syscalls), see:
[seccomp_allowlist.md](../seccomp_allowlist.md) and `scripts/trace_renderer_syscalls.sh`.

## Applying the sandbox to a renderer process (Linux)

There are two common integration patterns:

### 1) Apply in-process (inside the renderer binary)

Call `sandbox::apply_renderer_sandbox(...)` early in renderer startup:

- before starting any thread pools (or you may miss threads when TSYNC is unavailable),
- after setting up any required IPC fds / shared memory mappings.

This is the simplest pattern when the renderer is launched directly and can hard-fail if sandbox
setup fails.

### 2) Apply at spawn time (preferred on Linux)

When the browser spawns a dedicated renderer **subprocess**, prefer applying the sandbox in the
child process *after* `fork(2)` and *before* `execve(2)`. This minimizes the unsandboxed window.

Repo helper: `sandbox::spawn::configure_renderer_command(...)` (`src/sandbox/spawn.rs`).

On Linux this uses `std::os::unix::process::CommandExt::pre_exec` and therefore has strict safety
requirements (no allocations, no locks) — the helper is written to be `pre_exec`-safe.

Important nuance: a `pre_exec` hook cannot install the *full* renderer seccomp policy (the renderer
policy intentionally denies `execve` and path-based file opens). The spawn-time path therefore applies
an **early hardening subset** (PDEATHSIG, rlimits, optional Landlock, and a minimal “deny socket
creation” seccomp filter that still allows `execve`), and the renderer binary should still install
the full renderer sandbox early during startup (via `sandbox::apply_renderer_sandbox`).

Note: `configure_renderer_command` also respects the Linux sandbox env toggles
(`FASTR_DISABLE_RENDERER_SANDBOX`, `FASTR_RENDERER_SECCOMP`, `FASTR_RENDERER_LANDLOCK`) so developers
can disable layers during bring-up.

---

## Kernel / CI requirements and feature detection (Linux)

### seccomp prerequisites / failure modes

- Kernel must support `seccomp-bpf` (`CONFIG_SECCOMP` + `CONFIG_SECCOMP_FILTER`).
- `SECCOMP_FILTER_FLAG_TSYNC` is attempted for whole-process coverage. If the running kernel rejects
  it with `EINVAL`, FastRender falls back to installing the filter without TSYNC and reports
  `SandboxStatus::AppliedWithoutTsync` (sandbox must be applied before spawning threads).
- In containerized CI, an outer seccomp profile may block installing a filter, yielding `EPERM`.

The sandbox code maps these failures into structured errors (see `SandboxError` in
`src/sandbox/mod.rs`).

### Landlock prerequisites / failure modes

Landlock requires:

- Linux kernel ≥ **5.13** (`landlock_*` syscalls),
- `CONFIG_SECURITY_LANDLOCK`,
- and (currently) one of the architectures with known Landlock syscall numbers
  (`x86_64`, `aarch64`, `riscv64`).

Feature detection:

- `src/sandbox/linux_landlock.rs` probes the Landlock ABI version via
  `LANDLOCK_CREATE_RULESET_VERSION`.
- When Landlock is unsupported (`ENOSYS` / unknown arch), it reports `LandlockStatus::Unsupported`.

---

## Debugging and validation: `sandbox_probe` (Linux)

`sandbox_probe` is a small CLI intended to answer:

- Does this kernel/environment support the sandbox layers?
- If sandbox setup fails, which layer failed, and why?
- After enabling the sandbox, are forbidden operations actually blocked?

Run (defaults: `--mode full`, `--probe all`):

```bash
timeout -k 10 60 bash scripts/run_limited.sh --as 2G -- \
  bash scripts/cargo_agent.sh run --release --bin sandbox_probe
```

Common usage patterns:

```bash
# Seccomp only (no Landlock).
bash scripts/cargo_agent.sh run --release --bin sandbox_probe -- --mode seccomp

# Landlock only (deny-all ruleset; useful to confirm the kernel supports Landlock at all).
bash scripts/cargo_agent.sh run --release --bin sandbox_probe -- --mode landlock

# Only run filesystem probes (skip network + exec).
bash scripts/cargo_agent.sh run --release --bin sandbox_probe -- --probe fs

# Same, but via env vars (clap `env=` hooks).
FASTRENDER_SANDBOX_MODE=seccomp FASTRENDER_SANDBOX_PROBE=fs \
  bash scripts/cargo_agent.sh run --release --bin sandbox_probe
```

`sandbox_probe` also honors the renderer sandbox env vars documented in [`docs/env-vars.md`](../env-vars.md),
notably:

- `FASTR_DISABLE_RENDERER_SANDBOX=1` (master off switch; insecure)
- `FASTR_RENDERER_SECCOMP=0|1`
- `FASTR_RENDERER_LANDLOCK=0|1`
- `FASTR_RENDERER_CLOSE_FDS=0|1`

Exit codes:

- `0`: sandbox behaved as expected for the chosen probes.
- `1`: a probe was unexpectedly allowed/blocked (possible sandbox regression).
- `2`: sandbox setup failed (or invalid sandbox env var).

Interpreting failures:

- `EPERM` installing seccomp often means you’re already inside an outer sandbox (container seccomp).
- If `sandbox_probe` prints “applied without TSYNC”, the kernel does not support TSYNC. This is not
  inherently a failure, but it means you must apply the sandbox **before** any threads are spawned
  to ensure the entire process is covered.
- A fatal `EINVAL` from seccomp installation generally means the kernel does not support the
  requested seccomp mode/flags (beyond the TSYNC fallback).
- Landlock `Unsupported` is normal on older kernels or when Landlock is not enabled in the kernel’s
  active LSM list (e.g. missing from `lsm=`); in `--mode full` we treat Landlock as best-effort and
  still apply seccomp.
- If the process dies with `SIGSYS` / “Bad system call”, the renderer hit a syscall that wasn’t in
  the allowlist. Use:
  - `scripts/trace_renderer_syscalls.sh` and the workflow in [seccomp_allowlist.md](../seccomp_allowlist.md)
  - kernel logs (`dmesg` / `journalctl -k`) if seccomp auditing is enabled

---

## macOS / Windows pointers

- Cross-platform sandbox overview: [sandboxing.md](../sandboxing.md)
- macOS renderer sandbox: [macos_renderer_sandbox.md](macos_renderer_sandbox.md) (more detail: [macos_sandbox.md](../macos_sandbox.md))
- Windows renderer sandbox: [windows_renderer_sandbox.md](windows_renderer_sandbox.md) (more detail: [windows_sandbox.md](../windows_sandbox.md))

TODO: expand this doc with a cross-platform “capability matrix” once the IPC transport choices
settle.
