# Runtime-native `io_uring`: buffer lifetime + cancellation semantics + GC coordination (moving GC)

This doc is a **memory-safety + GC-correctness contract** for any runtime-native
async I/O implementation that submits Linux `io_uring` SQEs while running under a
**compacting/moving GC**.

In addition to pointer lifetime and cancellation rules, it defines the required
stop-the-world (STW) coordination for threads that may block in
`io_uring_enter(..., IORING_ENTER_GETEVENTS, ...)` waiting for CQEs.

The goal is to make it *impossible* (by construction) to hand the kernel a pointer
into GC-managed memory that may move or be freed while the kernel can still
dereference it.

Non-goals (v1):

- Zero-copy send (`SEND_ZC` / `SENDMSG_ZC`) (kept **disabled by default**; only enabled behind an
  explicit feature gate with notification-driven buffer release)
- Multi-shot ops (`ACCEPT`/`RECV`/`READ` multishot, etc.)
- Any implicit buffer reuse API (buffer selection / provided buffers) without a
  dedicated lifetime model

---

## Why this exists

`io_uring` lets user space submit requests containing **raw pointers** to user
memory (buffers, iovec arrays, path strings, sockaddr structs, etc.). The kernel
may dereference those pointers **asynchronously** after submission.

With a **moving GC**, an object’s address is not stable across safepoints/GC
cycles. If we submit an SQE that points into GC-managed memory and the GC moves
or frees that memory before the kernel is done, we risk:

- kernel writing into freed memory (UAF / corruption)
- kernel reading stale memory (data leak / wrong I/O)
- process crash (SIGSEGV) or silent corruption

Therefore, the runtime must enforce a strict lifetime discipline.

---

## Core rule (conservative): “stable until CQE observed”

For this project, treat **every user pointer reachable from an SQE** as requiring
that the referenced memory:

1. remains **allocated and writable/readable** as required by the op, and
2. remains at the **same virtual address**

**from the moment the SQE is submitted** until the moment the runtime
**observes the CQE for that specific request** (success or failure).

Exception: `IORING_OP_SEND_ZC` can keep user pages pinned *past* the initial CQE. Buffers must remain
stable until the **notification CQE** (`IORING_CQE_F_NOTIF`) is observed.

> “Observed” means: the CQE entry has been read from the completion queue and
> dispatched to the owning `IoOp` (or equivalent op-tracking state). It is not
> sufficient for the kernel to have *written* the CQE if user space has not yet
> consumed it.

Even if the kernel *might* copy some metadata at submission time, we do **not**
depend on that for correctness. We program to the conservative contract above.

Implementation note (Rust drivers in this repo):

- Both `runtime-io-uring::IoUringDriver` and the legacy `runtime-io-uring::Driver` implement
  **leak-on-drop when in-flight**: if a driver is dropped while there are still pending ops, it
  leaks the ring and in-flight op state rather than freeing kernel-referenced pointers early.

---

## Lifetime rules by op family

This section lists common pointer-carrying operations used by a runtime. The rule
is always the same: **everything reachable via pointers must stay stable until
the op’s CQE is observed**.

### 1) Buffer ops: `READ` / `WRITE` / `RECV` / `SEND`

Applies to:

- `IORING_OP_READ`, `IORING_OP_WRITE`
- `IORING_OP_RECV`, `IORING_OP_SEND`

Kernel-referenced user memory:

- `buf` (data buffer pointer)

Rule:

- The data buffer must remain valid and at a stable address **until the CQE for
  that op is observed**.
- For read-like ops (`READ`/`RECV`): the buffer must be writable for the full
  duration (kernel writes into it).
- For write-like ops (`WRITE`/`SEND`): the buffer must remain readable and must
  not be mutated if the program expects a coherent snapshot.

### 2) Vectored I/O: `READV` / `WRITEV`

Applies to:

- `IORING_OP_READV`, `IORING_OP_WRITEV`

