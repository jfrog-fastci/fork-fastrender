# Windows renderer sandbox (Job objects + AppContainer + restricted token)

This doc describes the **current intended sandbox boundary** for the Windows *renderer* process.
It is written to prevent “small refactors” from accidentally weakening isolation.

Implementation lives in `crates/win-sandbox/` (Win32 wrappers for:
AppContainer, restricted tokens + Integrity Levels, Job objects, and extended process creation via
`STARTUPINFOEXW`).

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
2. **Job object limits**: lifetime + process-count + optional memory ceiling.
3. **CreateProcess handle allowlist**: only explicitly-approved handles are inherited.
4. **Process mitigation policies**: reduce kernel / Win32 API attack surface.
5. **Fallback mode** if AppContainer cannot be used: restricted token + Low IL (weaker; see below).

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

We intentionally create the renderer AppContainer with:

- `SECURITY_CAPABILITIES.CapabilityCount = 0`
- `SECURITY_CAPABILITIES.Capabilities = NULL`

This means:

- **No network capabilities** (including no loopback access).
- No opt-in device or library capabilities.

If a future change wants to add a capability, it must be treated as a security-sensitive change
(and this doc must be updated).

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

### Limits we set

We configure the renderer Job object with `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` to enforce:

- **Kill-on-close:** `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
  - If the broker exits/crashes, the renderer is not left behind.
  - This prevents “orphaned sandbox processes” from accumulating or persisting.

- **Active process limit:** `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (limit = 1)
  - The renderer cannot spawn child processes.
  - This is important even if the token is restricted: child processes complicate containment and
    can become an escape vector if they end up outside the intended security config.

- **Optional memory cap:** (enabled when configured)
  - Use a Job memory limit so the renderer cannot allocate unbounded memory.
  - Tradeoff: if set too low, legitimate pages may hit the limit and fail/terminate.

Notes:

- The Job is owned by the broker. The renderer should never be given a handle to the Job object.
- The process-count limit is a security boundary, not just “resource management”.

## Handle inheritance allowlisting (critical)

On Windows, a sandbox boundary can be defeated if the child inherits **powerful handles** from the
broker (even if the child runs with a restricted token).

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

In addition to token restrictions, we enable Win32 process mitigations at creation time using
`PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` (and where supported, the “policy2” attribute).

These mitigations reduce the renderer’s attack surface against:

- the kernel
- win32k (GUI/GDI) system call layer
- dynamic code injection techniques
- DLL search order hijacks / image load tricks

Mitigations we enable (intended):

| Mitigation | Why we want it | Tradeoffs / compatibility |
|-----------|-----------------|---------------------------|
| **Win32k lockdown** (`WIN32K_SYSTEM_CALL_DISABLE`) | Removes a historically bug-dense kernel attack surface (USER/GDI). | Breaks anything that requires USER32/GDI syscalls. Renderer must stay headless (no windows, no GDI text paths). |
| **Disable extension points** | Blocks legacy “extension point” injection mechanisms. | Rare compatibility issues with injected DLL ecosystems (IME/hooks) — acceptable for sandbox. |
| **Prohibit dynamic code** | Prevents RWX / JIT-style codegen as an exploitation primitive. | Incompatible with JIT engines and some runtime code generation. Ensure renderer JS engine does not rely on JIT. |
| **Image load hardening** (no remote / no low-integrity images; prefer System32) | Reduces DLL planting / loading untrusted binaries. | Can break plugins/drivers/3rd-party DLL loading. Prefer static linking where possible. |
| **Strict handle checks** | Makes some handle misuse patterns fail fast. | Can surface latent bugs as hard failures; treat as “correctness pressure”. |

Important: these flags must be treated as *security-critical defaults*. If a mitigation is disabled
for compatibility, record:

- what broke
- why it’s safe
- what compensating controls exist

## Fallback mode: restricted token + Low Integrity Level (weaker)

If AppContainer is unavailable or fails to initialize, we fall back to a “best-effort” sandbox:

- Create a **restricted token** (drop SIDs/privileges; disable admin-like capabilities).
- Set the token’s **Integrity Level (IL)** to **Low**.

This is meaningfully weaker than AppContainer:

- **Network is not reliably blocked.** A restricted/low-IL process can usually still open outbound
  sockets unless additional OS policy/firewall rules exist. Do *not* assume “no network” in this
  mode.
- Filesystem access is reduced, but not eliminated: ACLs that allow read access to low-IL or “Everyone”
  may still be readable.
- Many Windows resources are not designed around Low IL as a strict sandbox boundary.

Guidance:

- Treat fallback mode as “better than nothing” for development/compatibility, not a production-grade
  renderer sandbox.
- Any security-sensitive feature should assume the renderer may have network access in fallback
  mode unless explicitly proven otherwise.

## Debugging / verification tips

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

If using the `windows` crate, prefer an error that preserves the code:

- `windows::core::Error::from_win32()`
- `windows::Win32::Foundation::GetLastError()` + `FormatMessageW` for custom formatting

Common error codes you’ll see:

- `ERROR_ACCESS_DENIED (5)`: token/capability/mitigation/job restriction blocked the action.
- `ERROR_INVALID_PARAMETER (87)`: a `STARTUPINFOEX` attribute list was malformed.
- `ERROR_NOT_SUPPORTED (50)`: OS doesn’t support the requested mitigation/policy.

When debugging, include in logs:

- the failing API name
- the numeric error code
- whether AppContainer or fallback mode was used
- which mitigation flags were requested

