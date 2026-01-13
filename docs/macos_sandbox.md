# macOS Seatbelt sandboxing (Seatbelt vs App Sandbox)

FastRender is moving toward a **multiprocess** architecture where untrusted page content is rendered
in a separate OS-sandboxed **renderer process**.

This document is the macOS-specific sandbox guide. It covers:

- What **Seatbelt** sandboxing is (`sandbox_init(3)` / `/usr/bin/sandbox-exec`) and why it does **not**
  require App Sandbox entitlements or codesigning.
- Why launching through **`sandbox-exec`** can be safer than using
  `std::os::unix::process::CommandExt::pre_exec` from a multithreaded browser parent.
- The chosen strict baseline profile: **`pure-computation`**, and what it blocks (**network +
  filesystem**).
- Common failure modes (especially **fonts**) and the intended mitigations (bundled/preloaded
  fonts).
- Practical debugging tips for sandbox denials (`log stream ...`).
- How to run the macOS-only sandbox tests and the `macos_sandbox_probe` tool.

## Runtime configuration: `FASTR_RENDERER_SANDBOX`

Renderer processes can select the Seatbelt sandbox mode at startup via:

`FASTR_RENDERER_SANDBOX=strict|relaxed|off`

Legacy aliases: `FASTR_RENDERER_SANDBOX=1` = `strict`, `FASTR_RENDERER_SANDBOX=0` = `off`.

Note: the debug escape hatches `FASTR_DISABLE_RENDERER_SANDBOX=1` and `FASTR_MACOS_RENDERER_SANDBOX=off`
also disable Seatbelt sandboxing, overriding `FASTR_RENDERER_SANDBOX`.

`FASTR_MACOS_RENDERER_SANDBOX=pure-computation|system-fonts` can also override the strict/relaxed mode
selection while still keeping sandboxing enabled.

The canonical entrypoint is:

- `fastrender::sandbox::apply_macos_sandbox_from_env()` (returns `MacosSandboxStatus` so callers can
  choose fail-closed in production or best-effort in dev).

Mode mapping:

- `strict` (default on macOS renderer processes): applies `macos::MacosSandboxMode::PureComputation`
  (Seatbelt `pure-computation`, with an embedded fallback profile).
- `relaxed`: applies `macos::MacosSandboxMode::RendererSystemFonts` (still blocks network, but
  allows read-only access to system font/framework locations).
- `off`: do not apply a sandbox (**debugging only**).

Advanced override:

- `FASTR_RENDERER_MACOS_SEATBELT_PROFILE=pure-computation|no-internet|renderer-default|<path>` can be
  used to override the underlying Seatbelt profile when `FASTR_RENDERER_SANDBOX` enables sandboxing.
  This is intended for experimentation; it overrides the `strict`/`relaxed` profile mapping.

Recommended: leave unset in production (default `strict`), use `relaxed` only when you need system
fonts during bring-up, and use `off` only when debugging sandbox behavior.

## Seatbelt vs App Sandbox (what’s the difference?)

### Seatbelt (what we use for renderer isolation)

On macOS, the kernel sandbox mechanism is commonly referred to as **Seatbelt** (implemented by the
system “sandboxd” machinery + kernel enforcement). There are two common interfaces:

- **`sandbox_init(3)`** (C API in `libsandbox`): the process installs a sandbox profile in-process.
  - FastRender’s entry point: `fastrender::sandbox::macos::apply_pure_computation_sandbox()`
    (and `apply_renderer_sandbox` for other modes).
  - One-way: once applied, it cannot be reverted for the lifetime of the process.
- **`/usr/bin/sandbox-exec`** (CLI wrapper; **deprecated by Apple**): launches a program already inside a sandbox.
  - FastRender helpers:
    - `fastrender::sandbox::macos_spawn::sandbox_exec_command(...)` (convenience wrapper for the
      `pure-computation` profile).
    - `fastrender::sandbox::macos_spawn::wrap_command_with_sandbox_exec(...)` (wraps an existing
      `std::process::Command` by constructing a new sandbox-exec command:
      `sandbox-exec -D HOME=... -D TMPDIR=... -f <temp_profile_file> -- <exe> <args...>`
      so SBPL can reference common parameters via `(param "HOME")` / `(param "TMPDIR")`).
      - Opt-in gate: `FASTR_MACOS_USE_SANDBOX_EXEC=1` + `maybe_wrap_command_with_sandbox_exec(...)`.
      - Escape hatch: `FASTR_DISABLE_RENDERER_SANDBOX=1`, `FASTR_RENDERER_SANDBOX=off`, or
        `FASTR_MACOS_RENDERER_SANDBOX=off` makes
        the `sandbox-exec` helpers no-ops (launches unsandboxed; INSECURE).

