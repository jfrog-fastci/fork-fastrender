# Runtime-native generational write barrier

This document specifies the **compiler/runtime ABI contract** for the generational GC write barrier (`rt_write_barrier`) used by `runtime-native` codegen.

- For the overall `runtime-native` ABI surface (exported symbols, call classifications, etc.), see `vendor/ecma-rs/docs/runtime-native.md`.
- For the authoritative stable C ABI declarations, see `include/runtime_native.h`.
- This document is the source of truth for the *write barrier itself* (arguments, required ordering, young-range mechanism).

It also specifies the semantics and policy defaults for **per-object card tables** intended for large pointer arrays (card size + representation). The exported barriers mark cards when a per-object card table pointer is installed on an object. The GC prototype installs card tables for large old pointer arrays and can scan/clear dirty cards, but the exported runtime ABI is not yet fully GC-integrated (e.g. `rt_gc_collect` does not yet run a full GC algorithm).

---

## Implementation status (runtime-native today)

`runtime-native` contains a prototype generational GC under `src/gc/*` (exercised by Rust tests), and the exported barrier is implemented.

- The exported symbols **`rt_write_barrier`** and **`rt_write_barrier_range`** exist (see `src/exports.rs`).
  - `rt_write_barrier` loads the stored pointer value from `slot` and performs the young-range fast-path checks described in this document.
  - On an old→young store it sets the `REMEMBERED` bit in the object header.
    - The exported barrier is **`NoGC`** and must not allocate. When the `REMEMBERED` bit transitions 0→1, it records `obj` into a fixed-capacity process-global remembered set without allocating (overflow aborts).
  - For objects with per-object card tables installed (`ObjHeader::card_table_ptr()` is non-null), it marks the relevant card dirty.
  - `rt_write_barrier_range` is a conservative post-bulk-write barrier: it marks all cards covering the written byte range (when a card table is present) and may over-mark cards.
 - The young-space range used by the barrier is configured via **`rt_gc_set_young_range`** / **`rt_gc_get_young_range`** (see below).
 - The exported symbol **`rt_gc_collect`** performs a stop-the-world handshake and can enumerate roots (via stackmaps), but does **not** yet run a full GC algorithm (mark/copy/etc).
   - `rt_alloc` / `rt_alloc_pinned` still use the milestone bump allocator.
   - `rt_alloc_array` is GC-backed.

---

## Background (why we need a barrier)

The GC is generational:

* **Nursery (young generation)** is collected with a **copying minor GC**.
* **Old generation** is collected separately (major GC).

During a minor GC the collector **does not scan all old objects**. Instead, it traces the nursery starting from:

* roots (stack/register roots, globals, etc.)
* a **remembered set** of old objects that may contain pointers into the nursery

This means **old→young edges must be tracked**. If an old object gains a pointer to a young object and we fail to record it, the minor GC can miss that young object and free/move it while it is still reachable.

Old-generation marks are **sticky across minor GCs**: a minor GC does not clear/redo the old-generation marking; it relies on the remembered set (and/or cards) to find the old→young edges relevant to the nursery collection.

---

## Young-space range

The runtime’s fast `is_young(ptr)` predicate is implemented as a simple address-range check against the **current nursery (young generation) range**:

```text
is_young(ptr) = (ptr >= young_start) && (ptr < young_end)
```

This range is stored in a pair of global atomics (see `src/gc/young.rs`) and is used by the exported write barrier (`rt_write_barrier`). The barrier uses the same range check both for the **stored value** and to classify the **base object** (`obj`) as young/old.

### ABI: `rt_gc_set_young_range`

The GC/runtime must keep this range up to date by calling the exported symbol **`rt_gc_set_young_range`**:

```c
// Authoritative declaration: include/runtime_native.h
void rt_gc_set_young_range(uint8_t* start, uint8_t* end);
```

This must be called:

- during runtime/GC initialization (before any mutator stores that may hit the barrier), and
- after each nursery flip/resize that changes the active young-space region.

If the range is not set correctly, `rt_write_barrier` will misclassify pointers and may fail to record old→young edges.

### Debug/test helper: `rt_gc_get_young_range`

If present, `rt_gc_get_young_range(uint8_t** out_start, uint8_t** out_end)` can be used by tests and debug tooling to read the current range. It is not intended for hot-path use.

---

## ABI: `rt_write_barrier`

### Signature

Stable C ABI (**authoritative**: `include/runtime_native.h`):

```c
void rt_write_barrier(uint8_t* obj, uint8_t* slot);
```

Rust side (for the exported symbol name / ABI):

```rust
#[no_mangle]
pub unsafe extern "C" fn rt_write_barrier(obj: *mut u8, slot: *mut u8);
```

### Call-site contract (MUST)

`rt_write_barrier` is called **after** the store.

* `obj` is the **object base pointer**: a `uint8_t*` that points at the start of the object header (`ObjHeader`).
* `slot` is the **address of the field location** that now contains the new pointer (i.e. a pointer to the slot).
* The barrier **reads the stored value** from `slot` (it is *not* passed as an argument).

