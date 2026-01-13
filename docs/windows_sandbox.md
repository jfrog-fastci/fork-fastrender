# Windows renderer sandbox (Job objects + AppContainer + restricted token)

This doc describes the **current intended sandbox boundary** for the Windows *renderer* process.
It is written to prevent “small refactors” from accidentally weakening isolation.

Status / repo reality (today):

- The windowed `browser` UI is still largely single-process. The Windows sandbox helpers are used
  primarily by **tests/tooling** today and are intended to become the standard spawn path once the
  renderer runs as a separate OS process.
- This doc is written as if the renderer were already a separate compromised process, because that
  is the security boundary we are building toward.

Code map (repo reality):

- High-level renderer spawn sandboxing:
  - `src/sandbox/windows.rs` (`fastrender::sandbox::windows::spawn_sandboxed(...)`)
  - `src/sandbox/windows/appcontainer.rs` (dynamic loader for AppContainer APIs in `userenv.dll`)
    - We resolve AppContainer APIs at runtime (via `LoadLibraryExW(..., LOAD_LIBRARY_SEARCH_SYSTEM32)`
      + `GetProcAddress`) so the binary can still load on Windows versions that lack these exports;
      missing symbols are treated as “no AppContainer support”.
      - `spawn_sandboxed(...)` **fails closed by default** (to avoid silent sandbox downgrades).
      - Set `FASTR_ALLOW_UNSANDBOXED_RENDERER=1` to opt in to restricted-token / unsandboxed fallback.