Critically:

- **Seatbelt does not require App Sandbox entitlements.**
- **Seatbelt does not require codesigning.**

This makes it suitable for developer builds, CI, and non-`.app` binaries.

### App Sandbox (what we are *not* relying on here)

**App Sandbox** is the entitlement-based sandbox used for `.app` bundles (especially Mac App Store
apps). It requires code signing and entitlements like `com.apple.security.app-sandbox`.

FastRender’s renderer sandboxing does **not** depend on App Sandbox because we need an isolation
boundary that works for:

- local developer runs of binaries,
- CI runs, and
- future browser-like multiprocess spawning without requiring an app bundle/entitlements.

That said, when we eventually ship FastRender as a macOS `.app`, we may also apply App Sandbox
entitlements to the renderer helper process as an additional enforcement layer for end-user builds.
See [security/macos_renderer_sandbox.md](security/macos_renderer_sandbox.md) and the placeholder
entitlement files under [`tools/macos/entitlements/`](../tools/macos/entitlements/).

## Baseline policy: `pure-computation`

The **strict baseline** is the built-in Seatbelt profile **`pure-computation`**.

At a high level it blocks:

- **Network**: the renderer cannot create sockets or connect directly (even localhost).
- **Filesystem**: the renderer cannot read/write arbitrary paths (including system fonts).

In FastRender, `pure-computation` is applied via:

- `fastrender::sandbox::macos::apply_pure_computation_sandbox()` (calls `sandbox_init`).
- or, when spawning from a parent process, via `sandbox-exec` (see `macos_spawn`).

### Why a strict baseline?

Renderer isolation is only meaningful if the renderer cannot “reach out” to the host:

- Network must be brokered by a privileged process (browser / network process).
- Filesystem access must be brokered by a privileged process (browser) or avoided entirely.

The long-term goal is to make the renderer viable under `pure-computation` by:

- removing host dependencies (fonts, caches, config reads),
- using **bundled fonts** and other embedded assets, and
- passing resources via IPC (or pre-opened/inherited file descriptors where appropriate).

## `sandbox-exec` vs `CommandExt::pre_exec` (why it matters)

From a browser-like, multithreaded parent process, avoid applying Seatbelt via a Rust
`CommandExt::pre_exec` hook.

Reasoning:

- `pre_exec` runs **after `fork()` and before `exec()`**, inside the child.
- In a multithreaded parent, the child after `fork()` inherits the parent’s memory state with only
  one thread running; calling complex code can deadlock on locks held by other threads at the moment
  of `fork()` (malloc/stdio/Objective‑C runtime locks, etc).
- On macOS, `std::process::Command` generally prefers `posix_spawn` for safety/performance; using
  `pre_exec` often forces a fallback to `fork` + custom child setup (exactly the sharp edge we want
  to avoid in a browser process).

Using `sandbox-exec` (e.g. via `fastrender::sandbox::macos_spawn::sandbox_exec_command` or
`wrap_command_with_sandbox_exec`) avoids running arbitrary Rust code in the `fork` window and keeps
spawning behavior easier to reason about.

## Common failure modes under `pure-computation`

### Fonts (most common): system font discovery is denied

Symptoms:

- Sandbox denials like `deny file-read-data /System/Library/Fonts/...`.
- Missing glyphs/tofu or font loading failures.

Why:

- FastRender’s system font discovery uses `fontdb` and platform APIs which open system font files.
- `pure-computation` blocks filesystem reads, so system font discovery cannot work inside the
  sandbox.

Mitigations (preferred order):

1. **Use bundled fonts** inside the sandboxed renderer.
   - `FASTR_USE_BUNDLED_FONTS=1` forces bundled-only mode.
   - See [`docs/notes/bundled-fonts.md`](notes/bundled-fonts.md).
2. If you must use host fonts temporarily for debugging/bring-up:
   - preload fonts **before** applying the sandbox (in-process `sandbox_init` flow), or
   - use a relaxed renderer profile that allows read-only access to system font paths (see
     `MacosSandboxMode::RendererSystemFonts` in `src/sandbox/macos.rs`).

Avoid loosening the strict baseline long-term; that defeats renderer isolation.

### Renderer writes to disk (caches, traces, downloads)

`pure-computation` is intentionally hostile to disk I/O. If you see denials for file writes/creates,
the renderer is likely trying to write:

- disk cache files,
- trace output files, or
- anything under `temp_dir()`.

Mitigation: move the write into the privileged process and request it via IPC, or keep the renderer
fully memory-only.

## Developer overrides (debug only)

