# Maintaining the renderer seccomp allowlist (Linux)

FastRender's sandboxed renderer aims to run with a *strict* `seccomp-bpf` filter on Linux: only a
small set of syscalls are permitted once the sandbox is active.

This doc describes a practical workflow for deriving/maintaining that allowlist as the renderer
evolves.

Related:
- Linux IPC checklist (shared memory + FD passing): [`docs/ipc_linux_fd_passing.md`](ipc_linux_fd_passing.md)

## Trace syscalls locally

Prereqs:
- Linux (seccomp + `strace` workflow is Linux-focused)
- `strace` installed (`sudo apt install strace`, `sudo dnf install strace`, etc.)

Run the tracing script from the repo root:

```bash
bash scripts/trace_renderer_syscalls.sh
```

This will:
- Build the chosen renderer workload **outside** of `strace` (so cargo/rustc syscalls are not in the trace)
- Run the workload under `strace -f` (follow forks/threads)
- Use the repo safety wrappers (`timeout -k 10 ...` and `scripts/run_limited.sh`)
- Emit a **sorted, unique** syscall list (one per line)

Outputs:
- Raw `strace` log: `target/seccomp/renderer.strace`
- Syscall set: `target/seccomp/renderer_syscalls.txt` (also printed to stdout)

### Customizing the workload

By default the script traces `render_fixtures` on a small offline fixture. You can pass arguments to
the traced binary after `--`:

```bash
# Trace a heavier fixture (or a set of fixtures).
bash scripts/trace_renderer_syscalls.sh -- --fixtures go.dev,amazon.com --jobs 1

# Add timestamps to help correlate “before/after sandbox installed” in the raw log.
STRACE_FLAGS="-ttt" bash scripts/trace_renderer_syscalls.sh
```

## Mapping traced syscalls into the BPF allowlist

`strace` outputs syscall **names** (e.g. `openat`, `futex`, `clock_gettime`). The seccomp filter
usually needs syscall **numbers**, but in Rust you typically express them as `libc::SYS_*` constants.

### IPC note: `SCM_RIGHTS` needs `sendmsg`/`recvmsg`

If the renderer uses UNIX-domain sockets with FD passing (`SCM_RIGHTS`), expect to see `sendmsg` and
`recvmsg` in the post-sandbox syscall set. (FD passing is per-message and uses `sendmsg(2)` /
`recvmsg(2)`.)

See [`docs/ipc_linux_fd_passing.md`](ipc_linux_fd_passing.md) for the IPC-side checklist and
footguns.

Workflow:
1. Run the trace script to get a syscall set.
2. Locate the renderer’s seccomp filter / allowlist in the codebase:
    - `rg "SECCOMP_RET_ALLOW" -n src`
   - `rg "ALLOWED_SYSCALLS" -n src`
3. For each syscall in `target/seccomp/renderer_syscalls.txt`:
   - If it’s required **after the sandbox is installed**, add it to the allowlist (prefer using
     `libc::SYS_<name>` constants and keep the list sorted).
   - If it only happens during **startup**, do *not* add it; instead move the work to happen before
     the sandbox is applied (see next section).
4. Re-run the traced workload until it completes without triggering seccomp violations.

When a syscall name doesn’t trivially map to a `libc::SYS_*` constant, check:
- `man 2 <syscall>` (e.g. `man 2 openat`)
- Linux syscall tables (`/usr/include/asm/unistd_64.h` on many distros)

## Startup syscalls vs post-sandbox syscalls

`strace` will show **everything** the process does, including dynamic linker + glibc startup:
- ELF loader mapping shared libraries (`openat`, `mmap`, `mprotect`, `close`, …)
- Thread/runtime setup (`set_tid_address`, `set_robust_list`, `rseq`, …)
- Randomness and time setup (`getrandom`, `clock_gettime`, …)

In the renderer, the seccomp sandbox should be applied **after startup**:
- do initialization (load libraries, set up IPC, open any required resources) first
- then install the seccomp filter
- only the “steady state” renderer loop runs under the strict allowlist

Implications:
- Don’t mechanically add every syscall you see in a full-process `strace` to the allowlist.
- If a syscall appears *after* the sandbox is installed, treat it as a real allowlist candidate or a
  signal that more initialization needs to move earlier.
- If you need to correlate the raw `strace` log with the “sandbox installed” moment, re-run with
  timestamps (`STRACE_FLAGS="-ttt"`), then compare timestamps with the renderer’s own logs.
