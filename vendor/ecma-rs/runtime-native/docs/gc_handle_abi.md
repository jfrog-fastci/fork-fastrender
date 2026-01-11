# runtime-native GC ABI (moving GC safe)

When `native-js` calls into `runtime-native`, it does so from code compiled with LLVM
GC statepoints/stackmaps. The `runtime-native` crate itself is plain Rust and is
**not** compiled with statepoints.

That means a moving GC can only precisely locate/update roots in *native-js
generated frames* (via stackmaps). Any `runtime-native` exported function that:

- can allocate / trigger GC, **and**
- needs to use GC-managed pointers passed as arguments

must not assume raw pointers remain valid across the call.

This document defines the ABI rules that make runtime-native calls safe under a
moving GC.

---

## Function classes

Every exported runtime-native function must be categorized as exactly one of:

### `may_gc`

Functions that may:

- allocate in the GC heap
- trigger or participate in a GC cycle
- call other `may_gc` functions

**ABI rule:** `may_gc` functions must **not** accept raw GC pointers (`*mut u8`)
as arguments *unless* they are:

- passed as **handles** (pointer-to-slot, see below), or
- passed as **pinned** pointers (only as an explicit, well-documented fallback)

### `no_gc`

Functions that must **not**:

- allocate in the GC heap
- trigger GC / participate in a GC cycle
- call `may_gc` functions

**ABI rule:** `no_gc` functions may accept raw GC pointers because objects are
guaranteed not to move during the call (since no GC occurs).

Note: `no_gc` describes *GC behavior*, not "never calls `malloc`". A `no_gc`
function may still allocate in the Rust/system heap, but it must not execute a
safepoint that could relocate GC-managed objects while it is using raw pointers.

For the **codegen-facing** classification table used by `native-js` (which may be
conservative), see:

- `docs/runtime-native.md` (“GC classification for codegen”)

---

## ABI strategy options

There are multiple viable strategies for making boundary calls safe. We want the
rules to be obvious from the exported signatures.

### A) Handle ABI (default)

Pass GC-managed pointer arguments as **handles**:

- `GcHandle = *mut *mut u8`
- i.e. a pointer to a *caller-owned* stack/root slot that contains a GC pointer

Properties:

- The caller owns the slot and keeps it live as a GC root.
- During a GC, the collector updates the slot (using stackmap information from
  the caller's statepoint).
- The runtime can safely **reload** `*handle` after any allocation / safepoint.

Implications:

- `may_gc` exported functions take `GcHandle` for any GC-managed pointer argument.
- Returning a raw GC pointer is fine; the caller must store it into a root slot
  before any further safepoint.

Limitations:

- A `GcHandle` is only valid for the duration of the call. Do **not** store
  `GcHandle` values in heaps/queues/async tasks.
- For long-lived references from host code, use a persistent root mechanism such
  as `rt_gc_register_root_slot` / `rt_gc_pin` (see `include/runtime_native.h`) or
  the runtime's handle table (`gc::HandleTable`).

### B) Preflight GC / non-GC allocation fastpath

Avoid GC while inside runtime-native helpers:

- Ensure all collection happens at explicit safepoint calls (e.g. `rt_gc_collect`).
- Allocation helpers return failure (or a slowpath token) instead of triggering GC.

Properties:

- Allows `may_gc`-like helpers to take raw pointers as long as they never trigger GC.
- Requires a more complex allocation protocol and tight discipline: *nothing*
  inside helpers may GC unless it is an explicit safepoint boundary.

### C) Pinning at the boundary (fallback / FFI)

Pin GC objects for the duration of a call:

- Temporarily mark objects as non-movable
- Pass raw pointers safely while pinned

Properties:

- Useful for FFI or "escape hatch" native integrations.
- Can harm GC performance/compaction; should be a last resort.

---

## Default for this project (initial implementation)

We adopt **(A) Handle ABI** as the default for `runtime-native` exports.

Rationale:

- Moving-GC correctness is explicit in the signature: `GcHandle` vs `GcPtr`.
- Keeps GC logic centralized in the collector + stackmap integration.
- Avoids relying on "this helper never allocates" as an ambient assumption.

---

## Naming and signature conventions

To make call sites obvious and grep-friendly:

- Prefer `*_h` for new exported `may_gc` functions that accept GC-managed pointer
  arguments as handles.
- Existing exported functions without a suffix must be documented/categorized as
  `may_gc` vs `no_gc` in code reviews and docs.

Examples:

- `rt_gc_safepoint_relocate_h(slot: GcHandle) -> GcPtr` (`may_gc`, handle-based)
- `rt_write_barrier(obj: GcPtr, slot: *mut u8)` (`no_gc`, raw pointers ok)

---

## Codegen implications (native-js)

`native-js` must:

1. Materialize any GC pointer that crosses a `may_gc` boundary into an
   address-taken slot (a GC root).
2. Pass `&mut slot` as a `GcHandle`.
3. Treat any returned GC pointer as an *unrooted* value until it is stored in a
   root slot.

This keeps runtime-native calls safe even when objects move during GC.
