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

Detaching drops the backing-store allocation. Detached buffers do not retain or expose the old
bytes.

### 3) Pin-count rule (critical)

Any operation that would change backing store identity or size must check `pin_count == 0`.

In MVP:

- `detach` and `transfer` return deterministic `*Error::Pinned` failures while pinned.
- resizable ArrayBuffers are **not supported yet**, but `ArrayBuffer::resize(..)` exists as a
  placeholder and still enforces the same pin-count check before returning `ArrayBufferError::Unimplemented`.

This ensures in-flight pinned buffers cannot be invalidated by detach/transfer/resize.

## Notes

This backing-store pin-count is distinct from GC pinning:

- GC pinning keeps **object addresses** stable.
- backing-store pinning keeps **byte pointers** stable and prevents detach/transfer/resize.
