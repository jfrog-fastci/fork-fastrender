# ecma-rs (agent instructions)

This file contains **repo-wide** rules shared by all workstreams.

## Workstreams

Pick one workstream and follow its specific doc:

- **TypeScript type checking (from-scratch rigorous checker)**: `instructions/ts_typecheck.md`
- **Native AOT compilation (LLVM-based JS/TS → native)**: `instructions/native_aot.md`

## Agent Resource Guidelines

**Context:** Hundreds of concurrent coding agents on one system (192 vCPU, 1.5TB RAM, 110TB disk).

### Assume every process can misbehave

This codebase implements a JavaScript engine — code designed to execute arbitrary, potentially hostile programs. **Any test or binary can hang, explode memory, or refuse to terminate:**

- Parser bugs can cause infinite loops or exponential backtracking
- Runtime tests can trigger `while(true){}` in generated code
- LLVM compilation can hang on pathological IR
- Tests that worked yesterday can regress into livelocks today

**Every command needs hard external limits that the code being run cannot bypass:**
- `timeout -k 10 <seconds>` — time limit with **guaranteed SIGKILL** (plain `timeout` sends SIGTERM which can be ignored)
- Memory limits via `run_limited.sh` / `cargo_agent.sh` (kernel-enforced)
- Scoped builds/tests (don't compile the universe)

If something exceeds limits, that's a **bug to investigate** — not a limit to raise.

**Critical constraint:** RAM. Too many concurrent memory-heavy processes will OOM-kill everything.

**Not a constraint:** CPU and disk I/O. Scheduler handles contention fine. Don't be overly conservative.

**Vendored checkout note:** In this repository, ecma-rs lives under `vendor/ecma-rs/` as a nested
workspace. The commands below are written to run from the **top-level repo root**. If you've
already `cd vendor/ecma-rs`, drop the `vendor/ecma-rs/` prefix from paths (scripts and `target/`).

### Rules

**1. Always use timeout + wrapper scripts:**
```bash
# CORRECT — time limit + memory limit + scoped:
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh build --release -p native-js
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p effect-js --lib

# WRONG — no time limit (can hang forever):
bash vendor/ecma-rs/scripts/cargo_agent.sh build --release -p native-js

# WRONG — no wrapper (uncontrolled parallelism + no memory limit):
cargo build
cargo test
```

The `-k 10` is critical: it sends SIGKILL 10 seconds after SIGTERM if the process doesn't exit.
Plain `timeout` only sends SIGTERM, which pathological code can catch and ignore forever.

The wrapper (`vendor/ecma-rs/scripts/cargo_agent.sh`) `cd`s into `vendor/ecma-rs/` and delegates to
the top-level `scripts/cargo_agent.sh` wrapper. It enforces:
- Slot-based concurrency limiting (prevents cargo stampedes)
- Per-command RAM cap via `RLIMIT_AS` (default 64GB)
- Reasonable test thread counts

**2. Scope your cargo commands:**
```bash
# CORRECT (scoped to specific crate + timeout):
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p native-js --lib
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh build -p effect-js

# WRONG (compiles entire workspace — combinatorial explosion):
bash vendor/ecma-rs/scripts/cargo_agent.sh build --all
bash vendor/ecma-rs/scripts/cargo_agent.sh test
```

**3. LLVM operations need extra RAM + longer timeouts:**

LLVM compilation is memory-hungry and can hang on pathological IR. Use the LLVM wrapper for native codegen:
```bash
# Preferred: LLVM wrapper (96GB limit) + timeout:
timeout -k 10 900 bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p native-js
timeout -k 10 900 bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib

# Or set manually:
timeout -k 10 900 FASTR_CARGO_LIMIT_AS=96G bash vendor/ecma-rs/scripts/cargo_agent.sh test -p native-js --lib

# For full release builds with LTO (very hungry, longer timeout):
timeout -k 10 1800 FASTR_CARGO_LIMIT_AS=128G bash vendor/ecma-rs/scripts/cargo_agent.sh build --release -p native-js
```

**Frame pointers are mandatory for runtime-native stack walking:**

- `runtime-native` must be compiled with `-C force-frame-pointers=yes` (Rust).
  - `vendor/ecma-rs/scripts/cargo_llvm.sh` injects this automatically.
- Any generated LLVM code that participates in stack walking must be compiled with
  `-frame-pointer=all` (Clang/LLVM).

**4. Don't artificially limit parallelism:**
```bash
# WRONG (too conservative - wastes resources):
FASTR_CARGO_JOBS=1 bash vendor/ecma-rs/scripts/cargo_agent.sh build ...

# RIGHT (let the wrapper decide based on available slots):
bash vendor/ecma-rs/scripts/cargo_agent.sh build ...

# RIGHT (if you need to limit for a specific reason, document why):
# Reduce parallelism because this test spawns subprocesses
FASTR_CARGO_JOBS=8 bash vendor/ecma-rs/scripts/cargo_agent.sh test ...
```

**5. ALL processes need time limits (not just "long-running"):**

Any process can hang — including ones that "should" be fast. Always use `timeout -k`:
```bash
# CORRECT — timeout with SIGKILL fallback + memory limit:
timeout -k 10 600 bash vendor/ecma-rs/scripts/run_limited.sh --as 32G -- \
  ./vendor/ecma-rs/target/release/my_binary

# WRONG — no SIGKILL fallback (process can ignore SIGTERM forever):
timeout 600 bash vendor/ecma-rs/scripts/run_limited.sh --as 32G -- ./vendor/ecma-rs/target/release/my_binary

# WRONG — no time limit at all:
bash vendor/ecma-rs/scripts/run_limited.sh --as 32G -- ./vendor/ecma-rs/target/release/my_binary
```

**6. Clean up disk when over budget:**
```bash
# Before long loops, check `vendor/ecma-rs/target/` size:
TARGET_MAX_GB="${TARGET_MAX_GB:-400}"
if [[ -d vendor/ecma-rs/target ]]; then
  size_gb=$(du -sg vendor/ecma-rs/target 2>/dev/null | cut -f1 || echo 0)
  if [[ "${size_gb}" -ge "${TARGET_MAX_GB}" ]]; then
    echo "vendor/ecma-rs/target at ${size_gb}GB, cleaning..." >&2
    bash vendor/ecma-rs/scripts/cargo_agent.sh clean
  fi
fi
```

### Resource Estimates

| Operation | RAM (per process) | Notes |
|-----------|-------------------|-------|
| `cargo check -p crate` | 2-8 GB | Depends on crate size |
| `cargo build -p crate` | 4-16 GB | Debug build |
| `cargo build --release -p crate` | 8-32 GB | Release + optimizations |
| `cargo build --release` (LTO) | 32-96 GB | Full workspace LTO |
| `cargo test -p crate` | 4-16 GB | Depends on test count |
| LLVM codegen (our native-js) | 16-64 GB | Per compilation unit |
| Running compiled binary | 1-32 GB | Depends on workload |

### What NOT to Worry About

- **CPU contention**: Scheduler handles it. If 200 agents all want CPU, they get time-sliced.
- **Disk I/O contention**: NVMe handles parallel I/O well. Don't serialize disk operations.
- **Network**: Not relevant for compilation.
- **Concurrent git operations**: Each agent has own repo copy. No conflicts.

### Quick Reference

**Every command needs `timeout -k` — no exceptions:**

```bash
# Standard build/test (most operations):
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh build -p <crate>
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p <crate> --lib

# LLVM-heavy operations (native-js, runtime-native) — longer timeout:
timeout -k 10 900 bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p native-js
timeout -k 10 900 bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib
# (This wrapper also injects `-C force-frame-pointers=yes` for runtime-native stack walking.)

# Or with explicit limit:
timeout -k 10 900 FASTR_CARGO_LIMIT_AS=96G bash vendor/ecma-rs/scripts/cargo_agent.sh <command>

# Running binaries:
timeout -k 10 600 bash vendor/ecma-rs/scripts/run_limited.sh --as 32G -- \
  ./vendor/ecma-rs/target/release/binary

# Check if target/ needs cleaning:
du -sh vendor/ecma-rs/target/
```
