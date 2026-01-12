# String interner (`InternedId`)

`runtime-native` provides a weak string interner intended for common UTF-8 strings like property
names, keywords, and other identifier-like data that is frequently compared.

The stable C ABI surface is:

```c
InternedId rt_string_intern(const uint8_t* s, size_t len);
void rt_string_pin_interned(InternedId id);
StringRef rt_string_lookup(InternedId id);
bool rt_string_lookup_pinned(InternedId id, StringRef* out);
```

## ID lifetime

`InternedId` values are **monotonically allocated and never reused**.

Unpinned interned strings are held weakly and may be reclaimed after a GC/prune cycle. When a string
is reclaimed, its `InternedId` becomes permanently invalid and will never be reassigned to a
different string.

Re-interning the same bytes after reclamation yields a **new** `InternedId`.

## Ownership + GC-safety (`rt_string_lookup` vs `rt_string_lookup_pinned`)

### `rt_string_lookup`

`rt_string_lookup(id)` returns a **borrowed** `StringRef` view of the interned UTF-8 bytes.

- The returned `StringRef` must NOT be freed (do not call `rt_string_free` / `rt_stringref_free`).
- Invalid/reclaimed IDs return `{ptr = NULL, len = 0}` (distinct from a valid empty string, which
  returns `{ptr != NULL, len = 0}`).
- For **unpinned** entries, `ptr..ptr+len` may point into a GC-managed allocation (via a weak handle)
  and is therefore only valid until the next GC safepoint/collection.
- For **pinned** entries, the bytes are stored outside the GC heap and may be stable, but callers
  that require a GC-stable byte pointer should still prefer `rt_string_lookup_pinned` (which has an
  explicit pinned-only contract).

### `rt_string_lookup_pinned`

The runtime uses a moving GC for GC-managed allocations. Returning raw pointers into movable GC
objects is unsafe unless the object is pinned or the bytes are copied out.

To keep a GC-safe lookup path for callers that need a stable byte pointer, `rt_string_lookup_pinned`
uses a **pinned-only** contract:

- `rt_string_lookup_pinned(id, out)` only succeeds for **pinned** interned strings.
- Call `rt_string_pin_interned(id)` to pin an ID before looking it up.
- On success, `out->ptr..out->ptr+out->len` points to **non-GC memory owned by the interner** and is
  stable for the lifetime of the process.
- The returned `StringRef` is borrowed and must NOT be freed (do not call `rt_string_free` /
  `rt_stringref_free` on it).
- If the ID is invalid, reclaimed, or not pinned, `rt_string_lookup_pinned` returns `false`.

### Example (C)

```c
InternedId id = rt_string_intern((const uint8_t*)"perm", 4);
rt_string_pin_interned(id);

StringRef s;
if (rt_string_lookup_pinned(id, &s)) {
  // `s.ptr` is stable and can be used across GC/safepoints.
  fwrite(s.ptr, 1, s.len, stdout);
}
```
