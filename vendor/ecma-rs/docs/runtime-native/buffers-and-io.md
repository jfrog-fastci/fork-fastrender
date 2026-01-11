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

Each backing store maintains a **pin count** (`pin_count: AtomicU32`) in a stable control block
(`BackingStoreInner`, reference-counted by `BackingStore` handles) that is shared across any GC moves
of the `ArrayBuffer` header.

An I/O operation that submits a buffer to the OS must:

1. **pin** the backing store before the OS can observe the pointer,
2. keep the pin alive until completion or cancellation is *acknowledged*,
3. **unpin** exactly once.

Pinning exists to prevent:

- freeing the backing store while the kernel still uses it,
- resize/detach/transfer semantics from invalidating the OS-visible data pointer.

### 3) Host code stores only handles/roots (no raw GC pointers)

Async operations must not store raw pointers to GC-managed objects across yield
points. Instead they store:

- a GC-safe handle/root to the JS value (promise, ArrayBufferView) **and/or**
- a stable `BackingStore` handle (cloneable + reference-counted) plus `(offset, len)`.

Raw pointers derived from GC objects are only permitted inside a short-lived
**pinned view** object that enforces the pin lifetime.

### 4) Exclusive-borrow semantics for in-flight async I/O (data-race safety)

Pointer stability + pinning is necessary but not sufficient: async I/O backends like `io_uring`
allow the kernel to concurrently read/write user memory while JS code continues to execute.
If runtime/native code performs plain non-atomic loads/stores on a backing store while the kernel
is concurrently accessing it, Rust/LLVM may assume "no data races" and miscompile the program.

`runtime-native` therefore adopts **Model A: exclusive-borrow semantics** for buffers passed to
async I/O:

- Submitting an I/O operation borrows the backing store until completion/cancel.
- While borrowed, host-safe access to backing bytes is rejected.
- While borrowed, backing-store invalidation operations (`detach`, `transfer`, `resize`) are also
  rejected.

This applies to both single-buffer and vectored operations:
- `io::IoOp::{pin_backing_store_range, pin_vectored, pin_iovecs}` acquire **shared** read-borrows for
  write-like ops (`write(2)`, `send(2)`, ...).
- `io::IoOp::{pin_backing_store_range_for_read, pin_vectored_for_read, pin_iovecs_for_read}` acquire
  an **exclusive** write-borrow for read-like ops (`read(2)`, `recv(2)`, ...).

#### Deviation from Node/Web APIs

Node.js and Web APIs generally allow continued reads/writes to a `Buffer`/`Uint8Array` while an
async I/O operation is in flight. In `runtime-native`, this is *intentionally* disallowed (for the
native TS subset) to preserve a sound aliasing model.

APIs should therefore be shaped so the buffer is effectively moved into the I/O request and returned
on `await` (or completion), making misuse hard/obvious:

- `await fs.read(fd, buf)` returns `{ nread, buf }`
- `await fs.write(fd, buf)` returns `{ nwritten, buf }`

#### Implementation sketch

`BackingStore` tracks an atomic `borrow_state`:

- `READ_BORROW_COUNT`: shared borrows for ops where the kernel reads from the buffer.
- `WRITE_BORROWED`: exclusive borrow for ops where the kernel writes into the buffer.

I/O submission must acquire:

1. a **pin guard** (pointer stability / detach/resize exclusion), and
2. the appropriate **borrow guard** (`read` vs `write` direction).

Host-side non-I/O access must go through `try_with_slice` / `try_with_slice_mut`, which fail while
any I/O borrow is active.

These APIs take a callback that is generic over the slice lifetime (`for<'a>`), so safe Rust code
cannot return the `&[u8]` / `&mut [u8]` and hold it beyond the call (which would allow aliasing
across in-flight async I/O borrows).

Additionally, `try_with_slice` / `try_with_slice_mut` acquire an internal scoped borrow for the
duration of the callback so that **new** async I/O borrows cannot start while a safe Rust slice
reference is live.

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

## Vectored I/O (`iovec[]` / `msghdr`)

Some syscalls and async APIs require **descriptor structs** in addition to the underlying byte
buffers:

- `readv` / `writev` take an `iovec[]` array.
- `sendmsg` / `recvmsg` (and io_uring `SendMsg`/`RecvMsg`) take an `msghdr` which contains:
  - an `iovec[]` array (`msg_iov`)
  - optional `msg_name` (sockaddr bytes) and `msg_control` (ancillary data) buffers.

For synchronous syscalls the kernel typically copies some metadata, but for async APIs (and across
platforms) the safe contract is:

> the in-flight op owns all descriptor memory **and** pins all underlying buffers.

