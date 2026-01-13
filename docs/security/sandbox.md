# Renderer sandboxing (Linux-focused)

This doc is a **Linux deep-dive** companion to the cross-platform sandbox overview:
[sandboxing.md](../sandboxing.md).

It explains:

- the threat model for a sandboxed renderer process,
- the Linux sandbox **layering and ordering** (rlimits → fd hygiene → Landlock → seccomp),
- the current seccomp “hybrid allowlist + denylist” policy,
- and how to debug sandbox bring-up failures.

---

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

---

## Linux sandbox layering and order of operations

On Linux we intentionally layer multiple mechanisms. **Order matters** because later layers can
prevent the syscalls needed to set up earlier layers.

**Recommended order (Linux):**

1. **rlimits**
2. **close/lock down file descriptors**
3. **Landlock (filesystem, optional defense-in-depth)**
4. **seccomp-bpf (syscall filter)**

Repo reality:

- Landlock + seccomp are implemented in `src/sandbox/` and applied by
  `sandbox::apply_renderer_sandbox` (`src/sandbox/mod.rs`).
- rlimits and fd hygiene are expected to be handled by the renderer launcher (browser process) as
  part of process creation.

### 1) rlimits (resource guardrails)

Apply kernel-enforced rlimits early in renderer startup.

Common caps:

- `RLIMIT_AS` (virtual address space ceiling; hard memory cap)
- `RLIMIT_NOFILE` (fd cap)
- `RLIMIT_NPROC` (defense-in-depth against process spawning)
- `RLIMIT_CORE=0` (no core dumps)

Related: [`docs/resource-limits.md`](../resource-limits.md) and `src/process_limits.rs`.

### 2) Close file descriptors (no fd leaks)

Before sandboxing, ensure the renderer does not inherit privileged fds.

Policy:

- Keep only stdin/stdout/stderr (or explicit redirections) and the intended IPC fds.
- Close everything else (prefer `close_range` where available).
- Prefer `CLOEXEC` on IPC fds to prevent accidental leaks across exec boundaries.

### 3) Landlock (filesystem defense in depth)

Landlock is an LSM that enforces a **path-based** filesystem policy.

Implementation: `src/sandbox/linux_landlock.rs`.

Current default config is `deny_all` (no allowlisted paths). When unsupported, Landlock is treated as
best-effort and the sandbox proceeds with seccomp.

Why Landlock at all if seccomp blocks `openat`?

- Landlock still provides defense-in-depth if the seccomp policy evolves or a filesystem-related
  syscall slips through the filter.

### 4) seccomp-bpf (syscall filter: hybrid allowlist + denylist)

Implementation: `src/sandbox/linux_seccomp.rs`.

Key properties:

- Uses `PR_SET_NO_NEW_PRIVS` (required for unprivileged seccomp).
- Installs `SECCOMP_MODE_FILTER` via the `seccomp()` syscall with `SECCOMP_FILTER_FLAG_TSYNC` so the
  filter applies to all threads.
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
- **Network surface**: `connect`, `bind`, `listen`, `accept*`, `send*`, `recv*`, `set/get sockopt`
- **High-risk kernel APIs**: `ptrace`, `bpf`, `perf_event_open`, `kexec_load`,
  `process_vm_{readv,writev}`, `userfaultfd`, `keyctl`/`add_key`/`request_key`

Additionally, `socket(2)` is special-cased: `socket(AF_UNIX, ...)` is allowed for local IPC, while
other domains (`AF_INET`, `AF_INET6`, …) return `EPERM`.

For allowlist maintenance workflow (when the renderer legitimately needs more syscalls), see:
[seccomp_allowlist.md](../seccomp_allowlist.md) and `scripts/trace_renderer_syscalls.sh`.

---

## Kernel / CI requirements and feature detection (Linux)

### seccomp prerequisites / failure modes

- Kernel must support `seccomp-bpf` (`CONFIG_SECCOMP` + `CONFIG_SECCOMP_FILTER`).
- `SECCOMP_FILTER_FLAG_TSYNC` must be supported; older kernels may reject with `EINVAL`.
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

Run:

```bash
timeout -k 10 60 bash scripts/run_limited.sh --as 2G -- \
  bash scripts/cargo_agent.sh run --release --bin sandbox_probe
```

Interpreting failures:

- `EPERM` installing seccomp often means you’re already inside an outer sandbox (container seccomp).
- `EINVAL` installing seccomp often means the kernel is too old for TSYNC (or the flags are not
  supported).
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