Kernel-referenced user memory (conceptually):

- pointer to iovec array (`struct iovec *`)
- each `iov_base` data buffer

Kernel behavior note:

- The kernel may copy the iovec metadata internally at submission time, but this
  is not a portable contract across versions/configs for our purposes.

Rule (project policy):

- Treat **both** the iovec array **and** all `iov_base` buffers as requiring
  stability **until the CQE is observed**.

This avoids subtle bugs if we later change flags, use fixed buffers, add
personality/selection features, or hit kernel-side refactors.

### 3) Message-based socket I/O: `SENDMSG` / `RECVMSG`

Applies to:

- `IORING_OP_SENDMSG`, `IORING_OP_RECVMSG`

Kernel-referenced user memory:

- `struct msghdr` itself
- nested iovec array (`msg_iov`) and each `iov_base` buffer
- `msg_name` (peer address buffer) if non-null
- `msg_control` (ancillary data buffer) if non-null
- any length/output fields the kernel may read or write (practically: treat the
  whole `msghdr` as mutable kernel-owned for the duration)
- any additional pointers passed by the op variant (e.g. APIs that take a
  `socklen_t*` out-parameter)

Rule (project policy):

- Treat the **entire `msghdr` graph** as requiring stability until the CQE is
  observed:
  - the `msghdr`
  - every nested iovec array
  - every referenced data buffer
  - `msg_name`, `msg_control`
  - any length pointers/out-params

### 4) Socket address / out-param ops: `ACCEPT` / `CONNECT`

Applies to:

- `IORING_OP_ACCEPT`
- `IORING_OP_CONNECT`

Kernel-referenced user memory:

- `sockaddr *` (address buffer) — read for `CONNECT`, written for `ACCEPT`
- `socklen_t *` (length in/out pointer) — typically read+written for `ACCEPT`

Rule:

- `sockaddr` buffers and `socklen_t*` out-params must remain stable until CQE.

### 5) Pathname ops: `OPENAT` / `OPENAT2` / `STATX` / `UNLINKAT` / `RENAMEAT` / …

Applies to common filesystem ops that take path pointers, such as:

- `IORING_OP_OPENAT`, `IORING_OP_OPENAT2`
- `IORING_OP_STATX`
- `IORING_OP_UNLINKAT`, `IORING_OP_RENAMEAT`, `IORING_OP_MKDIRAT`
- `IORING_OP_SYMLINKAT`, `IORING_OP_LINKAT`

Kernel-referenced user memory:

- path string pointer(s) (typically `const char *`)
- op-specific structs referenced by pointers (e.g. `open_how*` for `OPENAT2`)

Rules:

- Any path bytes passed to the kernel must live in **non-moving memory** (or be
  pinned) and remain stable until CQE.
- Do not pass pointers into GC-managed string storage unless it is explicitly
  pinned and guaranteed to be NUL-terminated in the expected encoding.
  In practice: allocate an owned C-string buffer (`Vec<u8>`/`Box<[u8]>`) per op.

### 6) Timeout/time structs: `TIMEOUT` / `LINK_TIMEOUT`

Applies to:

- `IORING_OP_TIMEOUT`
- `IORING_OP_LINK_TIMEOUT`

Kernel-referenced user memory:

- pointer to a timespec-like struct (e.g. `__kernel_timespec`)

Rule:

- The timespec struct must remain stable until the CQE is observed.

### 7) “If it has a pointer, it’s stable until CQE”

`io_uring` grows new opcodes over time (xattr ops, `SOCKET`, `URING_CMD`, etc.).
The safe default for this project is:

- If an SQE field (directly or indirectly) contains a **user pointer**, treat
  the referenced memory as requiring stability until CQE.
- Do not use a new pointer-carrying opcode in runtime-native I/O without first
  auditing and documenting its pointer graph and lifetime.

---

## Cancellation semantics: `IORING_OP_ASYNC_CANCEL`