`runtime-native` provides GC-safe helpers that satisfy this contract:

- `io::PinnedIoVec` / `io::IoVecList`: owns a heap-allocated `Box<[libc::iovec]>` and holds
  backing-store pin guards for each referenced buffer range.
- `io::PinnedMsgHdr` (unix-only): owns a heap-allocated `Box<libc::msghdr>` plus the `PinnedIoVec`
  it points to, and optional `msg_name` / `msg_control` buffers.

These types are `Send` so they can be moved into I/O worker threads or stored in in-flight op
records until completion/cancellation.

### Aliasing / borrow invariants (io_uring + compiler safety)

- **Borrow blocks safe access:** while any I/O borrow is active, safe slice access APIs must
  deterministically fail.
- **Safe access blocks new I/O borrows:** while `try_with_slice` / `try_with_slice_mut` are executing
  their callback, attempting to start an async I/O borrow must fail (so safe Rust references can
  never overlap kernel I/O access).
- **Borrow released on all paths:** completion, cancellation, and drop paths must always release the
  borrow state (RAII guards).

### GC integration invariants

- **External memory is tracked:** backing store bytes are added to a GC-visible
  external-memory counter on allocation and removed on free.
- **Finalization drops only the handle:** when an `ArrayBuffer` header becomes unreachable, its
  finalizer must only drop its `BackingStore` handle. The backing store allocation (and external
  bytes accounting) must remain alive as long as any host pin guard still holds a strong reference.
- **Backing store pointers are not GC pointers:** the `ArrayBuffer` header contains a pointer/handle
  to a non-moving backing store allocation (malloc/mmap/etc). That field must **never** be treated
  as a GC-traced pointer:
  - it must not appear in the runtime's GC trace map / `TypeDescriptor.ptr_offsets`, and
  - it must not be passed as a `"gc-live"` value to LLVM statepoints (`gc.relocate` must never
    rewrite it).
- **GC does not need to scan backing store bytes:** backing store contents are
  treated as raw bytes and must not contain GC pointers.

## Suggested API shape (sketch)

This is not the only possible API, but whatever we implement must enforce the
invariants above.

```rust
use core::sync::atomic::AtomicU32;

/// Non-moving bytes backing an ArrayBuffer-like object.
struct BackingStore {
  base: NonNull<u8>,
  len: usize,
  pin_count: AtomicU32,
  // ... external memory accounting metadata ...
}

/// A stable, owned pin guard. Holding this keeps the OS-visible pointer valid.
struct PinnedRange {
  store: BackingStore, // cloneable strong handle
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

## Async I/O limiter accounting (DoS resistance)

In addition to GC external-memory accounting, `runtime-native` also implements an async I/O
**limiter** (`io::IoLimiter`) to defend against programs that start many long-lived operations and
thereby keep large external allocations alive until completion/cancel-ack.

Accounting contract:

- **Charge retained allocation size:** pinning/borrowing any range of a `BackingStore` retains the
  *entire allocation* against detach/transfer/free for the lifetime of the I/O op. Therefore limiter
  “pinned bytes” accounting must charge `BackingStore::alloc_len()` (not the user-specified range
  length).
- **Deduplicate within an op:** vectored ops may reference the same backing store multiple times;
  the limiter should charge each unique backing store at most once per op.

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
- **SharedArrayBuffer** (requires thread-safe memory model, atomics, and
  cross-thread lifetime; see [Future work](#future-work)).

**Note:** `runtime-native` *does* implement **ArrayBuffer detach/transfer** today
as internal/runtime APIs, and they preserve the pin-count invariant: detaching or
transferring while pinned deterministically fails with `*Error::Pinned` rather
than invalidating an in-flight I/O pointer. See
[`runtime-native/docs/buffers.md`](../../runtime-native/docs/buffers.md) for the
detailed ADR (detach/transfer behavior + pin-count semantics).

If/when resizable ArrayBuffers are added later, they must be designed to
preserve the same pinning invariants (e.g. “resize fails while pinned”).

## Future work

- **`io_uring` fixed buffer registration:** backing stores are a natural match
  for `IORING_REGISTER_BUFFERS`. Pins could be used to manage the registration
  lifetime and prevent unmapping while registered.
- **Zero-copy file `mmap` for buffers:** allow an `ArrayBuffer` backing store to
  be backed by an `mmap` region for large files. Requires careful accounting,
  explicit `msync`/flush semantics, and interaction with detachment/resizing.
- **SharedArrayBuffer:** likely reuses the non-moving backing store concept, but
  adds atomic operations, cross-thread publication, and stricter lifetime rules.
