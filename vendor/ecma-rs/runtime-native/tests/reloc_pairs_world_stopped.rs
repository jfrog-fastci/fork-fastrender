use runtime_native::gc_roots::relocate_reloc_pairs_in_place;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use stackmap_context::ThreadContext;

struct UnregisterOnDrop;
impl Drop for UnregisterOnDrop {
  fn drop(&mut self) {
    threading::unregister_current_thread();
  }
}

#[test]
fn reloc_pairs_world_stopped_enumerates_and_updates_non_stackmap_roots() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);
  let _unreg = UnregisterOnDrop;

  // Root sources that are not described by stackmaps:
  // - per-thread handle stack (`roots::Root<T>`)
  // - global root registry (`rt_gc_register_root_slot`)
  // - persistent handle table (`roots::PersistentHandleTable`)
  let before_a = 0x1111usize as *mut u8;
  let before_b = 0x2222usize as *mut u8;
  let before_c = 0x3333usize as *mut u8;

  let root_a = runtime_native::roots::Root::<u8>::new(before_a);

  let mut slot_b = before_b;
  let handle_b = runtime_native::rt_gc_register_root_slot(&mut slot_b as *mut *mut u8);
  assert_ne!(handle_b, 0);

  let id_c = runtime_native::roots::global_persistent_handle_table().alloc(before_c);
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().get(id_c),
    Some(before_c)
  );

  threading::safepoint::with_world_stopped(|epoch| {
    let mut pairs: Vec<runtime_native::gc_roots::RelocPair> = Vec::new();
    threading::safepoint::for_each_reloc_pair_world_stopped(epoch, |pair| {
      pairs.push(pair);
    })
    .expect("reloc-pair enumeration should succeed");

    // Ensure our roots are present and appear as base roots (base_slot == derived_slot).
    let mut ctx = ThreadContext::default();
    let mut values: Vec<usize> = Vec::new();
    for pair in &pairs {
      assert_eq!(
        pair.base_slot, pair.derived_slot,
        "non-stackmap roots must be reported as (slot, slot) pairs"
      );
      values.push(pair.base_slot.read_u64(&ctx) as usize);
    }
    assert!(values.contains(&(before_a as usize)), "missing handle-stack root");
    assert!(values.contains(&(before_b as usize)), "missing global root-registry root");
    assert!(
      values.contains(&(before_c as usize)),
      "missing persistent-handle-table root"
    );

    // Simulate relocation: add a constant offset to all non-null pointers.
    relocate_reloc_pairs_in_place(&mut ctx, pairs, |old| old + 0x1000);
  });

  assert_eq!(root_a.get(), (before_a as usize + 0x1000) as *mut u8);
  assert_eq!(slot_b, (before_b as usize + 0x1000) as *mut u8);
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().get(id_c),
    Some((before_c as usize + 0x1000) as *mut u8)
  );

  // Cleanup.
  drop(root_a);
  runtime_native::rt_gc_unregister_root_slot(handle_b);
  assert!(runtime_native::roots::global_persistent_handle_table().free(id_c));

  // No need to drop the dummy pointers: they are opaque addresses.
}