Cancellation in `io_uring` is **best-effort** and inherently racy.

### Key facts a runtime must implement correctly

1. **Cancel races with completion.**
   - The target request may complete successfully while a cancel is in flight.
   - The cancel itself may “fail” even if the target will later complete with
     `-ECANCELED` (or vice versa), depending on timing and how the target was
     issued.

2. **The cancel SQE has its own CQE.**
   - That CQE only means “the kernel processed the cancel request”.
   - It does **not** mean the target request is done.

3. **Buffer/pin/GC-root lifetimes follow the *target* CQE, not the cancel CQE.**
   - All memory reachable from the target SQE must remain stable until the
     target’s CQE is observed, even if:
     - the cancel CQE arrives first, or
     - the cancel CQE reports success (`res == 0`).

4. **The target request will still produce a CQE.**
   - If cancellation succeeds, the target CQE typically reports `res ==
     -ECANCELED`.
   - Regardless of whether it’s canceled or completes normally, **do not release
     resources until the target CQE is observed**.

### Interpreting `cancel_cqe.res` conservatively

The cancel CQE `res` is primarily about whether the kernel found/processed a
matching request. Treat it as *advisory*.

Common outcomes:

- `res == 0`: cancel request was accepted and (at least one) target was found.
  - **Still wait for the target CQE** before releasing target buffers/pins.

- `res == -ENOENT`: no matching in-flight request was found.
  - This can mean:
    - the target already completed and its CQE is pending in the CQ ring, or
    - the target was never in flight (bug or logic race), or
    - the target completed concurrently with the cancel attempt.
  - Correct handling:
    - If the runtime still considers the target op “in-flight”, keep its
      resources and wait for its CQE.
    - If the runtime has already observed the target CQE, treat cancel as a
      no-op.

- `res == -EALREADY` (or other negative errno): cancellation could not be applied
  (already canceled, not cancelable, invalid flags, etc.).
  - Correct handling: **do not infer anything about buffer lifetime**; wait for
    the target CQE as usual.

### Recommended cancellation API shape (runtime perspective)

Expose cancellation as “request cancellation” rather than “synchronous stop”:

- `cancel(op_id) -> Future<CancelOutcome>`
  - resolves when the **target op** completes (success, error, or `-ECANCELED`)
  - may additionally surface whether the cancel SQE found the target

This matches the fundamental invariant: target buffers/pins are released only at
target completion.

---

## GC coordination: blocking waits (`io_uring_enter` / `IORING_ENTER_GETEVENTS`)

Under `runtime-native`, STW GC coordination is **cooperative**: mutator threads
must reach safepoints (via `safepoint_poll`) so the GC can proceed. A mutator
thread blocked in a syscall cannot poll and can therefore deadlock a GC request.

An `io_uring` backend commonly blocks while waiting for CQEs by calling:

- `io_uring_enter(..., flags = IORING_ENTER_GETEVENTS, ...)` directly, or
- helper APIs that wrap it (e.g. `io_uring_wait_cqe*`).

**Contract:** any thread that may block in
`io_uring_enter(..., IORING_ENTER_GETEVENTS, ...)` MUST be treated as quiescent
for STW **or** MUST be wakeable by the runtime so it can reach a safepoint
promptly.

### Acceptable patterns for “quiescent while blocked”

Both of these patterns are acceptable for threads that block waiting for CQEs.

#### A) Parked at a known safepoint (mutator is idle)

Before entering a potentially blocking `io_uring_enter(...GETEVENTS...)`:

- Mark the thread parked: `runtime_native::threading::set_parked(true)`.
- The call site MUST be a known safepoint where the thread has **no untracked GC
  pointers** in registers/stack.

After the syscall returns:

- Un-park: `runtime_native::threading::set_parked(false)`.
- Poll a safepoint **before** resuming any mutator work:
  `runtime_native::threading::safepoint_poll()`.

Connection to the buffer/pin rules in this document:

- While the wait thread is blocked, the in-flight op state (`IoOp` table) may
  continue to hold **GC roots and pin guards** to satisfy “stable until CQE
  observed”.
- However, when using the **parked** mechanism the waiting thread itself must
  not hold *untracked* GC pointers in locals/registers/stack frames during the
  block, because the safepoint coordinator is allowed to treat parked threads as
  already quiescent.

#### B) Enter a GC-safe region (mutator must not touch the heap)

Before entering a potentially blocking `io_uring_enter(...GETEVENTS...)`:

- Enter a GC-safe region: `let _g = runtime_native::threading::enter_gc_safe_region();`.

While in the GC-safe region:

- The thread MUST NOT touch the GC heap (no GC allocations, no dereference of
  movable GC objects, no write barriers).
- Any GC state that must outlive the blocking wait must be represented via
  explicit roots/handles/pins elsewhere (e.g. in the op state), not by keeping a
  raw pointer live in a stack local.

### GC → reactor wake hook (must exist)

Even if the waiting thread is treated as quiescent, the runtime should be able
to **wake** the `io_uring` wait primitive promptly when a GC request begins, to
avoid relying on unbounded syscall latency and to allow the reactor to observe
and respond to GC state transitions quickly.

`runtime-native` already exposes a hook for this:

- `runtime_native::threading::register_reactor_waker(fn())`

An `io_uring` implementation should register a waker that causes a blocked
`io_uring_enter(...GETEVENTS...)` to return promptly.

High-level wake strategies (one example):

- **Dedicated `eventfd` + permanently armed `IORING_OP_POLL_ADD`:**
  - Create an `eventfd` used only for wakes.
  - Keep one `IORING_OP_POLL_ADD` in flight on that `eventfd` for `POLLIN`.
  - `wake()` writes to the `eventfd`.
  - The poll op completes, producing a CQE, which breaks the blocking wait.
  - The reactor drains the `eventfd` and re-arms the poll op.

Other wake strategies are acceptable if they are race-free, do not require
touching the GC heap from the waker callback, and ensure bounded wake latency.

### Minimal invariants (should be testable)

- **No GC deadlock:** a stop-the-world GC request must not deadlock while any
  runtime thread is blocked in `io_uring_enter(..., IORING_ENTER_GETEVENTS, ...)`.
- **Quiescent or wakeable:** any such blocked wait is either surrounded by
  `set_parked(true)`/`set_parked(false)` (with a safepoint poll after un-parking)
  or occurs in a GC-safe region.
- **Waker registered:** the `io_uring` reactor registers a GC-triggered waker
  (`register_reactor_waker`) that reliably breaks the blocking wait.

---

## Sharp edges to avoid in v1

These features require additional lifetime tracking beyond “stable until the op’s
CQE”.

### `SEND_ZC` / zero-copy send

`IORING_OP_SEND_ZC` (and related zerocopy variants) can keep user buffers alive
*past* the initial operation CQE because the kernel may complete the send
asynchronously via later notifications.

Policy:

- Treat `SEND_ZC` as a separate lifetime class: the buffer must remain stable until the
  **notification CQE** (`IORING_CQE_F_NOTIF`) is observed (not just the send CQE).
- In `runtime-io-uring`, zerocopy send support is feature-gated (`send_zc`) and holds the buffer
  until notification. Keep it disabled unless this extended lifetime is explicitly required.

### Multi-shot ops

Multi-shot operations (e.g. accept multishot, recv multishot) produce **multiple
CQEs** from a single submission and can keep buffers alive across multiple
completions.

Policy:

- Do not enable multishot in v1.
- Only enable once we have:
  - a dedicated API that makes buffer reuse explicit, and
  - a clear “final CQE / shutdown” signal for releasing pinned memory.

---

## Recommended runtime-native API contract

This is the contract the runtime-native `io_uring` layer should implement.

### `IoOp` ownership model