- Reusable Win32 wrappers + reusable spawner + tests:
  - `crates/win-sandbox/`
    - `Job` (job object wrapper + limits)
    - AppContainer SID helpers (`AppContainerProfile`, `derive_appcontainer_sid`)
    - Restricted-token fallback builder (`RestrictedToken`)
    - `CreateProcessW` spawner (`spawn_sandboxed`, `SpawnConfig`) → `ChildProcess`
      - Can attach an AppContainer token (no capabilities) via `SECURITY_CAPABILITIES` when the
        caller provides an `AppContainerProfile`.
      - Can attach a Job object via `PROC_THREAD_ATTRIBUTE_JOB_LIST` when the caller provides a
        `Job` (the caller configures job limits separately).
      - Note: if `SpawnConfig.current_dir` is `None`, Windows inherits the parent process CWD
        (`lpCurrentDirectory = NULL`). When spawning with an AppContainer token, that inherited
        directory may be inaccessible and can cause surprising startup failures (or break code that
        uses relative paths). Callers should set an explicit, sandbox-accessible working directory
        when using this low-level helper.
      - Supports handle inheritance allowlisting (`inherit_handles` /
        `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`).
      - When `SpawnConfig.job` is set and the parent process is already inside a Job, the spawner
        attempts `CREATE_BREAKAWAY_FROM_JOB` first, then retries without breakaway on
        `ERROR_ACCESS_DENIED` (best-effort). It does **not** provide an automatic “jobless” mode.
      - Can apply a mitigation policy bitmask (`mitigation_policy`) via
        `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` (escape hatch: `FASTR_DISABLE_WIN_MITIGATIONS=1`).
        - Best-effort compatibility: if the OS rejects the mitigation attribute (e.g.
          `ERROR_INVALID_PARAMETER` / `ERROR_NOT_SUPPORTED`), the spawner retries without mitigations
          rather than failing process creation.
      - Can remove the broad `ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) from the created
        AppContainer token via `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`
        (`SpawnConfig::all_application_packages_hardened`, best-effort / retried without on
        unsupported Windows builds).
    - Restricted-token `CreateProcessAsUserW` spawner (`restricted_token::spawn_with_token`)
      - Uses a low-integrity restricted primary token (from `RestrictedToken`).
      - Also supports job/handle allowlisting and mitigation policies via `STARTUPINFOEX`.
      - When `SpawnConfig.job` is set and the parent process is already inside a Job, the spawner
        attempts `CREATE_BREAKAWAY_FROM_JOB` first, then retries without breakaway on
        `ERROR_ACCESS_DENIED` (best-effort). It does **not** provide an automatic “jobless” mode.
      - Mitigation policies are best-effort: if the OS rejects
        `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`, the spawner retries without mitigations.
    - High-level renderer sandbox wrapper (`win_sandbox::renderer::RendererSandbox`)
      - Sets up a Job object (kill-on-close + active-process limit; optional memory cap) and (when
        supported) an AppContainer profile with zero capabilities, then spawns the child suspended,
        assigns it to the job, and resumes it.
      - Attempts to remove the broad `ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) from the
        AppContainer token via `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (best-effort /
        retried without on unsupported Windows builds).
      - Used primarily by `crates/win-sandbox` tests/tooling; it does **not** include `fastrender`’s
        environment sanitization or executable relocation workarounds.
    - AppContainer-only convenience spawner (`win_sandbox::RendererSandbox`)
      - Spawns in a no-capabilities AppContainer and allowlists stdio handles.
      - Includes dev/CI executable relocation + ACL fixing for `ERROR_ACCESS_DENIED`.
      - Attempts to remove the broad `ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) from the
        AppContainer token via `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (best-effort /
        retried without on unsupported Windows builds).
      - Applies the default renderer mitigation policy via `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`
        (best-effort; escape hatch: `FASTR_DISABLE_WIN_MITIGATIONS=1`).
      - Does **not** assign the child to a Job object (callers must do that separately).
    - Manual sandbox probe example (`crates/win-sandbox/examples/probe.rs`)
      - `cargo run -p win-sandbox --example probe -- --connect-localhost`
      - Spawns a sandboxed copy of itself and prints observed sandbox state from inside the child
        (AppContainer + integrity level + Job membership + selected mitigations), with optional
        filesystem/network probes for quick regression triage.
    - Mitigation policy builder + verifier (`mitigations::*`)
    - Capability detection helpers (`support::*`, `SandboxSupport`) and opt-in policy wrapper
      (`RendererSandboxMode`) used to avoid silent sandbox downgrades on unsupported hosts.

Rule of thumb:

- Use `fastrender::sandbox::windows::spawn_sandboxed(...)` when you want the **full renderer spawn
  sandbox** (AppContainer/restricted token + Job + handle allowlisting).
- Use `win_sandbox::spawn_sandboxed(&SpawnConfig)` when you want a **reusable `CreateProcessW` spawner**
  that can apply AppContainer/Job/handle-allowlist and (optionally) mitigations, but you do **not**
  need the extra `fastrender`-specific behavior in `src/sandbox/windows.rs` (notably env sanitization
  and AppContainer executable relocation/current-dir workarounds).
- Use `win_sandbox::RestrictedToken` + `win_sandbox::restricted_token::spawn_with_token(...)` when
  you specifically want a restricted-token (Low IL) child process.
- Use `win_sandbox::renderer::RendererSandbox` when you want a small, reusable “renderer-like” spawn
  wrapper (Job + AppContainer when available + mitigations), and you are okay with its simpler
  behavior (no env sanitization; no AppContainer executable relocation).
- Use `win_sandbox::RendererSandbox` when you specifically want “AppContainer-only spawn + stdio
  allowlist + exe relocation + default mitigations (best-effort)” and will handle job assignment separately.

Related docs:

- Sandboxing overview (cross-platform): [sandboxing.md](sandboxing.md)
- IPC safety invariants (framing, shared memory): [ipc.md](ipc.md)
- Multiprocess threat model (what the sandbox is protecting): [multiprocess_threat_model.md](multiprocess_threat_model.md)

## Threat model (renderer on Windows)

Treat the renderer process as **attacker-controlled**:

- It parses and executes untrusted HTML/CSS/JS (and decodes untrusted images/fonts/media).
- Assume a memory-safety bug, logic bug, or JIT bug yields **arbitrary native code execution** inside
  the renderer process.

The sandbox must make the following *impossible or dramatically harder*:

- **Filesystem access:** reading user files (cookies, history DB, documents), writing persistence, or
  modifying the install dir.
- **Network access:** direct outbound requests bypassing our network policy / cookie jar / CORS /
  request logging.
- **Cross-process attacks:** opening handles to the broker (browser) process, injecting threads/DLLs,
  or using inherited handles to escape confinement.
- **Process tree abuse:** spawning child processes for persistence or to reach unsandboxed
  components.

Non-goals (handled elsewhere, or inherently “hard” to fully prevent):

- CPU DoS / memory DoS (we mitigate with Job limits, but “make the renderer fast” is not a security
  boundary).
- Side channels (timing/Spectre class issues).

## High-level architecture (Windows)

The Windows renderer sandbox is **defense-in-depth**, layered as:

1. **AppContainer token** (preferred): no capabilities → blocks network + most filesystem/registry.
2. **Job object limits**: lifetime + process-count (and optional memory ceiling, when enabled).
3. **CreateProcess handle allowlist**: only explicitly-approved handles are inherited.
4. **Process mitigation policies**: reduce kernel / Win32 API attack surface.
5. **Fallback mode** if AppContainer cannot be used: restricted token + Low IL (weaker; see below).

### Windows version / environment constraints

- **AppContainer requires Windows 8+** (and `userenv.dll` must export the AppContainer profile APIs).
  On older Windows versions, or unusual Windows Server configurations where the APIs are absent,
  we treat AppContainer as unsupported and **fail closed by default**.
  - Some hardened Windows environments expose the AppContainer exports but still refuse to create or
    ensure profiles (e.g. `CreateAppContainerProfile` fails due to system policy). Treat this the
    same way: AppContainer is effectively unavailable, so spawns fail closed unless explicitly opted
    into fallback mode.
  - For developer convenience, set `FASTR_ALLOW_UNSANDBOXED_RENDERER=1` to opt in to falling back to
    the restricted-token (or unsandboxed) spawn path.
- **Nested jobs** (assigning a process to a new Job when the parent is already in one) are
  generally supported on Windows 8+. If the parent is in a Job that disallows breakaway / nested-job
  assignment, sandbox setup may fail.
  - We treat job-assignment failures as a sandbox failure (fail closed by default); set
    `FASTR_ALLOW_UNSANDBOXED_RENDERER=1` to opt in to running without full job containment.

The *broker* (browser process) is trusted and is responsible for:

- Deciding what the renderer is allowed to do.
- Owning privileged resources (disk/network).
- Spawning the renderer with a restricted security configuration.

The renderer should only have:

- CPU + memory for computation.
- A small set of **IPC handles** back to the broker (and nothing else).

## AppContainer mode (preferred)

### Why AppContainer

We use an **AppContainer** process with **zero capabilities** because it is the only practical,
OS-supported way to block:

- **Outbound network** in a way that’s robust against “clever Win32 usage”.
- **Filesystem / registry access** beyond what is explicitly allowed.

In particular:

- An AppContainer with *no* `internetClient` / `internetClientServer` capability cannot initiate
  normal network connections. This prevents the renderer from bypassing our network stack.
- The AppContainer token has a drastically reduced default DACL access compared to a normal user
  token. The renderer cannot “just open” user profile files, named pipes, or shared objects unless
  they are explicitly accessible.

### Policy: no capabilities

We intentionally create the renderer AppContainer with **zero capabilities** (see
`src/sandbox/windows.rs`):

- `SECURITY_CAPABILITIES.CapabilityCount = 0`
- `SECURITY_CAPABILITIES.Capabilities = NULL`

This means:

- **No network capabilities** (including no loopback access).
- No opt-in device or library capabilities.
- We are **not** using LPAC or capability-based allowlists yet; the current design is “pure
  computation” and relies on brokered IPC for anything privileged.

If a future change wants to add a capability, it must be treated as a security-sensitive change
(and this doc must be updated).

### How AppContainer is applied (process creation)

We create the renderer as an AppContainer process by calling `CreateProcessW` with `STARTUPINFOEXW`
and setting:

- `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` → a `SECURITY_CAPABILITIES` struct containing:
  - `AppContainerSid = <derived AppContainer SID>`
  - `Capabilities = NULL`, `CapabilityCount = 0` (no capabilities)
- `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (best-effort) →
  `PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK`
  - Removes the broad `ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) from the created
    AppContainer token.
  - Some system objects are ACL'd to `ALL APPLICATION PACKAGES`; removing the group reduces ambient
    access for the renderer.
  - Compatibility: older Windows builds may reject this attribute (`ERROR_NOT_SUPPORTED` /
    `ERROR_INVALID_PARAMETER`). The spawner retries without it.
  - This hardening is enabled by default for the main `fastrender` Windows renderer spawn helper
    (`SpawnConfig::default()` sets `all_application_packages_hardened = true`).

This is implemented in `src/sandbox/windows.rs::spawn_appcontainer`.

### AppContainer identity (profile + SID)

An AppContainer process runs under an **AppContainer SID**.

Repo reality:

- `src/sandbox/windows.rs` uses a fixed AppContainer name (`"FastRender.Renderer"`) and derives the
  SID at spawn time.
- `crates/win-sandbox` provides higher-level helpers:
  - `AppContainerProfile::ensure(name, display_name, description)` (idempotent; treats
    `ERROR_ALREADY_EXISTS` as success)
  - `derive_appcontainer_sid(name)`

Notes:

  - AppContainer APIs are in `userenv.dll` and are resolved at runtime (see code map above). If the
    APIs are missing, AppContainer is treated as unsupported.
    - `src/sandbox/windows.rs` **fails closed by default** (returns an error so we don’t silently run
      without the intended sandbox). Set `FASTR_ALLOW_UNSANDBOXED_RENDERER=1` to explicitly opt in to
      falling back to restricted-token (or unsandboxed) spawning.
    - `crates/win-sandbox` exposes a similar opt-in policy helper (`RendererSandboxMode`) used to avoid
      silent sandbox downgrades on unsupported hosts.
- Creating the profile is a one-time system registration; the profile persists on the machine. We
  intentionally use a stable name so we do not create many profiles over time.

### Filesystem expectations

With no capabilities, the renderer should assume:

- It cannot read arbitrary files (including user profile, downloads, etc.).
- Any data needed from disk must be **brokered** (the broker opens it and streams it over IPC).

If something “needs filesystem access”, the correct design is generally:

- broker opens file → validates policy → passes bytes / a limited read handle (if absolutely
  necessary).

## Job object limits (always applied)

Even with AppContainer, we apply a Job object so the broker can reliably control the renderer’s
lifetime and resource usage.

### Spawn sequencing: create suspended → assign to Job → resume

For stronger “starts sandboxed” semantics, `src/sandbox/windows.rs` creates the child process with
`CREATE_SUSPENDED`, assigns it to the Job, then resumes the main thread via `ResumeThread`.

If `ResumeThread` fails, the code terminates the child process to avoid leaving a partially
configured process around.

### Limits we set

We configure the renderer Job object with `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` to enforce (see
`src/sandbox/windows.rs::JobObject::apply_limits` and `crates/win-sandbox/src/job.rs`):

- **Kill-on-close:** `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
  - If the broker exits/crashes, the renderer is not left behind.
  - This prevents “orphaned sandbox processes” from accumulating or persisting.

