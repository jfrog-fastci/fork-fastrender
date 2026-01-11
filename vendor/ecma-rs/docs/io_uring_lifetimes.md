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
memory. Instead the op becomes **detached** from the caller, and the driver retains ownership of the
in-flight op state until the CQE is observed (or the final CQE for multi-shot).

Cleanup is **CQE-driven** and happens exactly once in the completion path.

Notes:

- Dropping `IoOp` does **not** cancel the kernel request. If the handle is dropped early, the driver
  still performs the completion path to release pins/roots/metadata, but the completion value is
  discarded.
- Dropping `MultiShotHandle` submits an `IORING_OP_ASYNC_CANCEL` request (best-effort) to stop the
  multi-shot stream, but kernel-referenced metadata and provided buffers are still held until the
  final CQE indicates the kernel is finished (`IORING_CQE_F_MORE` is no longer set).

### Driver drop (policy B)

Drivers must not be dropped while operations are still in-flight (including internal ops such as
buffer provisioning):

- In debug builds (and when `runtime-io-uring` is built with `debug_stability` enabled), dropping a
  driver with in-flight ops **panics** (unless already unwinding). Importantly, it **leaks first**
  and only then panics, to avoid dropping in-flight pointers during unwinding.
- In release builds, dropping a driver with in-flight ops **leaks** the ring + in-flight state to
  avoid use-after-free.

This matches the crate's **policy B**: explicit shutdown/drain is required (drive CQE processing and
cancel as needed) before dropping the driver.

When the `debug_stability` feature is enabled, extra assertions verify that SQE-referenced pointers
are not dropped before CQE processing.
