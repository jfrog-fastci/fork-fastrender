# Buffers and I/O (stable pointers under a moving GC)

For the canonical buffer/I/O invariants (pin-count protocol, cancellation rules,
etc.), see:
- [`docs/runtime-native/buffers-and-io.md`](../../docs/runtime-native/buffers-and-io.md)
- [`docs/buffers.md`](./buffers.md) (ADR: detach/transfer + pin-count semantics)

JavaScript `ArrayBuffer` / `TypedArray` objects are frequently used as I/O buffers (filesystem,
sockets, async runtimes like `io_uring`, etc.). Many OS APIs require the buffer memory to remain
valid at a **stable virtual address** until the kernel has finished using it.

In a moving GC (Immix-inspired with opportunistic copying), **GC object pointers are not stable**:
objects can be relocated during collection. This means `ArrayBuffer` bytes cannot live inside the
moving heap when they are passed to the OS by raw pointer.

## Design: movable header + non-moving backing store

`runtime-native` splits a buffer into:

- **Header object** (`buffer::ArrayBuffer`, `buffer::Uint8Array`):
  - Intended to be allocated in the GC heap (and therefore movable).
  - Contains lengths/offsets and a handle/pointer to the backing store.
- **Backing store** (`buffer::BackingStore`):
  - Allocated outside the moving heap using a `BackingStoreAllocator`.
  - The pointer is stable for the lifetime of the backing store.
  - Stored as a plain “bitwise movable” handle inside the header.

This allows the GC to relocate the **header** while the underlying byte pointer remains valid for
kernel I/O.

## Alignment

All runtime-native backing store allocations are at least **16-byte aligned**
(`BACKING_STORE_MIN_ALIGN = 16`). This provides a predictable baseline alignment for:

- syscall buffer requirements
- future SIMD/vectorized typed-array operations

When adopting existing buffers (e.g. `Vec<u8>` / `Box<[u8]>`), runtime-native will only keep the
allocation without copying if the pointer is already 16-byte aligned. Otherwise it will allocate a
fresh aligned buffer and copy.

## Accounting and finalization

Backing store bytes live outside the GC heap but still contribute to process memory pressure. Each
allocator reports the total currently-owned backing store bytes via `BackingStoreAllocator::external_bytes()`.

When an `ArrayBuffer` header becomes unreachable, its backing store must be released **exactly
once**. The current milestone runtime does not yet have a per-type finalizer mechanism wired into
the GC, so `buffer::ArrayBuffer` exposes an explicit `finalize(_in)` API that a future GC finalizer
should call.

If finalization runs while the backing store is pinned (in-flight I/O), freeing is **deferred**:
the `ArrayBuffer` header drops its handle and becomes detached, but the backing store allocation
remains alive because pin guards keep a strong reference. The actual deallocation happens only when
the last `BackingStore`/pin guard is dropped.

## Using buffers for I/O

To obtain a `(ptr, len)` pair for kernel I/O:

- Create an `ArrayBuffer` (`ArrayBuffer::new_zeroed`, `ArrayBuffer::from_bytes`, etc.).
- Create a typed view (`Uint8Array::view`).
- Use either:
  - `Uint8Array::as_ptr_range()` for **immediate** / synchronous I/O (not pinned), or
  - `Uint8Array::pin()` / `ArrayBuffer::pin()` for async I/O (pinned).

`Uint8Array::as_ptr_range()` is *not* sufficient for async I/O: it does not pin
the backing store, so the byte pointer can be invalidated by detach/transfer/resize
or GC finalization while an operation is in flight.

Pinned guards (`PinnedUint8Array`, `PinnedArrayBuffer`) increment the backing
store pin count, keeping the bytes alive and forcing detach/transfer/resize to
fail deterministically with `*Error::Pinned` until the guard is dropped.

The byte pointer itself comes from the non-moving backing store, so it is stable
for as long as the backing store remains alive and is not detached/transferred/resized;
pinning is what makes that lifetime explicit for async I/O.

## Vectored I/O (`iovec[]` / `msghdr`)

Some syscalls and async APIs require **descriptor structs** in addition to the underlying byte
buffers:

- `readv` / `writev` take an `iovec[]` array.
- `sendmsg` / `recvmsg` (and io_uring `SendMsg`/`RecvMsg`) take an `msghdr` which contains:
  - an `iovec[]` array (`msg_iov`)
  - optional `msg_name` (sockaddr bytes) and `msg_control` (ancillary data) buffers.

Even if the kernel copies *some* metadata, implementations differ; the safest contract for async
I/O is:

> the in-flight op owns all descriptor memory **and** pins all underlying buffers.

`runtime-native` provides GC/io_uring-safe helpers in `io::`:

- `io::PinnedIoVec` / `io::IoVecList`: owns a heap-allocated `Box<[libc::iovec]>` and holds pin
  guards for each referenced `ArrayBuffer`/`Uint8Array` range.
- `io::PinnedMsgHdr` (unix-only): owns a heap-allocated `Box<libc::msghdr>` and the `PinnedIoVec` it
  points to, plus optional `msg_name` / `msg_control` buffers.

These types are designed to be stored inside an in-flight `io::IoOp` so descriptor/buffer lifetimes
are tied to completion/cancellation.
