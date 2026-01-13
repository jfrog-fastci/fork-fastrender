# Workstream: Multiprocess Architecture & Security

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

---

## The job

Build a **secure, isolated multiprocess architecture** like real browsers have.

Current state: Single process. A renderer bug can compromise everything.

Target state: Process isolation where content renderer exploits cannot:
- Steal data from other tabs
- Access browser state (bookmarks, history, passwords)
- Escape to the host system
- Spoof the address bar

## Why this matters

Modern browsers use multiprocess for:

1. **Security**: Untrusted web content runs in sandboxed processes
2. **Stability**: One tab crash doesn't kill the browser
3. **Performance**: Parallel rendering across CPU cores
4. **Isolation**: Sites can't spy on each other

Without it, FastRender is fundamentally unsafe for real browsing.

## What counts

A change counts if it lands at least one of:

- **Process separation**: Content renders in a separate process from browser UI.
- **Sandbox enforcement**: Renderer process has restricted OS capabilities.
- **IPC mechanism**: Processes communicate safely via message passing.
- **Crash isolation**: Renderer crash recovers gracefully.
- **Security boundary**: Clear separation between trusted and untrusted code.

## Architecture

### Target process model

```
┌────────────────────────────────────────────────────────────────┐
│                     Browser Process                            │
│  - Window management (winit)                                   │
│  - UI rendering (egui) [or renderer-chrome in future]          │
│  - Navigation decisions                                        │
│  - Bookmark/history/settings storage                           │
│  - Cookie jar                                                  │
│  - IPC orchestration                                           │
│  - TRUSTED (not sandboxed)                                     │
└────────────────────────────────────────────────────────────────┘
         │ IPC                    │ IPC                   │ IPC
         ▼                        ▼                       ▼
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│ Renderer Process│    │ Renderer Process│    │ Renderer Process│
│   (Tab 1)       │    │   (Tab 2)       │    │   (Tab 3)       │
│                 │    │                 │    │                 │
│ - HTML parsing  │    │ - HTML parsing  │    │ - HTML parsing  │
│ - CSS/layout    │    │ - CSS/layout    │    │ - CSS/layout    │
│ - JS execution  │    │ - JS execution  │    │ - JS execution  │
│ - Painting      │    │ - Painting      │    │ - Painting      │
│                 │    │                 │    │                 │
│ SANDBOXED       │    │ SANDBOXED       │    │ SANDBOXED       │
│ - No filesystem │    │ - No filesystem │    │ - No filesystem │
│ - No network    │    │ - No network    │    │ - No network    │
│ - Limited IPC   │    │ - Limited IPC   │    │ - Limited IPC   │
└─────────────────┘    └─────────────────┘    └─────────────────┘
         │                        │                       │
         ▼                        ▼                       ▼
┌─────────────────────────────────────────────────────────────────┐
│                      Network Process                            │
│  - All network requests go through here                         │
│  - Cookie enforcement                                           │
│  - CORS checks                                                  │
│  - Certificate validation                                       │
│  - Sandboxed (limited capabilities)                             │
└─────────────────────────────────────────────────────────────────┘
```

If/when renderer-chrome lands (chrome UI rendered by FastRender inside the browser process), the
trusted chrome document may use a privileged JS bridge (`globalThis.chrome`). The canonical API
surface + trust boundary is documented in [`docs/chrome_js_bridge.md`](../docs/chrome_js_bridge.md).

### Site isolation

Beyond process-per-tab, consider site isolation:
- Different origins get different processes
- Prevents Spectre-style cross-origin attacks
- Chrome's model: one process per site (not per tab)
- See [`docs/site_isolation.md`](../docs/site_isolation.md) for FastRender’s intended process assignment policy (MVP + planned evolution).

FastRender's intended per-origin process assignment + OOPIF semantics are specified in:
- [`docs/site_isolation.md`](../docs/site_isolation.md) (normative)

### IPC design

