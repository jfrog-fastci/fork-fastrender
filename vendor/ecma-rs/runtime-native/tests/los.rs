use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::GcHeap;
use runtime_native::RootStack;
use runtime_native::TypeDescriptor;

#[test]
fn los_mark_sweep_frees_unreachable_objects() {
  let mut heap = GcHeap::new();

  const OBJ_SIZE: usize = IMMIX_MAX_OBJECT_SIZE + 1024;
  static DESC: TypeDescriptor = TypeDescriptor::new(OBJ_SIZE, &[]);

  // Allocate three large objects; keep only `a` and `c` as roots.
  let mut a = heap.alloc_old(&DESC);
  let b = heap.alloc_old(&DESC);
  let mut c = heap.alloc_pinned(&DESC);
  let c_addr_before = c as usize;
  unsafe {
    let hdr = &*(c as *const ObjHeader);
    assert!(hdr.is_pinned());
  }

  // Roots are enumerated via explicit slot registration.
  let mut roots = RootStack::new();
  roots.push(&mut a);
  roots.push(&mut c);

  let mut remembered = SimpleRememberedSet::new();

  assert_eq!(heap.los_object_count(), 3);
  heap.collect_major(&mut roots, &mut remembered).unwrap();

  // b should be swept.
  assert_eq!(heap.los_object_count(), 2);
  assert!(heap.is_in_los(a));
  assert!(heap.is_in_los(c));
  assert!(!heap.is_in_los(b));

  // Pinned objects are never moved (LOS is non-moving) and retain their pinned bit.
  assert_eq!(c as usize, c_addr_before);
  unsafe {
    let hdr = &*(c as *const ObjHeader);
    assert!(hdr.is_pinned());
  }
}
