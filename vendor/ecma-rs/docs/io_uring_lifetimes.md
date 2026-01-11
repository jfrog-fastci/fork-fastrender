# `io_uring` pointer lifetimes and teardown policy

## Core invariant

Any user memory referenced by an SQE **must remain valid and stable** until the kernel produces
the corresponding CQE (or the final CQE for multi-shot operations).

This includes:

- I/O buffers (`read`, `write`, `send`, `recv`, …)
- I/O vector metadata (`readv`/`writev` iovec arrays)
- Any per-op metadata whose address is placed into an SQE

Freeing or moving that memory early is Undefined Behavior: the kernel may dereference stale
pointers.

## `runtime-io-uring` lifecycle semantics

`runtime-io-uring` chooses a conservative, memory-safe policy:

### Per-operation handle drop

Dropping an operation handle (e.g. `IoOp` / `MultiShotHandle`) **does not free** any SQE-referenced
memory. Instead the op is marked as **detached**, and the driver retains ownership of the op state
until the CQE is observed (or the final CQE for multi-shot). Cleanup is **CQE-driven** and happens
exactly once in the completion path.

### Driver drop (policy B)

Drivers must not be dropped while operations are still in-flight:

- In debug builds, dropping a driver with in-flight ops **panics** (unless already unwinding).
- In release builds, dropping a driver with in-flight ops **leaks** the ring + in-flight state to
  avoid use-after-free.

This matches the crate's **policy B**: explicit shutdown/drain is required (drive CQE processing and
cancel as needed) before dropping the driver.

When the `debug_stability` feature is enabled, extra assertions verify that SQE-referenced pointers
are not dropped before CQE processing.