- **Active process limit:** `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (limit = 1)
  - The renderer cannot spawn child processes.
  - This is important even if the token is restricted: child processes complicate containment and
    can become an escape vector if they end up outside the intended security config.

- **UI restrictions:** `JOBOBJECT_BASIC_UI_RESTRICTIONS`
  - Disables a set of UI capabilities (clipboard, global atoms, display settings, etc.) appropriate
    for a headless renderer.
  - In `src/sandbox/windows.rs` this is “best-effort” (failure is ignored); in `crates/win-sandbox`,
    `Job::set_ui_restrictions_headless()` returns an error on failure.

- **Optional memory cap:** `JOB_OBJECT_LIMIT_JOB_MEMORY` / `JOBOBJECT_EXTENDED_LIMIT_INFORMATION::JobMemoryLimit`
  - Supported by `crates/win-sandbox` (`Job::set_job_memory_limit_bytes`).
  - **Not currently set** by `src/sandbox/windows.rs::spawn_sandboxed`.
  - `crates/win-sandbox` test/tooling helpers (`win_sandbox::renderer::RendererSandbox::new_default()` /
    `RendererSandboxBuilder`, `win-sandbox --example probe`) apply this limit when
    `FASTR_RENDERER_JOB_MEM_LIMIT_MB` is set (use `0`/unset to disable).
  - Semantics: limits total *committed* memory for the entire Job (not RSS; not per-process).
  - Tradeoff: if set too low, legitimate pages may hit the limit and fail allocations.

Notes:

- The Job is owned by the broker. The renderer should never be given a handle to the Job object.
- The process-count limit is a security boundary, not just “resource management”.
- **Do not allow breakaway.** The job must not be configured with `JOB_OBJECT_LIMIT_BREAKAWAY_OK` or
  `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`. The `crates/win-sandbox::Job` wrapper defensively clears
  these flags when updating extended limits.

### Important edge case: parent already in a Job

If the parent process is already running inside a Windows Job (common in CI/supervisors):

- `src/sandbox/windows.rs::spawn_sandboxed` and `win_sandbox::renderer::RendererSandbox` may try
  `CREATE_BREAKAWAY_FROM_JOB` first, then retry without breakaway on `ERROR_ACCESS_DENIED`.
- The lower-level `win_sandbox::spawn_sandboxed` and `win_sandbox::restricted_token::spawn_with_token`
  helpers also implement this retry behavior when a `Job` is requested (`SpawnConfig.job != None`).
- Assigning the child to our new Job can still fail (nested jobs/breakaway restrictions).
  - **Default:** fail closed. The spawner terminates the child process and returns an error (so we
    don’t silently lose `kill-on-close` / active-process limits).
  - **Opt-in:** if `FASTR_ALLOW_UNSANDBOXED_RENDERER=1` is set, the spawner may continue *jobless*
    (`SandboxedChild.job == None`) and prints a warning: **kill-on-close + active process limit are
    not enforced**.

Note: the low-level `win_sandbox::{spawn_sandboxed, restricted_token::spawn_with_token}` helpers
implement the same `CREATE_BREAKAWAY_FROM_JOB` retry strategy as the higher-level spawners *when*
`SpawnConfig.job` is set (try breakaway first, retry without on `ERROR_ACCESS_DENIED`). They still do
**not** provide a “jobless” fallback mode: only the higher-level spawners (`src/sandbox/windows.rs`
and `win_sandbox::renderer::RendererSandbox`) allow continuing without job containment when
explicitly opted in via `FASTR_ALLOW_UNSANDBOXED_RENDERER=1`.

## Handle inheritance allowlisting (critical)

On Windows, a sandbox boundary can be defeated if the child inherits **powerful handles** from the
broker (even if the child runs with a restricted token).

Key idea: **handles are capabilities**.

- AppContainer / restricted tokens mostly control what the process can do *when opening new
  resources*.
- If the broker hands the renderer an already-open handle (file, process, pipe, section), the
  renderer can usually use it with whatever access rights the handle was created with.

Examples of dangerous inherited handles:

- File handles (read/write access to arbitrary files the broker opened).
- Process/thread handles (ability to inject into the broker or other processes).
- Named pipe / ALPC / section handles that grant capabilities.
- Job handles (ability to escape limits or manipulate the job).

### The rule

The renderer must inherit **only** the minimal set of handles needed for IPC.

### How we enforce it

We use extended startup info with:

- `STARTUPINFOEXW`
- `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`

This is an *explicit allowlist* of inheritable handles:

- The broker duplicates/creates the IPC handles it wants the renderer to have.
- Process creation sets `bInheritHandles = TRUE` **but** provides a handle list, so only those
  handles can cross the boundary.
- When no handles are needed, `bInheritHandles = FALSE` so there is no “ambient inheritance”.

Note: handles still need to be created/marked inheritable (e.g. `bInheritHandle=TRUE` at creation,
or `SetHandleInformation(HANDLE_FLAG_INHERIT, ...)`). The allowlist prevents *extra* inheritable
handles from leaking into the sandboxed child.

Practical example: if the broker opens a sensitive file (cookies DB, bookmarks, arbitrary user
documents) and accidentally leaves the handle inheritable, then without a handle allowlist the
renderer may inherit it and read/write it **despite** being in an AppContainer.

Why this matters:

- Relying on “make everything non-inheritable” is brittle: a new feature might accidentally create
  an inheritable handle and silently leak it.
- `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` makes leakage **fail-closed**: if you forget to add the new
  handle to the list, the renderer simply won’t receive it.

When adding a new IPC primitive (shared memory, events, pipes):

1. Ensure it is safe for the renderer to hold.
2. Add it to the allowlist used at process creation.
3. Assume any omitted handle is *not inherited* (by design).

## Process mitigation policies

In addition to token restrictions, we support Win32 process mitigations at creation time using
`PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`.

In repo reality today:

- `crates/win-sandbox/src/mitigations.rs` defines the renderer mitigation bitmask
  (`mitigations::renderer_mitigation_policy()`).
- `crates/win-sandbox/src/spawn.rs` can apply it during process creation.
- Mitigations are **opt-in per spawn** for `win_sandbox::spawn_sandboxed`: `SpawnConfig::mitigation_policy`
  is `None` by default (disabled) unless the caller sets it.
- When `SpawnConfig::mitigation_policy` is set, both `win_sandbox::spawn_sandboxed` and
  `win_sandbox::restricted_token::spawn_with_token` treat the mitigation attribute as
  **best-effort**: if the OS rejects `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` with
  `ERROR_INVALID_PARAMETER` / `ERROR_NOT_SUPPORTED`, they retry process creation without mitigations
  instead of failing the spawn.
- Escape hatch: `FASTR_DISABLE_WIN_MITIGATIONS=1` disables **mitigation policies only** (useful for
  debugging/compatibility).

`src/sandbox/windows.rs::spawn_sandboxed` applies these mitigation policies at process creation time
(best-effort). If the OS rejects the mitigation attribute (e.g. older Windows builds), the spawn
logic retries without mitigations instead of failing process creation.

`win_sandbox::renderer::RendererSandbox` (the higher-level Job+AppContainer wrapper) also applies the
default renderer mitigation policy by default (best-effort; retries without mitigations if the OS
rejects the attribute).

`crates/win-sandbox::RendererSandbox` also applies the default mitigation policy at process creation
time (best-effort) when spawning its AppContainer-only child.

These mitigations reduce the renderer’s attack surface against:

- the kernel
- win32k (GUI/GDI) system call layer
- dynamic code injection techniques
- DLL search order hijacks / image load tricks

Mitigations we enable (when supported by the OS):

| Mitigation | Why we want it | Tradeoffs / compatibility |
|-----------|-----------------|---------------------------|
| **Win32k lockdown** (`WIN32K_SYSTEM_CALL_DISABLE`) | Removes a historically bug-dense kernel attack surface (USER/GDI). | Breaks anything that requires USER32/GDI syscalls. Renderer must stay headless (no windows, no GDI text paths). |
| **Disable extension points** | Blocks legacy “extension point” injection mechanisms. | Rare compatibility issues with injected DLL ecosystems (IME/hooks) — acceptable for sandbox. |
| **Prohibit dynamic code** | Prevents RWX / JIT-style codegen as an exploitation primitive. | Incompatible with JIT engines and some runtime code generation. Ensure renderer JS engine does not rely on JIT. |
| **Image load hardening** (no remote images, no low-mandatory-label images) | Reduces DLL planting / loading untrusted binaries. | Can break plugins/drivers/3rd-party DLL loading. Prefer static linking where possible. |
| **Strict handle checks** | Makes some handle misuse patterns fail fast. | Can surface latent bugs as hard failures; treat as “correctness pressure”. |

Important: these flags must be treated as *security-critical defaults*. If a mitigation is disabled
for compatibility, record:

- what broke
- why it’s safe
- what compensating controls exist

### Mitigation policy bitmask (exact flags)

In `crates/win-sandbox/src/mitigations.rs` we build a `u64` mask consumed by
`PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` using `PROCESS_CREATION_MITIGATION_POLICY_*` values from
the Windows SDK headers (`winbase.h`). `windows-sys` does not currently export these macro values,
so the crate defines the ones we use:

- `PROCESS_CREATION_MITIGATION_POLICY_STRICT_HANDLE_CHECKS_ALWAYS_ON`
- `PROCESS_CREATION_MITIGATION_POLICY_WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON`
- `PROCESS_CREATION_MITIGATION_POLICY_EXTENSION_POINT_DISABLE_ALWAYS_ON`
- `PROCESS_CREATION_MITIGATION_POLICY_PROHIBIT_DYNAMIC_CODE_ALWAYS_ON`
- `PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_REMOTE_ALWAYS_ON`
- `PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON`

The builder adds only flags supported by the current OS (best-effort compatibility).

## Fallback mode: restricted token + Low Integrity Level (weaker)

If AppContainer is unavailable or fails to initialize, the Windows sandbox spawner **fails closed**
by default and returns an error describing what went wrong (so we avoid silent security downgrades).

For developer convenience on unsupported Windows versions / unusual CI environments, you can opt in
to allowing weaker sandboxing (or no sandbox) by setting:

- `FASTR_ALLOW_UNSANDBOXED_RENDERER=1`

When this opt-in is enabled, we fall back to a “best-effort” sandbox:

- Create a **restricted token** via
  `CreateRestrictedToken(..., DISABLE_MAX_PRIVILEGE, ...)`.
  - Repo reality: `src/sandbox/windows.rs` currently strips privileges but does **not** disable any
    group SIDs (`DisableSidCount = 0`).
  - `crates/win-sandbox` additionally disables a small set of broad local group SIDs (deny-only)
    like `BUILTIN\\Users` as defense-in-depth against permissive filesystem ACLs.
- Set the token’s **Integrity Level (IL)** to **Low** (`S-1-16-4096`).

Then we spawn the child using `CreateProcessAsUserW` with the restricted primary token (see
`src/sandbox/windows.rs::spawn_restricted_token`).

Important pitfall (restricted-token spawns):

- If `lpCurrentDirectory` is left as `NULL`, Windows inherits the parent’s CWD. When `BUILTIN\\Users`
  (or similar broad groups) are disabled in the restricted token **or** when the token runs at Low
  IL, that inherited directory may no longer be accessible, causing `CreateProcessAsUserW` to fail
  with `ERROR_ACCESS_DENIED` (or leaving the child in a “weird” working directory).
- `src/sandbox/windows.rs::spawn_restricted_token` avoids this by setting `lpCurrentDirectory` to the
  executable’s parent directory (best-effort) and falling back to `C:\\Windows\\System32`.
- `crates/win-sandbox::restricted_token::spawn_with_token` avoids this by setting an explicit current
  directory: `SpawnConfig.current_dir` if provided, otherwise `SpawnConfig.exe.parent()` (best-effort
  “if the image is loadable, the directory is usually traversable too”).

This is meaningfully weaker than AppContainer:

- **Network is not reliably blocked.** A restricted/low-IL process can usually still open outbound
  sockets unless additional OS policy/firewall rules exist. Do *not* assume “no network” in this
  mode.
- Even with some group disabling (where used), this is not equivalent to AppContainer. Treat it as a
  compatibility fallback rather than a production-grade “no network/no filesystem” boundary.
- Filesystem access is reduced, but not eliminated: ACLs that allow read access to low-IL or “Everyone”
  may still be readable.
- Many Windows resources are not designed around Low IL as a strict sandbox boundary.

Guidance:

- Treat fallback mode as “better than nothing” for development/compatibility, not a production-grade
  renderer sandbox.
- Any security-sensitive feature should assume the renderer may have network access in fallback
  mode unless explicitly proven otherwise.

## Debugging / verification tips

### Useful environment variables

From `src/sandbox/windows.rs` (spawn-time sandboxing):

- `FASTR_LOG_SANDBOX=1`: enable verbose stderr logging for sandbox spawn decisions (AppContainer
  availability, `ERROR_ACCESS_DENIED` retries, breakaway/job assignment failures).
  - In debug (non-`--release`) builds, sandbox debug logging is enabled by default; this env var is
    primarily to enable the same logs in release builds.
- `FASTR_DISABLE_RENDERER_SANDBOX=1` / `FASTR_WINDOWS_RENDERER_SANDBOX=off`: disable Windows
  renderer sandboxing entirely (**debug only; insecure**).
  - Note: even with the token/AppContainer sandbox disabled, `spawn_sandboxed(...)` still uses the
    **handle allowlist** and still attempts to apply the **Job object** limits (kill-on-close,
    active-process cap).
    - If the child cannot be assigned to the Job (nested-job restrictions), it runs jobless and
      prints a warning.
- `FASTR_WINDOWS_SANDBOX_INHERIT_ENV=1`: opt into inheriting the full parent environment for the
  sandboxed child.
  - By default `src/sandbox/windows.rs` builds a sanitized environment block (so secrets from the
    browser process environment are not leaked into the renderer, and so TEMP/TMP point at an
    AppContainer-writable location).
  - This is intended for local debugging only.
- `FASTR_ALLOW_UNSANDBOXED_RENDERER=1`: opt in to running without the full Windows sandbox when
  required primitives are missing or sandbox startup fails.

From `crates/win-sandbox` (mitigation policy escape hatch):

- `FASTR_DISABLE_WIN_MITIGATIONS=1` (any value): do not apply mitigation policies during process
  spawn.

From `crates/win-sandbox` (strict “no silent downgrade” policy helper):

- `FASTR_ALLOW_UNSANDBOXED_RENDERER=1`: allow `RendererSandboxMode::new_default()` to return
  `Disabled` on hosts that do not support AppContainer and/or nested jobs. Without this opt-in,
  `new_default()` returns an error so callers don’t accidentally ship a silently-unsandboxed renderer.

### Verify AppContainer / Job in Process Explorer

Sysinternals Process Explorer can confirm the sandbox is applied:

1. Find the renderer process.
2. **AppContainer:**
   - Open *Properties* → *Security* tab.
   - Look for “AppContainer” / “Package name” style fields (exact UI varies by version).
   - Alternatively, add the “AppContainer” column in the process list (if available).
3. **Integrity level (fallback mode):**
   - Add the “Integrity Level” column and confirm it shows **Low**.
4. **Job object:**
   - Open *Properties* → *Job* tab (or add the “Job” column).
   - Confirm the process is in the expected job and that “Kill on job close” is set.
5. **Mitigations:**
   - Newer Process Explorer versions expose a *Mitigation* tab showing Win32k lockdown, dynamic code,
     etc.

If any of these are missing, the renderer is not properly sandboxed.

### Capturing Win32 errors (must-do when debugging sandbox failures)

Many sandbox failures present as “access denied” during process creation or IPC setup. Always log
the **real Win32 error code** at the failure site.

In Rust, capture immediately after the failing Win32 call:

```rust
let err = std::io::Error::last_os_error();
eprintln!("CreateProcessW failed: {err}"); // includes code + message
```

If using the `windows`/`windows-sys` crates, prefer an error that preserves the code:

- `windows::core::Error::from_win32()`
- `windows::Win32::Foundation::GetLastError()` + `FormatMessageW` for custom formatting

If you are working inside `crates/win-sandbox`, prefer returning `WinSandboxError::last("ApiName")`,
which captures `GetLastError()` and includes the formatted message in the error string.

Common error codes you’ll see:

- `ERROR_ACCESS_DENIED (5)`: token/capability/mitigation/job restriction blocked the action.
- `ERROR_INVALID_PARAMETER (87)`: a `STARTUPINFOEX` attribute list was malformed.
- `ERROR_NOT_SUPPORTED (50)`: OS doesn’t support the requested mitigation/policy.

When debugging, include in logs:

- the failing API name
- the numeric error code
- whether AppContainer or fallback mode was used
- which mitigation flags were requested

### Common issue: AppContainer `ERROR_ACCESS_DENIED` executing dev binaries

Unpackaged dev/test binaries are often not executable by an AppContainer token because their
directory does not grant read/execute access to the derived AppContainer SID.

Repo reality (`src/sandbox/windows.rs::spawn_appcontainer`):

- If the first `CreateProcessW` fails with `ERROR_ACCESS_DENIED`, we copy the image to a temporary
  directory and grant read/execute ACLs (prefer the derived AppContainer SID; fallback to **ALL
  APPLICATION PACKAGES**), then retry.
  - Note: when the AppContainer token is hardened to remove `ALL APPLICATION PACKAGES` (via
    `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`), granting ACLs to the `ALL APPLICATION
    PACKAGES` SID is only useful when the hardening attribute is disabled/unsupported on the host.
- We always set an explicit `lpCurrentDirectory` to a sandbox-accessible working directory so we do
  **not** inherit the parent process CWD (including on relocation retries). In the main spawner this
  is typically the AppContainer profile storage folder returned by `GetAppContainerFolderPath`
  (fallback: `C:\\Windows\\System32`).

Enable `FASTR_LOG_SANDBOX=1` to see which path was taken.

## Tests / regression coverage

Windows-only tests that encode the intended boundary:

- Handle allowlisting (no handle leaks): `tests/sandbox/windows_handle_inheritance.rs`
- No child processes (active process job limit): `tests/sandbox/windows_no_child_process.rs`
- Parent process handle escape attempt: `tests/sandbox/windows_process_handle_escape.rs`
- AppContainer spawn smoke: `tests/sandbox/windows_sandbox_appcontainer_spawn.rs`
- AppContainer token does not include `ALL APPLICATION PACKAGES` when hardening is enabled:
  `tests/sandbox/windows_all_application_packages_hardening.rs`
- `win_sandbox::spawn_sandboxed` can remove `ALL APPLICATION PACKAGES` when hardening is enabled:
  `crates/win-sandbox/tests/all_application_packages_hardening.rs`
- Sandboxed renderer smoke (can initialize FastRender + render minimal HTML under AppContainer + mitigations): `tests/sandbox/windows_renderer_smoke.rs`
- Environment sanitization (no secret env inheritance): `tests/sandbox/windows_sandbox_env_sanitization.rs`
- AppContainer temp dir is writable (override parent TEMP/TMP): `tests/sandbox/windows_appcontainer_temp_dir.rs`
- Job kill-on-close semantics: `tests/sandbox/windows_job_kill_on_close.rs`
- AppContainer blocks outbound network (end-to-end spawn helper): `tests/sandbox/windows_network_denial.rs`
- End-to-end token state + filesystem/network denial + job kill-on-close: `tests/sandbox/windows_renderer_sandbox_test.rs`
- AppContainer blocks user profile file reads (end-to-end spawn helper): `crates/win-sandbox/tests/filesystem_denied.rs`
- AppContainer blocks outbound network (no capabilities): `crates/win-sandbox/tests/network_denied.rs`
- Job object invariants (kill-on-close, process count): `crates/win-sandbox/tests/job_limits.rs`
- `win_sandbox::spawn_sandboxed` handle allowlist enforcement: `crates/win-sandbox/tests/handle_inheritance.rs`
- `win_sandbox::renderer::RendererSandbox` smoke (AppContainer + Job + no grandchildren): `crates/win-sandbox/tests/renderer_sandbox.rs`
- Restricted-token fallback invariants (low integrity, reduced filesystem access): `crates/win-sandbox/tests/restricted_token_mode.rs`
- Restricted-token spawn does not inherit an inaccessible parent CWD: `crates/win-sandbox/tests/restricted_token_cwd.rs`
- Mitigation policy verification: `crates/win-sandbox/src/lib.rs` tests +
  `crates/win-sandbox/src/mitigations.rs::verify_renderer_mitigations_current_process`

## Checklist for Windows sandbox changes (use in reviews)

The Windows renderer sandbox is security-critical. When changing any of the spawn/sandbox code, be
explicit about which layer you are modifying and why.

- **Are you using the right spawner?**
  - The full sandbox is `fastrender::sandbox::windows::spawn_sandboxed(...)`.
  - Do not spawn the renderer with raw `std::process::Command` unless you are intentionally running
    unsandboxed (debugging), and even then prefer routing through the existing escape hatches so the
    run is obviously insecure.

- **AppContainer (preferred mode)**
  - Must remain **zero capabilities** (`CapabilityCount = 0`, `Capabilities = NULL`) unless there is
    a strong security justification.
  - If you add a capability, treat it as a security boundary change:
    - update this doc
    - add/adjust tests
    - document what new host access it grants (network, device access, etc.)

- **Job object**
  - Must keep `KILL_ON_JOB_CLOSE` and `ACTIVE_PROCESS_LIMIT=1`.
  - Must not enable breakaway (`JOB_OBJECT_LIMIT_BREAKAWAY_OK` / `SILENT_BREAKAWAY_OK`).
  - If the child cannot be assigned to the Job (nested-job restrictions), treat it as a sandbox
    degradation (today it’s a warning); consider whether the caller should fail-closed in the future.

- **Handle inheritance**
  - Must use `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` for an explicit allowlist.
  - Any new inherited handle should be reviewed as a capability:
    - can it be used to reach the filesystem/network/other processes?
    - does it allow duplicating privileged handles from the broker?
  - If you need to pass a handle, add it to the allowlist and extend the handle inheritance tests.

- **Mitigations**
  - If you change the mitigation policy bitmask, update:
    - `crates/win-sandbox/src/mitigations.rs` (bitmask + verifier)
    - this doc (tradeoffs + exact flags)
  - If mitigations are wired into the main renderer spawn path (`src/sandbox/windows.rs`), ensure
    the “repo reality” section stays accurate and add a regression test verifying mitigations are
    active in the spawned child.

- **Fallback mode**
  - Never assume fallback blocks network. Design the architecture so the renderer is safe even if
    fallback mode still has outbound network access.
