extern void rt_gc_collect(void);
extern void rt_gc_safepoint(void);

int main(void) {
  rt_gc_safepoint();
  rt_gc_collect();
  return 0;
}
