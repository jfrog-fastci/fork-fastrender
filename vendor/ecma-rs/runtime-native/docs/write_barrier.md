# Runtime-native generational write barrier

This document specifies the **compiler/runtime ABI contract** for the generational GC write barrier used by runtime-native codegen.

The intent is to make it unambiguous:

* **When** native code must call the barrier.
* **What** arguments it must pass (and why it is safe).
* **Which** compiler optimizations / eliminations are allowed.

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

## ABI: `rt_write_barrier`

### Signature

Stable C ABI (see also `include/runtime_native.h`):

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

* `obj` is the base pointer of the GC-managed object containing the field being written.
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

1. `obj` points to the start of a GC-managed heap object (including GC-managed arrays).
2. `slot` points **within** the object described by `obj` (field area / inline element storage).
3. `slot` is aligned for a pointer-sized load (natural alignment).
4. The memory at `slot` contains either:
   * a valid GC pointer, or
   * `NULL` (0).

Violating these invariants is memory-unsafe.

---

## Runtime fast-path conditions

The barrier is expected to be cheap. It may return immediately if any of the following hold:

1. **Stored value is null** (no edge).
2. **Stored value is not young** (does not point into the nursery).
3. **Object is not old** (the base object is young, so the edge will be found by nursery tracing).

Only when `obj` is old *and* the stored value is young does the barrier perform the slow-path bookkeeping.

---

## Remembered-set semantics (non-array objects)

For ordinary objects (a fixed set of pointer fields), the runtime maintains a remembered set of **old objects that may contain young pointers**.

* Each object has a header bit `REMEMBERED`.
* On an old→young store, the barrier sets `REMEMBERED`.
  * If the bit was previously clear, the object is appended to the remembered set.
  * This ensures each object is added **at most once** per remembered-set rebuild cycle.

During minor GC:

* The collector scans the remembered set and traces any young pointers from each remembered object.
* Scanning **rebuilds** the remembered set for the next minor GC:
  * objects that no longer contain young pointers have their `REMEMBERED` bit cleared and are omitted
  * objects that still contain young pointers remain remembered

The remembered set is therefore a property of the heap (“contains a young pointer”), **not** merely a log of writes.

---

## Per-object card table semantics (pointer arrays)

Large arrays whose elements are GC pointers use per-object **card marking** to avoid rescanning the entire array on every minor GC.

* The array’s element storage is subdivided into fixed-size **cards** (implementation-defined).
* A marked card means: **this card may currently contain one or more young pointers**.
  * It does **not** mean “this card has been written since the last GC”.

Barrier behavior:

* On an old→young store into a pointer array, the barrier marks the corresponding card.

Minor GC behavior:

* Card marks are **rebuilt at each minor GC**:
  * marked cards are scanned
  * the runtime recomputes which cards still contain young pointers
  * cards with no remaining young pointers are cleared

This keeps scanning proportional to the number of old-array regions that actually reference the nursery.

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
