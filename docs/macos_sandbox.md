# macOS sandbox probe tool

FastRender uses macOS **Seatbelt** profiles to sandbox renderer processes. Iterating on those
profiles inside the full multiprocess browser stack can be slow.

`macos_sandbox_probe` is a small CLI binary that applies a renderer-style sandbox profile and then
tries a few “canary” operations (including IPC primitives) so you can quickly see what the sandbox
allows/denies.

## Run

This tool is **macOS-only**.

```bash
# From repo root (recommended for agent/CI-style environments)
bash scripts/cargo_agent.sh run --bin macos_sandbox_probe -- --mode strict

# Or, directly:
cargo run --bin macos_sandbox_probe -- --mode strict
```

### Network probe (recommended)

To make network denial obvious (instead of just seeing “connection refused”), run a local TCP
listener first:

```bash
python3 -m http.server 8000
```

Then, in another terminal:

```bash
cargo run --bin macos_sandbox_probe -- --mode strict --port 8000
```

The `connect to 127.0.0.1:8000` probe should report `DENIED`.

## Modes

- `--mode strict`
  - Intended to be the “locked down” profile: denies network, denies reading `/etc/passwd`, and
    denies writing under `temp_dir()`.
- `--mode relaxed`
  - Still denies network and denies reading `/etc/passwd`, but may allow more filesystem access for
    iteration.

## IPC capability matrix (Seatbelt)

The probe also attempts a few IPC primitives **after** applying the sandbox. This is intended to
inform the renderer<->browser IPC transport choice.

| Capability | Primitive | Strict profile expectation | Recommendation |
|---|---|---|---|
| Anonymous pipe | `pipe()` | **ALLOWED** | Safe default. Prefer inherited pipes (created by the browser before sandboxing) if a future profile denies in-sandbox creation. |
| Anonymous Unix domain socketpair | `UnixStream::pair()` (`socketpair`) | **ALLOWED** | Prefer for bidirectional framed IPC on Unix-y platforms. If denied under a future profile, create the socketpair in the parent before sandboxing the renderer. |
| Filesystem-backed Unix domain socket | `UnixListener::bind($TMPDIR/…)` | **DENIED** (filesystem write denied) | Avoid named UDS paths inside the renderer sandbox. Use inherited FDs (pipes/socketpair), or a macOS-specific transport (Mach/XPC) if needed. |

### Design implications

- Do **not** rely on creating/binding IPC endpoints that require filesystem access from inside the
  sandbox.
- Prefer **inherited** IPC endpoints created by the browser *before* sandboxing the renderer.
- Keep IPC explicit and minimal: a small number of long-lived channels, with the browser mediating
  all privileged operations (network, file reads, GPU, etc).

## Exit codes

- `0`: No “unexpectedly allowed” probes were observed.
- `1`: At least one probe that was expected to be denied succeeded.
- `2`: Sandbox failed to apply (or you ran it on a non-macOS host).

## Editing the profiles

The profiles currently live in `src/bin/macos_sandbox_probe/probe.rs`. This tool is intentionally
small so you can tweak the profile rules and re-run quickly.
