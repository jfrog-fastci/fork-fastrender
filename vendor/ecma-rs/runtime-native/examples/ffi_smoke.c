#include "runtime_native.h"

#include <stdint.h>
#include <string.h>

#define BYTES_LIT(s) ((const uint8_t*)(s)), (sizeof(s) - 1)

static int check(int cond) {
  return cond ? 0 : 1;
}

int main(void) {
  ShapeId shape = (ShapeId)0;
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

  return 0;
}
