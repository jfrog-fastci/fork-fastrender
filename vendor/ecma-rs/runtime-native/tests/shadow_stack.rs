use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use runtime_native::gc::{ObjHeader, RememberedSet, RootScope, TypeDescriptor};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading::{registry, safepoint};
use runtime_native::GcHeap;

#[repr(C)]
struct Blob {
  header: ObjHeader,
  value: usize,
}

static BLOB_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Blob>(), &[]);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn root_handle_updates_after_minor_gc() {
  let _rt = TestRuntimeGuard::new();
  registry::register_current_thread(registry::ThreadKind::Main);

  let mut heap = GcHeap::new();
  let mut remembered = NullRememberedSet::default();

  runtime_native::gc::with_thread_state(|ts| {
    let scope = RootScope::new(ts);
    let obj = heap.alloc_young(&BLOB_DESC);
    assert!(heap.is_in_nursery(obj));

    let h = scope.root(obj);
    heap
      .collect_minor_with_shadow_stacks(&mut remembered)
      .expect("minor GC");

    let new_obj = h.get();
    assert_ne!(new_obj, obj);
    assert!(!heap.is_in_nursery(new_obj));
  });
}

#[test]
fn scope_drop_truncates_shadow_stack() {
  let _rt = TestRuntimeGuard::new();
  registry::register_current_thread(registry::ThreadKind::Main);

  let mut heap = GcHeap::new();

  runtime_native::gc::with_thread_state(|ts| {
    let base_len = ts.shadow_stack().len();

    {
      let outer = RootScope::new(ts);
      let _a = outer.root(heap.alloc_young(&BLOB_DESC));
      assert_eq!(ts.shadow_stack().len(), base_len + 1);

      {
        let inner = RootScope::new(ts);
        let _b = inner.root(heap.alloc_young(&BLOB_DESC));
        let _c = inner.root(heap.alloc_young(&BLOB_DESC));
        assert_eq!(ts.shadow_stack().len(), base_len + 3);
      }

      assert_eq!(ts.shadow_stack().len(), base_len + 1);
    }

    assert_eq!(ts.shadow_stack().len(), base_len);
  });
}

#[test]
fn stw_root_enumerator_updates_shadow_stack_slots() {
  let _rt = TestRuntimeGuard::new();
  registry::register_current_thread(registry::ThreadKind::Main);

  let before = 0xdead_beef_dead_beefu64 as usize as *mut u8;
  let after = 0xcafe_babe_cafe_babeu64 as usize as *mut u8;

  runtime_native::gc::with_thread_state(|ts| {
    let scope = RootScope::new(ts);
    let h = scope.root(before);
    assert_eq!(h.get(), before);

    safepoint::with_world_stopped(|stop_epoch| {
      let mut updated = 0usize;
      safepoint::for_each_root_slot_world_stopped(stop_epoch, |slot| unsafe {
        if slot.read() == before {
          slot.write(after);
          updated += 1;
        }
      })
      .expect("root enumeration should succeed");
      assert_eq!(updated, 1);
    });

    assert_eq!(h.get(), after);
  });
}

#[test]
fn gc_traces_shadow_stacks_of_all_threads() {
  let _rt = TestRuntimeGuard::new();
  registry::register_current_thread(registry::ThreadKind::Main);

  let mut heap = GcHeap::new();
  let mut remembered = NullRememberedSet::default();

  let threads = 4usize;
  let mut objs = Vec::with_capacity(threads);
  for _ in 0..threads {
    let obj = heap.alloc_young(&BLOB_DESC);
    assert!(heap.is_in_nursery(obj));
    objs.push(obj as usize);
  }

  let gc_done = Arc::new(AtomicBool::new(false));
  let (ready_tx, ready_rx) = mpsc::channel::<()>();
  let (done_tx, done_rx) = mpsc::channel::<(usize, usize)>();

  let mut joins = Vec::new();
  for obj in &objs {
    let obj = *obj;
    let gc_done = gc_done.clone();
    let ready_tx = ready_tx.clone();
    let done_tx = done_tx.clone();

    joins.push(std::thread::spawn(move || {
      registry::register_current_thread(registry::ThreadKind::Worker);

      runtime_native::gc::with_thread_state(|ts| {
        let scope = RootScope::new(ts);
        let h = scope.root(obj as *mut u8);

        ready_tx.send(()).unwrap();

        while !gc_done.load(Ordering::Acquire) {
          safepoint::rt_gc_safepoint();
          std::thread::yield_now();
        }

        done_tx.send((obj, h.get() as usize)).unwrap();
      });

      // Ensure the thread is unregistered before exiting so subsequent tests don't wait for it.
      registry::unregister_current_thread();
    }));
  }

  for _ in 0..threads {
    ready_rx.recv().unwrap();
  }

  // Stop the world so we can safely evacuate and update shadow stack slots.
  runtime_native::rt_gc_request_stop_the_world();
  runtime_native::rt_gc_wait_for_world_stopped();

  heap
    .collect_minor_with_shadow_stacks(&mut remembered)
    .expect("minor GC");

  runtime_native::rt_gc_resume_world();
  gc_done.store(true, Ordering::Release);

  for _ in 0..threads {
    let (old, new) = done_rx.recv().unwrap();
    assert!(objs.contains(&old));
    assert_ne!(old, new);
    assert!(!heap.is_in_nursery(new as *mut u8));
  }

  for join in joins {
    join.join().unwrap();
  }
}
