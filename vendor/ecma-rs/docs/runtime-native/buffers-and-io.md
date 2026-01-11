# Buffers and async I/O under a moving GC

This document defines the **memory invariants** and **API contract** for
`ArrayBuffer` / `TypedArray` backing storage in `runtime-native/`, specifically
to make OS I/O (syscalls, `io_uring`, threadpool I/O, etc.) safe under a **precise
moving GC**.

If you are about to:

- add a new I/O primitive (fs, sockets, fetch, compression, crypto),
- refactor the GC,
- introduce a new async executor / I/O backend,

read this first. The rules below exist to prevent reintroducing **moving pointers
into kernel APIs**.

## Problem statement

`runtime-native/` intends to use a **precise moving collector** (compaction and
copying) for JS objects. A moving GC can relocate objects between safepoints.

Most OS I/O APIs require that user-space buffers are backed by a **stable
address** for some time window:

- synchronous syscalls (`read`, `write`, `recvmsg`, `sendmsg`) require a stable
  pointer for the duration of the call,
- asynchronous I/O APIs (`io_uring`, AIO, threadpool emulation) require a stable
  pointer **until completion**.

If a buffer points into the moving GC heap, then any GC that runs while an I/O
operation is in flight can move/free that memory, while the kernel still holds
the old pointer. The result is memory corruption.

We therefore must ensure that *any pointer passed to the OS* is either:

1) outside the moving heap (non-moving allocation), and
2) kept alive until the OS will no longer touch it.

## Chosen approach

### 1) Non-moving backing stores for `ArrayBuffer` / `TypedArray`

All `ArrayBuffer`-like objects expose their bytes via a separate
**BackingStore** allocation that:

- is allocated in non-moving memory (e.g. `Box<[u8]>`, `mmap`, or a custom
  allocator),
