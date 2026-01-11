# ADR: `runtime-native` buffers (ArrayBuffer detach/transfer + pin-count)

## Status
Accepted (MVP).

## Context / problem

Even with a non-moving GC, runtime code and host bindings may hand out raw pointers into an
`ArrayBuffer` backing store (e.g. to the OS for async I/O).

JavaScript-level operations like:

- **detach** (structured clone / postMessage transfer),
- **transfer** (ownership moves to a new buffer; old becomes detached), and
- future **resizable ArrayBuffers**

can invalidate these pointers by swapping/freed/reallocating the backing store.

For correctness, in-flight async I/O must not observe the backing store being invalidated.

## Decision (MVP)

We implement a minimal backing-store state machine and enforce an explicit pin-count rule:

### 1) Detach/transfer are implemented (internal/runtime APIs)

Backing store state:

```
Alive  -> Detached
```

- `ArrayBuffer::is_detached()` reports the state.
- `ArrayBuffer::detach()` transitions to `Detached`.
- `ArrayBuffer::transfer()` moves the bytes into a new `ArrayBuffer` and detaches the original.

When a buffer is detached:

- `byte_len == 0`
- `ArrayBuffer::data_ptr()` fails with `ArrayBufferError::Detached`
- pinning (`ArrayBuffer::pin()`) fails with `ArrayBufferError::Detached`
- typed array views over the buffer observe `length/byte_length == 0` and element reads behave as
  out-of-bounds (`Ok(None)` / "undefined-like").

### 2) Backing store memory is freed on detach

Detaching drops the `ArrayBuffer` header's `BackingStore` handle. The backing store allocation is
freed when the last strong `BackingStore` handle is dropped (including any in-flight pin guards).

Detached buffers do not retain or expose the old bytes at the JS/typed-array API level.

### 3) Pin-count rule (critical)

Any operation that would change backing store identity or size must check `pin_count == 0`.

In MVP:

- `detach` and `transfer` return deterministic `*Error::Pinned` failures while pinned.
- `detach`, `transfer`, and `resize` are also rejected while the backing store is I/O-borrowed
  (`BorrowError::Borrowed`). (In the async I/O layer, borrows and pins are both held for the
  duration of the op; pinned takes precedence in error reporting.)
- resizable ArrayBuffers are **not supported yet**, but `ArrayBuffer::resize(..)` exists as a
  placeholder and still enforces the same pin-count check before returning `ArrayBufferError::Unimplemented`.

This ensures in-flight pinned buffers cannot be invalidated by detach/transfer/resize.

### 4) GC finalization defers free while pinned

Even if the `ArrayBuffer` header becomes unreachable (and its finalizer runs), the backing store
must not be freed while it is pinned for in-flight I/O.

In `runtime-native`, the backing store is an independently-owned, reference-counted object:

- the GC finalizer drops the header's `BackingStore` handle (making the buffer detached)
- each pin guard holds its own strong `BackingStore` handle, keeping the allocation alive
- the allocation is freed when the last handle is dropped (with `pin_count == 0` asserted at drop)

### 5) Exclusive-borrow semantics for in-flight async I/O (data-race safety)

Pinning is necessary for pointer stability, but not sufficient for memory safety.

Async I/O backends (e.g. `io_uring`) allow the kernel to concurrently read/write user-space memory
while JS continues executing. If runtime/native code performs plain non-atomic loads/stores on a
buffer while the kernel is concurrently accessing it, Rust/LLVM may assume "no data races" and
miscompile the program.

`runtime-native` therefore adopts **Model A: exclusive-borrow semantics** for buffers passed to
async I/O:

- Submitting an async I/O operation borrows the backing store until completion/cancel (RAII guards).
- While borrowed, safe access to backing bytes is rejected (e.g. `ArrayBuffer::data_ptr`,
  `ArrayBuffer::try_with_slice`, `ArrayBuffer::try_with_slice_mut`).

Borrow kinds:

- **Read borrows**: shared (multiple concurrent ops allowed) for operations where the kernel
  *reads from* the buffer (e.g. `write`).
- **Write borrows**: exclusive for operations where the kernel *writes into* the buffer
  (e.g. `read` / `recv`).

#### Deviation from Node/Web APIs

Node.js and Web APIs typically allow user code to keep reading/writing a buffer while an async I/O
operation is in flight. `runtime-native` intentionally disallows this for the native TS subset to
preserve a sound aliasing model.

Host APIs should be shaped so buffers are effectively moved into I/O requests and returned on
completion (e.g. `await fs.read(fd, buf)` returns `{ nread, buf }`).

## Notes

This backing-store pin-count is distinct from GC pinning:

- GC pinning keeps **object addresses** stable.
- backing-store pinning keeps **byte pointers** stable and prevents detach/transfer/resize.