Correct codegen shape (pseudo-code):

```c
// slot points at a pointer-sized field inside obj.
*(void**)slot = new_value;
rt_write_barrier(obj, (uint8_t*)slot);
```

The store and call must not be reordered: the barrier must observe the final value stored.

### Safety invariants required of callers

The barrier treats `slot` as a pointer slot and will load a pointer-sized value from it.

Callers must guarantee:

1. `obj` points to the start of a GC-managed heap object (the `ObjHeader` address).
2. `slot` points **within** the object described by `obj` (field area / inline element storage).
3. `slot` is aligned for a pointer-sized load (natural alignment).
4. The memory at `slot` contains either:
   * a valid GC pointer, or
   * `NULL` (0).

Violating these invariants is memory-unsafe.

---

## ABI: `rt_write_barrier_range`

### Signature

Stable C ABI (**authoritative**: `include/runtime_native.h`):

```c
void rt_write_barrier_range(uint8_t* obj, uint8_t* start, size_t len_bytes);
```

Rust side (for the exported symbol name / ABI):

```rust
#[no_mangle]
pub extern "C" fn rt_write_barrier_range(obj: *mut u8, start: *mut u8, len_bytes: usize);
```

### When to use

Use `rt_write_barrier_range` after **bulk pointer writes** that do not naturally expose per-slot stores, for example:

- Lowering of `Array.prototype.concat` / `push` loops
- Object/array spread lowering
- `memcpy`/`memmove` of composite values containing GC pointers

### Call-site contract (MUST)

`rt_write_barrier_range` is called **after** the bulk write.

- `obj` is the base pointer of the GC-managed object that was written.
- `start` points *within* `obj` to the first written byte (typically the first pointer slot).
- `len_bytes` is the number of bytes written starting at `start`.

### Semantics

Fast paths:

- If `obj` is young → return.
- If `len_bytes == 0` → return.

Slow path (old object):

- If the object has a per-object card table, mark **all cards covering** the written range dirty (atomically).
- Ensure the object is in the remembered set (idempotently via the header `REMEMBERED` flag).

`rt_write_barrier_range` is **conservative**: it does not inspect the values that were written and may over-mark cards. This is correct as long as the minor GC treats card marks as “may contain young” and scans/rebuilds/clears cards as needed.

If an object does not have a card table, `rt_write_barrier_range` falls back to remembering the whole object (idempotently).

---

## Runtime fast-path conditions

The barrier is expected to be cheap. It may return immediately if any of the following hold:

1. **Stored value is null** (no edge).
2. **Stored value is not young** (does not point into the nursery).
3. **Object is not old** (the base object is young, so the edge will be found by nursery tracing).

Only when `obj` is old *and* the stored value is young does the barrier perform the slow-path bookkeeping.

---

## Remembered-set semantics (non-array objects)

The minor collector traces nursery objects starting from roots plus a **remembered set** of old objects that may contain pointers into the nursery.

In Rust, this is modeled by the [`RememberedSet`](../src/gc/roots.rs) trait; tests typically use [`SimpleRememberedSet`](../src/gc/roots.rs).

### Header bit

* Each object has a header bit `REMEMBERED` (`ObjHeader::is_remembered()`).
* On an old→young store, the barrier sets `REMEMBERED`.
  * A remembered-set implementation may use this bit to ensure each object is added **at most once** (e.g. `SimpleRememberedSet`).

### Exported barrier status (important)

The exported `rt_write_barrier` sets the per-object `REMEMBERED` header bit and records newly
remembered objects into a fixed-capacity process-global remembered set without allocating (the
barrier remains `NoGC`). When a per-object card table is present, it marks the corresponding card.

Because the remembered set stores raw object pointers, tests that manually allocate/free mock
objects must clear it via `clear_write_barrier_state_for_tests` to avoid dangling pointers.

Full GC wiring for the exported runtime (`rt_gc_collect` and integration with the GC prototype) is still TODO.

### Minor GC behavior (current `GcHeap`)

`GcHeap::collect_minor` evacuates the entire nursery into old-gen and then resets the nursery. After a minor GC there are no remaining young objects, so the remembered set can be cleared (and `REMEMBERED` bits cleared) without scanning for “still-young” edges.

---

## Per-object card table semantics (pointer arrays)

Per-object card tables are intended for large objects with contiguous pointer storage (arrays, backing stores, etc.) to avoid rescanning the entire buffer on every minor GC. The exported barriers can mark cards when a per-object card table is installed, and the GC prototype can use dirty cards to scan only the relevant pointer-array regions. This section records the intended design and benchmark-driven defaults.

### Implementation status

Card marking in the exported barrier is implemented today:

- `rt_write_barrier` marks the single card that contains `slot` when `ObjHeader::card_table_ptr()` is non-null.
- `rt_write_barrier_range` conservatively marks **all cards covering** the written byte range (clamped to the object bounds) when a card table pointer is present. It does not inspect the values written and may over-mark.

