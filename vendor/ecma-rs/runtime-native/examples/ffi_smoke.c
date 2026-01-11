#include "runtime_native.h"

#include <stdint.h>
#include <string.h>

#define BYTES_LIT(s) ((const uint8_t*)(s)), (sizeof(s) - 1)

static int check(int cond) {
  return cond ? 0 : 1;
}

static void par_for_body(size_t i, uint8_t* data) {
  uint32_t* out = (uint32_t*)data;
  out[i] = (uint32_t)(i * 3u + 1u);
}

int main(void) {
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
  uint8_t* pinned = rt_alloc_pinned(16, shape);
  (void)pinned;
  rt_gc_safepoint();

  InternedId id1 = rt_string_intern(BYTES_LIT("hello"));
  InternedId id2 = rt_string_intern(BYTES_LIT("hello"));
  if (check(id1 == id2)) return 1;

  StringRef ab = rt_string_concat(BYTES_LIT("a"), BYTES_LIT("b"));
  if (check(ab.len == 2)) return 2;
  if (check(ab.ptr != NULL)) return 3;
  if (check(memcmp(ab.ptr, "ab", 2) == 0)) return 4;

  enum { N = 4096 };
  uint32_t out[N];
  memset(out, 0, sizeof(out));
  rt_parallel_for(0, N, par_for_body, (uint8_t*)out);
  for (size_t i = 0; i < N; i++) {
    if (check(out[i] == (uint32_t)(i * 3u + 1u))) return 5;
  }

  return 0;
}