The macOS renderer sandbox is a security boundary. These knobs exist to unblock local debugging and
bring-up; do not rely on them in production.

- Disable Seatbelt sandboxing entirely (INSECURE):
  - `FASTR_DISABLE_RENDERER_SANDBOX=1`
- Select a different renderer Seatbelt profile:
  - `FASTR_MACOS_RENDERER_SANDBOX=pure-computation|system-fonts|off`
    - `system-fonts` maps to the relaxed `MacosSandboxMode::RendererSystemFonts` profile (allows
      read-only access to system font paths; still blocks network + user filesystem).
    - Aliases: `pure`/`strict` for `pure-computation`, `fonts`/`relaxed` for `system-fonts`.
- Opt into launching a renderer already sandboxed via Apple’s deprecated `sandbox-exec` wrapper
  (debug/legacy; see `src/sandbox/macos_spawn.rs`):
  - `FASTR_MACOS_USE_SANDBOX_EXEC=1`
  - Note: this is ignored when sandboxing is disabled via `FASTR_DISABLE_RENDERER_SANDBOX=1` or
    `FASTR_MACOS_RENDERER_SANDBOX=off`.

Renderer-focused quick reference + more examples: [`docs/security/macos_renderer_sandbox.md`](security/macos_renderer_sandbox.md).

## Debugging Seatbelt denials on macOS

Seatbelt denials are visible in macOS’s unified logging.

### Live stream (recommended)

```bash
# Replace <renderer-binary> with the process name (as shown in Activity Monitor).
log stream --style syslog --level debug --predicate \
  'subsystem == "com.apple.sandbox" && process == "<renderer-binary>"'
```

To filter to only denies:

```bash
log stream --style syslog --level debug --predicate \
  'subsystem == "com.apple.sandbox" && eventMessage CONTAINS[c] "deny"'
```

### Query recent denials

```bash
log show --last 5m --style syslog --predicate \
  'subsystem == "com.apple.sandbox" && eventMessage CONTAINS[c] "deny"'
```

Practical advice:

- The denial usually includes the operation (`file-read-data`, `network-outbound`, …) and the path.
- Prefer fixing the renderer to avoid the operation, rather than widening the sandbox.

## Tooling: `macos_sandbox_probe`

FastRender uses macOS Seatbelt profiles to sandbox renderer processes. Iterating on those profiles
inside the full multiprocess browser stack can be slow.

`macos_sandbox_probe` is a small CLI binary that applies a renderer-style sandbox profile and then
tries a few “canary” operations (including IPC primitives) so you can quickly see what the sandbox
allows/denies.

### Run

This tool is **macOS-only**.

```bash
# From repo root (wrapper-safe; no raw `cargo`).
timeout -k 10 600 bash scripts/cargo_agent.sh run --features macos_sandbox_probe --bin macos_sandbox_probe -- --mode strict
```

