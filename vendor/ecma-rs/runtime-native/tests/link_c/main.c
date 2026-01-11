#include <stdint.h>

extern void rt_thread_init(uint32_t kind);
extern void rt_thread_deinit(void);
extern void rt_gc_collect(void);
extern void rt_gc_safepoint(void);

int main(void) {
  rt_thread_init(3);
  rt_gc_safepoint();
  rt_gc_collect();
  rt_thread_deinit();
  return 0;
}
