use runtime_native::roots::RootScope;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[test]
fn root_registry_register_unregister_and_enumeration() {
  let _rt = TestRuntimeGuard::new();

  // Root scopes are backed by the thread registry.
  threading::register_current_thread(ThreadKind::Main);

  let obj_a = Box::into_raw(Box::new(0u8)) as *mut u8;
  let obj_b = Box::into_raw(Box::new(0u8)) as *mut u8;
  let obj_c = Box::into_raw(Box::new(0u8)) as *mut u8;
  let obj_d = Box::into_raw(Box::new(0u8)) as *mut u8;

  let mut root_a = obj_a;
  let mut root_b = obj_b;

  let handle_a = runtime_native::rt_gc_register_root_slot(&mut root_a as *mut *mut u8);
  let handle_b = runtime_native::rt_gc_register_root_slot(&mut root_b as *mut *mut u8);
  let handle_c = runtime_native::rt_gc_pin(obj_c);

  let mut scope_root = obj_d;
  let mut scope = RootScope::new();
  scope.push(&mut scope_root as *mut *mut u8);

  threading::safepoint::with_world_stopped(|epoch| {
    let mut values: Vec<usize> = Vec::new();
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      values.push(*slot as usize);
    })
    .expect("root enumeration should succeed");

    assert!(values.contains(&(obj_a as usize)), "missing obj_a root");
    assert!(values.contains(&(obj_b as usize)), "missing obj_b root");
    assert!(values.contains(&(obj_c as usize)), "missing obj_c pinned root");
    assert!(values.contains(&(obj_d as usize)), "missing obj_d scope root");
  });

  // Dropping the scope should remove its roots.
  drop(scope);
  threading::safepoint::with_world_stopped(|epoch| {
    let mut values: Vec<usize> = Vec::new();
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      values.push(*slot as usize);
    })
    .unwrap();
    // Still contains global roots.
    assert!(values.contains(&(obj_a as usize)));
    assert!(values.contains(&(obj_b as usize)));
    assert!(values.contains(&(obj_c as usize)));
    assert!(!values.contains(&(obj_d as usize)), "scope root should have been removed");
  });

  runtime_native::rt_gc_unregister_root_slot(handle_a);
  runtime_native::rt_gc_unregister_root_slot(handle_b);
  runtime_native::rt_gc_unpin(handle_c);

  // After unregistering everything, there should be no roots.
  threading::safepoint::with_world_stopped(|epoch| {
    let mut count = 0usize;
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |_slot| {
      count += 1;
    })
    .unwrap();
    assert_eq!(count, 0);
  });

  // Cleanup dummy objects.
  unsafe {
    drop(Box::from_raw(obj_a));
    drop(Box::from_raw(obj_b));
    drop(Box::from_raw(obj_c));
    drop(Box::from_raw(obj_d));
  }

  threading::unregister_current_thread();
}

#[test]
fn root_registry_is_thread_safe_under_concurrent_mutation_and_enumeration() {
  let _rt = TestRuntimeGuard::new();

  const WORKERS: usize = 4;
  let start = Arc::new(Barrier::new(WORKERS + 1));
  let stop = Arc::new(AtomicBool::new(false));

  let mut threads = Vec::new();
  for i in 0..WORKERS {
    let start = start.clone();
    let stop = stop.clone();
    threads.push(std::thread::spawn(move || {
      start.wait();
      let mut iter = 0u64;
      while !stop.load(Ordering::Acquire) {
        // Register a stack slot that stays live until we unregister.
        let mut ptr = (0x1000usize + (i * 0x10)) as *mut u8;
        let handle = runtime_native::rt_gc_register_root_slot(&mut ptr as *mut *mut u8);

        // Encourage interleavings.
        iter += 1;
        if iter % 8 == 0 {
          std::thread::yield_now();
        } else {
          std::hint::spin_loop();
        }

        runtime_native::rt_gc_unregister_root_slot(handle);
      }
    }));
  }

  start.wait();

  let deadline = Instant::now() + Duration::from_millis(200);
  while Instant::now() < deadline {
    let mut values: Vec<usize> = Vec::new();
    runtime_native::roots::global_root_registry().for_each_root_slot(|slot| unsafe {
      values.push(*slot as usize);
    });

    assert!(
      values.len() <= WORKERS,
      "expected at most {WORKERS} concurrent roots, saw {}",
      values.len()
    );

    for v in values {
      assert!(
        (0x1000..0x1000 + (WORKERS * 0x10)).contains(&v),
        "unexpected root value {v:#x}"
      );
    }
  }

  stop.store(true, Ordering::Release);
  for t in threads {
    t.join().unwrap();
  }
}

#[test]
fn root_registry_get_and_set_observe_slot_updates() {
  let _rt = TestRuntimeGuard::new();

  let obj_a = Box::into_raw(Box::new(0u8)) as *mut u8;
  let obj_b = Box::into_raw(Box::new(1u8)) as *mut u8;

  let handle = runtime_native::rt_gc_pin(obj_a);
  assert_eq!(runtime_native::roots::global_root_registry().get(handle), Some(obj_a));

  // Simulate a moving GC updating the slot in place.
  runtime_native::roots::global_root_registry().for_each_root_slot(|slot| unsafe {
    if *slot == obj_a {
      *slot = obj_b;
    }
  });
  assert_eq!(runtime_native::roots::global_root_registry().get(handle), Some(obj_b));

  // Direct `set` should also update the stored pointer.
  assert!(runtime_native::roots::global_root_registry().set(handle, obj_a));
  assert_eq!(runtime_native::roots::global_root_registry().get(handle), Some(obj_a));

  runtime_native::rt_gc_unpin(handle);
  assert_eq!(runtime_native::roots::global_root_registry().get(handle), None);
  assert!(!runtime_native::roots::global_root_registry().set(handle, obj_a));

  unsafe {
    drop(Box::from_raw(obj_a));
    drop(Box::from_raw(obj_b));
  }
}
