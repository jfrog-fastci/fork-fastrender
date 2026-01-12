# String interner (`InternedId`)

`runtime-native` provides a weak string interner intended for common UTF-8 strings like property
names, keywords, and other identifier-like data that is frequently compared.

The stable C ABI surface is:

```c
InternedId rt_string_intern(const uint8_t* s, size_t len);
void rt_string_pin_interned(InternedId id);
bool rt_string_lookup(InternedId id, StringRef* out);
```

## ID lifetime

`InternedId` values are **monotonically allocated and never reused**.

Unpinned interned strings are held weakly and may be reclaimed after a GC/prune cycle. When a string
is reclaimed, its `InternedId` becomes permanently invalid and will never be reassigned to a
different string.

Re-interning the same bytes after reclamation yields a **new** `InternedId`.

## GC-safety and `rt_string_lookup`

The runtime uses a moving GC for GC-managed allocations. Returning raw pointers into movable GC
objects is unsafe unless the object is pinned or the bytes are copied out.

To keep the C ABI GC-safe, `rt_string_lookup` uses a **pinned-only** contract:

- `rt_string_lookup(id, out)` only succeeds for **pinned** interned strings.
- Call `rt_string_pin_interned(id)` to pin an ID before looking it up.
- On success, `out->ptr..out->ptr+out->len` points to **non-GC memory owned by the interner** and is
  stable for the lifetime of the process.
- If the ID is invalid, reclaimed, or not pinned, `rt_string_lookup` returns `false`.

### Example (C)

```c
InternedId id = rt_string_intern((const uint8_t*)"perm", 4);
rt_string_pin_interned(id);

StringRef s;
if (rt_string_lookup(id, &s)) {
  // `s.ptr` is stable and can be used across GC/safepoints.
  fwrite(s.ptr, 1, s.len, stdout);
}
```