```rust
// Messages from Browser → Renderer
enum BrowserToRenderer {
    Navigate { url: Url },
    ExecuteScript { script: String },
    Resize { width: u32, height: u32 },
    MouseEvent { ... },
    KeyEvent { ... },
}

// Messages from Renderer → Browser
enum RendererToBrowser {
    FrameReady { pixels: SharedMemory },
    TitleChanged { title: String },
    NavigationRequest { url: Url },
    ContextMenu { items: Vec<MenuItem> },
    Alert { message: String },
}
```

Linux implementation checklist (shared memory + FD passing footguns): [`docs/ipc_linux_fd_passing.md`](../docs/ipc_linux_fd_passing.md).

### Sandbox technologies

| Platform | Sandbox mechanism |
|----------|-------------------|
| Linux | seccomp-bpf, namespaces, landlock |
| macOS | Seatbelt (`sandbox_init` / `sandbox-exec`), App Sandbox |
| Windows | Job objects, AppContainer, LPAC |

macOS note: FastRender prefers the system-provided Seatbelt profile `pure-computation` when
applying a strict sandbox. Some macOS versions do not ship that named profile (or treat it as
invalid), so the implementation falls back to an embedded SBPL profile string with:

- `(deny default)`
- explicit denies for `file-read*`, `file-write*`, and `network*`

See `src/sandbox/macos.rs` (`apply_strict_sandbox`) for details.

## Priority order

### P0: Process separation (no sandbox yet)

Just get content rendering in a separate process.

1. **Split renderer into library + executable**
   - `libfastrender` - rendering logic
   - `fastrender-renderer` - executable that hosts one tab

2. **IPC mechanism**
   - Shared memory for pixel buffers
   - Pipe/socket for control messages
   - Consider existing crates: `ipc-channel`, `interprocess`

3. **Browser process spawns renderers**
   - One renderer per tab
   - Browser composites frames from all renderers

4. **Crash recovery**
   - Renderer crash shows "tab crashed" UI
   - User can reload tab
   - Other tabs unaffected

### P1: Basic sandboxing

Restrict what renderer processes can do.

1. **No direct filesystem access**
   - Renderer can only read via IPC (browser mediates)
   
2. **No direct network access**
   - Network requests go through network process
   - Browser enforces same-origin policy

3. **Limited IPC surface**
   - Renderer can only send allowed message types
   - No arbitrary syscalls

### P2: Site isolation

Separate processes by origin, not just by tab.

1. **Cross-origin iframes get own process**
2. **Navigate to new origin → new process**
3. **Memory isolation between sites**

### P3: Defense in depth

1. **ASLR/CFI in renderer builds**
2. **Seccomp filters (Linux)**
3. **Memory-safe language benefits (Rust helps here)**
4. **Audit IPC attack surface**

## Testing

### Security tests

```rust
#[test]
fn renderer_cannot_read_filesystem() {
    let renderer = spawn_sandboxed_renderer();
    // Attempt should fail
    let result = renderer.try_read_file("/etc/passwd");
    assert!(result.is_err());
}

#[test]
fn renderer_crash_doesnt_kill_browser() {
    let browser = TestBrowser::new();
    browser.open_tab("crash://");  // Intentionally crash renderer
    assert!(browser.is_running());
    assert!(browser.tab(0).shows_crash_page());
}
```

### Fuzzing

- Fuzz IPC messages from renderer
- Fuzz content that might escape sandbox
- Use existing browser security test suites

## Relationship to other workstreams

- **renderer_chrome.md**: If we render chrome with FastRender, it must be in trusted (browser) process
- **live_rendering.md**: Render loop runs in renderer process
- **browser_chrome.md**: Chrome UI stays in browser process

## Success criteria

Multiprocess security is **done** when:
- Content renders in separate process from browser UI
- Renderer crash doesn't crash browser
- Renderer cannot access filesystem directly
- Renderer cannot make network requests directly
- Site isolation prevents cross-origin data access

This is foundational infrastructure. Without it, the browser is unsafe.