Tip: if you're iterating on Seatbelt profiles and want faster rebuilds, you can disable the crate's
default features during probe runs:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh run --no-default-features --features macos_sandbox_probe --bin macos_sandbox_probe -- --mode strict
```

### Network probe

By default the tool uses `--port 0`, which means it will bind an ephemeral local TCP listener
*before* applying the sandbox and then attempt to connect to it *after* applying the sandbox. This
makes network denial obvious (a non-sandboxed process would succeed, but the sandboxed connect
should report `DENIED`).

If you pass a specific `--port`, the tool will attempt to connect to that port directly. If nothing
is listening there you may see `connection refused` instead of a sandbox permission error.

### Modes

- `--mode strict`
  - Intended to be the “locked down” profile for iteration: denies network, denies reading
    `/etc/passwd`, and denies writing under `temp_dir()`.
- `--mode relaxed`
  - Still denies network and denies reading `/etc/passwd`, but may allow more filesystem access for
    iteration.
- `--mode pure-computation`
  - Applies Apple’s built-in `pure-computation` Seatbelt profile (very strict).
  - This is the closest quick approximation to a “renderer can only compute” sandbox.
  - Some macOS versions do not ship the named profile (or treat it as invalid). In that case, the
    probe falls back to an embedded SBPL profile that denies filesystem + network.

Note: on macOS, `/etc` is typically a symlink into `/private/etc` (and similarly `/var` → `/private/var`).
The probe’s built-in Seatbelt profiles deny both the public and `/private/*` paths so the results are
stable across hosts.

### IPC capability matrix (Seatbelt)

The probe attempts a few IPC primitives **after** applying the sandbox. This is intended to inform
the renderer↔browser IPC transport choice.

The output distinguishes between:

- **create after sandbox**: what the sandboxed process can create on its own
- **created before sandbox**: what remains usable if the browser creates IPC endpoints and passes
  them into the renderer (typical multiprocess model)

| Capability | Primitive | Probe output (mode=strict) | Recommendation |
|---|---|---|---|
| Anonymous pipe | `pipe()` | See `ipc: pipe()` lines (both “create after sandbox” and “created before sandbox”) | Safe default. Prefer inherited pipes (created by the browser before sandboxing) if in-sandbox creation is denied. |
| Anonymous Unix domain socketpair | `UnixStream::pair()` (`socketpair`) | See `ipc: unix socketpair` lines (both “create after sandbox” and “created before sandbox”) | Prefer for bidirectional framed IPC on Unix-y platforms. If in-sandbox creation is denied, create the socketpair in the parent before sandboxing the renderer (and rely on the inherited FD). |
| Filesystem-backed Unix domain socket | `UnixListener::bind($TMPDIR/…)` | **DENIED** (requires filesystem write) | Avoid named UDS paths inside the renderer sandbox. Use inherited FDs (pipes/socketpair), or a macOS-specific transport (Mach/XPC) if needed. |

#### POSIX shared memory (`shm_open`) + `mmap` (Seatbelt)

FastRender’s planned “shared memory pixel buffer” IPC design depends on whether the renderer
sandbox can:

1. Create new POSIX shared memory objects (`shm_open` + `ftruncate` + `mmap`) *after* sandboxing.
2. `mmap` an **inherited** shared memory fd (opened before sandbox activation).

The probe prints `ALLOWED` / `DENIED` and the relevant `errno` for both cases.

##### Results (pure-computation)

On macOS with the built-in Seatbelt profile `pure-computation`:

- Creating new POSIX shmem after sandbox (`shm_open` + `ftruncate` + `mmap`): **DENIED**
- Mapping an inherited shmem fd after sandbox (`mmap` on an fd opened pre-sandbox): **ALLOWED**

##### Recommendation (pixel buffer IPC)

If `shm_open` is **DENIED** under `pure-computation`, but mapping an inherited fd is **ALLOWED**, the
recommended design is:

> **Browser creates the POSIX shared memory object and passes the fd to the renderer; the renderer
> only `mmap`s the inherited fd.**

This avoids needing “create global named objects” privileges in the renderer sandbox while still
allowing a shared pixel buffer.

### Design implications

- Do **not** rely on creating/binding IPC endpoints that require filesystem access from inside the
  sandbox.
- Prefer **inherited** IPC endpoints created by the browser *before* sandboxing the renderer.
- Keep IPC explicit and minimal: a small number of long-lived channels, with the browser mediating
  all privileged operations (network, file reads, GPU, etc).

### Exit codes

- `0`: No “unexpectedly allowed” probes were observed.
- `1`: At least one probe that was expected to be denied succeeded.
- `2`: Sandbox failed to apply.

### Editing the profiles

The profiles currently live in `src/bin/macos_sandbox_probe/probe.rs`. This tool is intentionally small
so you can tweak the profile rules and re-run quickly.

## Running the macOS-only sandbox tests

### In-process `sandbox_init` tests (default)

The core Seatbelt tests live in `src/sandbox/macos.rs` and run on macOS as normal unit tests.

```bash
# If your environment provides GNU `timeout`:
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender sandbox::macos -- --nocapture
```

FastRender also has an integration test that asserts the **relaxed renderer sandbox** still permits
system font discovery via `fontdb` (important for early bring-up when bundled fonts are not used):

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration sandbox::macos_sandbox_fontdb -- --nocapture
```

FastRender additionally ships macOS-only integration tests that exercise the renderer sandbox
profiles and spawn paths directly:

```bash
# Strict/relaxed Seatbelt profile enforcement (`sandbox_init`).
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration sandbox::macos_renderer_sandbox -- --nocapture

# `sandbox-exec` wrapper enforcement (sandbox applied before libtest starts).
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration sandbox::macos_sandbox_exec -- --nocapture

# Render smoke test: strict Seatbelt sandbox + bundled-only fonts + offline fetcher.
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration sandbox::macos_seatbelt_render_smoke -- --nocapture
```

If your macOS environment does not have `timeout`, either install coreutils (`brew install
coreutils`, then use `gtimeout`) or run without the outer timeout wrapper.

### `sandbox-exec` launcher tests

FastRender has a small unit test in `src/sandbox/macos_spawn.rs` that uses the `sandbox-exec`
wrapper to deny network binding in a child process. It will automatically **skip** if
`/usr/bin/sandbox-exec` is missing (it is deprecated by Apple and may not exist on future macOS
releases).

Run it explicitly on macOS with:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender sandbox_exec_blocks_network_bind -- --nocapture
```