Define a per-request object (`IoOp`) that owns everything the kernel might
reference.

**`IoOp` MUST:**

1. **Own all kernel-referenced non-GC structs in non-moving memory.**
   Examples:
   - iovec arrays
   - `msghdr` and any nested iovecs
   - sockaddr buffers, `socklen_t` out-params
   - `open_how`, timespec, etc.

2. **Hold GC roots + pin guards for any GC-managed backing stores whose
   addresses must not change.**
   Examples:
   - `Uint8Array` / `ArrayBuffer` backing store used as a read/write buffer
   - any GC-managed string storage (prefer copying to a C-string buffer instead)

3. **Release pins/roots only after the target CQE is observed.**
   - Cancellation does not change this.

4. **Never store raw moving-GC pointers in `user_data`.**
   - `SQE.user_data` must be either:
     - a stable integer ID that indexes an op table, or
     - a pointer to a non-GC `IoOp` allocation (only if the pointer is stable and
       lifetime-managed).

### Minimal invariants (should be testable)

These are the invariants we should be able to assert in tests later:

- **Submission invariant:** Every SQE submitted to the kernel has an owning
  `IoOp` that:
  - outlives the kernel’s use of the SQE’s pointer graph, and
  - keeps all required GC pins held.

- **Completion invariant:** An `IoOp` is removed from the in-flight table and
  releases its pins **only after** its CQE has been observed.

- **Cancellation invariant:** Observing a cancel CQE never releases the target
  op’s resources; only the target CQE does.

- **`user_data` invariant:** `user_data` does not encode a direct pointer into
  moving GC memory.

### Sketch (Rust-ish pseudocode)

```rust
/// Non-GC, non-moving per-request state.
struct IoOp {
    id: u64, // stored in SQE.user_data
    // Non-moving allocations referenced by the kernel:
    c_path: Option<Box<[u8]>>,
    iovecs: Option<Box<[libc::iovec]>>,
    msghdr: Option<Box<libc::msghdr>>,
    sockaddr: Option<Box<[u8]>>,
    socklen: Option<Box<libc::socklen_t>>,
    // GC integration:
    gc_roots: Vec<GcRoot>,
    pins: Vec<GcPinGuard>,
}

/// Submitting an op installs it into an in-flight map until CQE.
fn submit(op: Box<IoOp>, sqe: &mut io_uring_sqe) {
    // 1) Establish pins/roots BEFORE filling SQE pointers.
    // 2) Fill SQE pointers from op-owned allocations or pinned GC buffers.
    sqe.user_data = op.id;
    inflight.insert(op.id, op);
    ring.submit();
}

/// Completion dispatch releases resources only at target CQE.
fn on_cqe(cqe: io_uring_cqe) {
    let op = inflight.remove(&cqe.user_data).expect("unknown cqe");
    // op drops here => pins released here (after CQE).
}
```

The details differ per runtime, but the lifetime shape must match.

---

## Checklist for implementers

- [ ] Every pointer written into an SQE is either:
  - into `IoOp`-owned non-moving memory, or
  - into a GC object protected by a pin guard for the full op lifetime.
- [ ] `IoOp` is kept alive in an in-flight table keyed by a stable `user_data`.
- [ ] No “early free” paths exist (including cancel completion).
- [ ] v1 does not expose zerocopy send or multishot operations.
- [ ] Any thread that may block in `io_uring_enter(..., IORING_ENTER_GETEVENTS, ...)` is either:
  - marked parked at a known safepoint (`runtime_native::threading::set_parked(true)`), then
    un-parked and safepoint-polled (`runtime_native::threading::safepoint_poll()`) before resuming
    mutator work, **or**
  - in a GC-safe region (`runtime_native::threading::enter_gc_safe_region()`).
- [ ] The `io_uring` backend registers a GC-triggered reactor waker via
  `runtime_native::threading::register_reactor_waker` (e.g. eventfd + `IORING_OP_POLL_ADD`).
