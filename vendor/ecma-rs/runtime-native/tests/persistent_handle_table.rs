use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

#[test]
fn persistent_handle_table_is_enumerated_as_root() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let obj = Box::into_raw(Box::new(0u8)) as *mut u8;
  let id = runtime_native::roots::global_persistent_handle_table().alloc(obj);

  threading::safepoint::with_world_stopped(|epoch| {
    let mut values: Vec<usize> = Vec::new();
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      values.push(*slot as usize);
    })
    .expect("root enumeration should succeed");

    assert!(values.contains(&(obj as usize)), "missing persistent-handle root");
  });

  assert!(runtime_native::roots::global_persistent_handle_table().free(id));

  threading::safepoint::with_world_stopped(|epoch| {
    let mut values: Vec<usize> = Vec::new();
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      values.push(*slot as usize);
    })
    .expect("root enumeration should succeed");

    assert!(
      !values.contains(&(obj as usize)),
      "persistent-handle root should have been removed"
    );
  });

  unsafe {
    drop(Box::from_raw(obj));
  }
  threading::unregister_current_thread();
}

