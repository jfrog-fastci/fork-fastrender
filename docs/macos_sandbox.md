# macOS sandbox probe tool

FastRender uses macOS Seatbelt profiles to sandbox renderer processes. Iterating on those profiles
inside the full multiprocess browser stack can be slow.

`macos_sandbox_probe` is a tiny CLI binary that applies a renderer-style sandbox profile and then
tries a few “canary” operations so you can quickly see what the sandbox allows/denies.

## Run

This tool is **macOS-only**.

```bash
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

## Exit codes

- `0`: No “unexpectedly allowed” probes were observed.
- `1`: At least one probe that was expected to be denied succeeded.
- `2`: Sandbox failed to apply (or you ran it on a non-macOS host).

## Editing the profiles

The profiles currently live in `src/bin/macos_sandbox_probe/probe.rs`. This tool is intentionally
small so you can tweak the profile rules and re-run quickly.
