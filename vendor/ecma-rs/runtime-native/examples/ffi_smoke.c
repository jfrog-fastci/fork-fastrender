#include "runtime_native.h"

#include <stdint.h>
#include <string.h>

#define BYTES_LIT(s) ((const uint8_t*)(s)), (sizeof(s) - 1)

static int check(int cond) {
  return cond ? 0 : 1;
}

static void set_int(uint8_t* data) {
  int* flag = (int*)data;
  *flag = 1;
}

static void par_for_body(size_t i, uint8_t* data) {
  uint32_t* out = (uint32_t*)data;
  out[i] = (uint32_t)(i * 3u + 1u);
}

int main(void) {
  // The runtime expects mutator threads to register before executing compiled
  // code or participating in GC safepoints.
  rt_thread_init(0);
  int rc = 0;
  static const RtShapeDescriptor kShapes[1] = {
    {
      .size = 16,
      .align = 16,
      .flags = 0,
      .ptr_offsets = (const uint32_t*)0,
      .ptr_offsets_len = 0,
      .reserved = 0,
    },
  };
  rt_register_shape_table(kShapes, 1);

  RtShapeId shape = (RtShapeId)1;
  GcPtr obj1 = rt_alloc_pinned(16, shape);
  GcPtr obj2 = rt_alloc_pinned(16, shape);
  if (check(obj1 != NULL)) { rc = 6; goto done; }
  if (check(obj2 != NULL)) { rc = 7; goto done; }

  // Temporary shadow-stack roots: embedders/native glue can register addressable
  // GC pointer slots so a moving GC can update them in-place.
  //
  // This smoke test doesn't exercise relocation (pinned objects never move),
  // but it ensures the symbol is present and callable from C.
  GcPtr tmp = obj1;
  rt_root_push(&tmp);
  rt_gc_safepoint();
  rt_root_pop(&tmp);
  // `rt_keep_alive_gc_ref` is the native equivalent of Go's `runtime.KeepAlive`: it exists to
  // extend the liveness of GC objects when native/compiled code uses derived raw pointers.
  // This smoke test doesn't model those derived pointers, but it does ensure the symbol is present
  // and callable from C.
  rt_keep_alive_gc_ref(obj1);

  // Persistent handle API: stable u64 ids for keeping GC objects alive across
  // async / OS boundaries.
  HandleId h = rt_handle_alloc(obj1);
  if (check(rt_handle_load(h) == obj1)) { rc = 8; goto done; }
  rt_handle_store(h, obj2);
  if (check(rt_handle_load(h) == obj2)) { rc = 9; goto done; }
  rt_handle_free(h);
  if (check(rt_handle_load(h) == NULL)) { rc = 10; goto done; }
  rt_handle_store(h, obj1);
  if (check(rt_handle_load(h) == NULL)) { rc = 11; goto done; }

  InternedId id1 = rt_string_intern(BYTES_LIT("hello"));
  InternedId id2 = rt_string_intern(BYTES_LIT("hello"));
  if (check(id1 == id2)) { rc = 1; goto done; }
  rt_string_pin_interned(id1);

  StringRef ab = rt_string_concat(BYTES_LIT("a"), BYTES_LIT("b"));
  if (check(ab.len == 2)) { rc = 2; goto done; }
  if (check(ab.ptr != NULL)) { rc = 3; goto done; }
  if (check(memcmp(ab.ptr, "ab", 2) == 0)) { rc = 4; goto done; }

  // Microtask API: microtasks should not run synchronously when queued, and should run once drained.
  int microtask_ran = 0;
  Microtask mt = {
    .func = set_int,
    .data = (uint8_t*)&microtask_ran,
  };
  rt_queue_microtask(mt);
  if (check(microtask_ran == 0)) { rc = 12; goto done; }
  rt_drain_microtasks();
  if (check(microtask_ran == 1)) { rc = 13; goto done; }

  enum { N = 4096 };
  uint32_t out[N];
  memset(out, 0, sizeof(out));
  rt_parallel_for(0, N, par_for_body, (uint8_t*)out);
  for (size_t i = 0; i < N; i++) {
    if (check(out[i] == (uint32_t)(i * 3u + 1u))) { rc = 5; goto done; }
  }

done:
  rt_thread_deinit();
  return rc;
}