- has a **stable base pointer** for the lifetime of the backing store,
- is treated as **external memory** by the GC (see [Memory accounting](#memory-accounting)).

The GC-managed `ArrayBuffer` / `TypedArray` objects may move freely; they only
contain a handle/pointer to the backing store, plus view metadata (offset/len).

### 2) Pin-count protocol for in-flight I/O

Each `BackingStore` maintains a **pin count** (`pin_count: AtomicUsize`).

An I/O operation that submits a buffer to the OS must:

1. **pin** the backing store before the OS can observe the pointer,
2. keep the pin alive until completion or cancellation is *acknowledged*,
3. **unpin** exactly once.

Pinning exists to prevent:

- freeing the backing store while the kernel still uses it,
- (future) resize/detach/transfer semantics from changing the data pointer.

### 3) Host code stores only handles/roots (no raw GC pointers)

Async operations must not store raw pointers to GC-managed objects across yield
points. Instead they store:

- a GC-safe handle/root to the JS value (promise, ArrayBufferView) **and/or**
- a stable `BackingStoreHandle` (e.g. `Arc<BackingStore>`) plus `(offset, len)`.

Raw pointers derived from GC objects are only permitted inside a short-lived
**pinned view** object that enforces the pin lifetime.

## Invariants (turn these into asserts/tests)

These invariants are the contract between the GC, buffer implementation, and
all I/O backends. Violations are memory safety bugs.

### Backing store invariants

- **Stable address:** A backing store’s `base_ptr` never changes after
  allocation.
- **No implicit reallocation:** A backing store is not backed by a growable
  container that may reallocate while still referenced (e.g. no `Vec::reserve`
  after publication).
- **Pinned ⇒ alive:** If `pin_count > 0`, the backing store memory must not be
  freed.
- **No underflow:** `pin_count` never underflows; `unpin()` must be paired
  1:1 with a successful `pin()`.
- **Drop safety:** Dropping/freeing a backing store must assert (or otherwise
  guarantee) `pin_count == 0`.

### I/O invariants

- **No moving pointers into syscalls:** All OS I/O submission APIs take a
  `PinnedSlice`/`PinnedRange` (or equivalent) rather than `(ptr, len)` from
  untrusted callers.
- **Pin before submit:** The pin is acquired before the pointer is passed to the
  OS / I/O backend and remains held until the backend guarantees the pointer is
  no longer in use.
- **Bounds checked:** Creating a pinned slice checks `offset + len <=
  byte_length` with overflow checking.
- **Exactly-once completion:** Each I/O op transitions through a single
  completion path; the JS promise is settled **exactly once** (resolve or
  reject), even if cancellation races with completion.

### GC integration invariants

- **External memory is tracked:** backing store bytes are added to a GC-visible
  external-memory counter on allocation and removed on free.
- **GC does not need to scan backing store bytes:** backing store contents are
  treated as raw bytes and must not contain GC pointers.

## Suggested API shape (sketch)

This is not the only possible API, but whatever we implement must enforce the
invariants above.

```rust
/// Non-moving bytes backing an ArrayBuffer-like object.
struct BackingStore {
  base: NonNull<u8>,
  len: usize,
  pin_count: AtomicUsize,
  // ... external memory accounting metadata ...
}

/// A stable, owned pin guard. Holding this keeps the OS-visible pointer valid.
struct PinnedRange {
  store: BackingStoreHandle, // e.g. Arc<BackingStore>
  ptr: NonNull<u8>,
  len: usize,
}

impl BackingStore {
  fn pin_range(&self, offset: usize, len: usize) -> Result<PinnedRange, IoError>;
}

impl Drop for PinnedRange {
  fn drop(&mut self) { self.store.unpin(); }
}
```

**Rule:** All runtime-native I/O code should be structured such that it can only
obtain a raw pointer for the kernel by holding a `PinnedRange` value.

## Memory accounting

Backing stores can be large and live longer than JS objects. If their bytes are
not included in GC pressure, programs can allocate large `ArrayBuffer`s and
never trigger GC, leading to OOM even though “heap bytes” look small.

Contract:

- On backing store allocation: `external_bytes += len`.
- On backing store free: `external_bytes -= len`.
- The GC trigger heuristic must incorporate `external_bytes` (exact policy is
  GC-specific; e.g. treat it as part of the live-set size or as a separate
  budget).

Implementation note: if we support backing stores backed by `mmap` or other
external sources, the accounting still tracks the virtual size to maintain GC
backpressure.

## Cancellation contract

Pinning is coupled to the *OS-level lifetime* of the pointer, not to the JS
promise lifetime.

### Definitions

- **Completion:** the I/O backend reports that the operation will not access the
  user buffer again (success or error).
- **Cancel request:** user requests cancellation (e.g. `AbortSignal`), or the
  runtime decides to cancel.
- **Cancel acknowledgement (cancel-ack):** the backend reports that cancellation
  is effective, or otherwise guarantees the operation has ceased accessing the
  buffer.

### Rules

- An operation that has submitted a `PinnedRange` must hold the pin until
  **completion or cancel-ack**, whichever comes last.
- The JS promise is settled **exactly once**:
  - if completion wins the race, settle with the completion result;
  - if cancellation wins, reject with an abort/cancel error; later completion
    must not resettle the promise (but it still performs cleanup/unpin).

This implies a small state machine (often an atomic enum) for each op:

```
Pending -> Completed
Pending -> Cancelled (promise settled) -> Completed (cleanup only)
```

Even if JS drops the promise/buffer, the runtime must still run the completion
path to unpin and free resources; reaching completion must not depend on JS
reachability.

## Non-goals / MVP exclusions

To keep the initial runtime-native design small and to preserve the invariants
above, the MVP explicitly does **not** implement:

- **Resizable ArrayBuffer** (would require pointer-stability rules during growth
  and interaction with pins),
- **ArrayBuffer detach/transfer** (would require detaching to block or fail
  while pinned, plus careful promise/I/O interactions),
- **SharedArrayBuffer** (requires thread-safe memory model, atomics, and
  cross-thread lifetime; see [Future work](#future-work)).

If/when these features are added later, they must be designed to preserve the
pinning invariants (e.g. “detach fails while pinned”).

## Future work

- **`io_uring` fixed buffer registration:** backing stores are a natural match
  for `IORING_REGISTER_BUFFERS`. Pins could be used to manage the registration
  lifetime and prevent unmapping while registered.
- **Zero-copy file `mmap` for buffers:** allow an `ArrayBuffer` backing store to
  be backed by an `mmap` region for large files. Requires careful accounting,
  explicit `msync`/flush semantics, and interaction with detachment/resizing.
- **SharedArrayBuffer:** likely reuses the non-moving backing store concept, but
  adds atomic operations, cross-thread publication, and stricter lifetime rules.