The GC prototype auto-installs card tables for large old-generation pointer arrays (see `CARD_TABLE_MIN_BYTES`) and the minor collector can consult and clear dirty cards when scanning remembered objects. Card tables are not yet installed for all object kinds, and the exported runtime (`rt_gc_collect`) is still missing end-to-end GC integration.

### Semantics

Per-object card marking uses the following semantics:

- The object’s pointer storage is subdivided into fixed-size **cards**.
- A marked card means: **this card may currently contain one or more young pointers**.
  - It does **not** mean “this card has been written since the last GC”.

Intended minor GC behavior is to treat card marks as a “may contain young” summary:

- marked cards are scanned
- the runtime recomputes which cards still contain young pointers
- cards with no remaining young pointers are cleared

This keeps scanning proportional to the number of old-array regions that actually reference the nursery.

### Policy defaults

#### Card size

**Default:** `CARD_SIZE = 512 B`

We benchmarked 128 B (Immix line-sized), 512 B (common generational choice), and 1 KiB cards. In the `runtime-native/benches/card_table.rs` microbench, 512 B cards were consistently faster than 128 B for both marking and scanning due to:

- fewer card indices to compute and iterate
- less card-table metadata to walk per object

1 KiB was sometimes faster still, but it increases over-scanning when writes are sparse (each dirty mark forces scanning a larger region), so 512 B is the default compromise.

#### Representation

**Default:** **bitset** (1 bit per card, stored as `AtomicU64` words)

We benchmarked:

- byte-per-card (`u8` dirty flag)
- bitset (`u64` words)

The microbench showed:

- **Marking:** byte-per-card is cheaper (single store) than bitset (read/OR/write). In the benchmark (1 MiB buffer, 16k random slot marks), bitset was ~1.7× slower.
- **Scanning / rebuilding:** bitset is significantly faster at low dirty rates because it scans far less metadata and can skip all-zero words quickly. At 1% dirty, bitset was ~6× faster than byte-per-card for 512 B cards.

Minor GC performance is dominated by scanning, and large old objects typically have low dirty rates between minor collections, so the default is bitset.

### When to enable per-object card tables

Card tables are most valuable when:

1) the object contains a **large contiguous run of pointer slots** (arrays, property backing stores, etc), and
2) the mutation rate is **low enough** that only a small fraction of cards are marked between minor GCs.

Suggested starting heuristic:

- Enable a card table when a pointer buffer is at least **8× `CARD_SIZE`** (i.e. ≥ 4 KiB of pointers with the 512 B default), and the buffer is expected to survive into old-gen.

For smaller pointer buffers, scanning the whole object is usually cheaper than maintaining card metadata and running the barrier.

### Benchmarking

To reproduce the benchmarks locally:

```bash
bash vendor/ecma-rs/scripts/cargo_agent.sh bench -p runtime-native --bench card_table
```

---

## Promotion / tenuring

`rt_write_barrier` observes pointer **stores** performed by the mutator. It does not retroactively notice edges that become old→young due to a generation change.

If the GC can promote/tenure an object from young to old while still leaving some objects in the young generation, then the promoted object may already contain pointers into the nursery without any barrier firing. In that case promotion must explicitly register the object with the remembered-set policy:

* call `RememberedSet::on_promoted_object(obj, has_young_refs)` after scanning the promoted object.

Note: the current `GcHeap::collect_minor` implementation evacuates the entire nursery into old-gen and resets the nursery, so promoted objects cannot retain pointers into the (now empty) young space. The promotion hook exists for future policies and is exercised by `tests/promotion.rs`.

---

## Compiler write-barrier elimination rules

Native codegen must conservatively emit `rt_write_barrier` for any store that **might** create an old→young pointer.

The compiler may omit the barrier only when it can *prove* the store cannot create such an edge:

1. **NoEscape / stack-allocated objects → no barrier**
   * If an object is proven not to escape and is stack-allocated (or scalar-replaced), it is not a GC heap object.

2. **Stores into young objects → no barrier**
   * If the base object is known to be in the nursery, any edges will be discovered by nursery tracing.

3. **Stores of non-pointer types → no barrier**
   * Only GC-pointer stores need a write barrier. Integer/float/byte stores never do.

4. **RHS proven not-young → no barrier**
   * If the compiler can prove the stored value is not in the nursery, the store cannot create an old→young edge.
   * Examples:
     * `NULL`
     * global/immortal constants
     * allocations proven to be pretenured directly into old gen

Any uncertain case **must emit** the barrier.

### Interaction with pretenuring

Pretenuring only changes which generation an object is allocated into; it does not change the barrier rule.

If an allocation site is forced to allocate into old gen, the object begins life as **old**. Its initialization writes are just normal stores into an old object:

* Initialization stores of young values **must** emit `rt_write_barrier`.
* Initialization stores may omit the barrier only when the RHS is proven not-young (e.g. null or pretenured).

Do not assume that “initialization stores are safe” unless the compiler can prove the object is young (or non-escaping).
