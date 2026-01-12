use core::ptr::null_mut;
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader, PromiseRef, CORO_FLAG_RUNTIME_OWNS_FRAME,
  RT_ASYNC_ABI_VERSION,
};
use runtime_native::promise_reactions::{PromiseReactionNode, PromiseReactionVTable};
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{CoroutineId, PromiseRef as AbiPromiseRef, RtShapeDescriptor, RtShapeId};

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: core::mem::size_of::<PromiseHeader>() as u32,
      align: core::mem::align_of::<PromiseHeader>() as u16,
      flags: 0,
      ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[repr(C)]
struct AwaitOnceCoro {
  header: Coroutine,
  state: u32,
  awaited: PromiseRef,
}

unsafe extern "C" fn await_once_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut AwaitOnceCoro;
  assert!(!coro.is_null());

  match (*coro).state {
    0 => {
      (*coro).state = 1;
      CoroutineStep::await_((*coro).awaited)
    }
    // We never resume in this test (we cancel before settling), but be robust.
    1 => {
      runtime_native::rt_promise_fulfill(AbiPromiseRef((*coro).header.promise.cast()));
      CoroutineStep::complete()
    }
    other => panic!("unexpected coroutine state: {other}"),
  }
}

unsafe extern "C" fn await_once_destroy(coro: CoroutineRef) {
  if coro.is_null() {
    return;
  }
  unsafe { drop(Box::from_raw(coro as *mut AwaitOnceCoro)) };
}

static AWAIT_ONCE_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: await_once_resume,
  destroy: await_once_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[repr(C)]
struct DropCounterNode {
  node: PromiseReactionNode,
  drops: *const AtomicUsize,
}

extern "C" fn drop_counter_run(_node: *mut PromiseReactionNode, _promise: *mut PromiseHeader) {
  // Cancel paths must drop pending reactions without running them.
  std::process::abort();
}

extern "C" fn drop_counter_drop(node: *mut PromiseReactionNode) {
  if node.is_null() {
    return;
  }
  unsafe {
    let node = Box::from_raw(node.cast::<DropCounterNode>());
    (&*node.drops).fetch_add(1, Ordering::SeqCst);
  }
}

static DROP_COUNTER_VTABLE: PromiseReactionVTable = PromiseReactionVTable {
  run: drop_counter_run,
  drop: drop_counter_drop,
};

fn decode_waiters_ptr(head_val: usize) -> *mut PromiseReactionNode {
  if head_val == 0 {
    return null_mut();
  }
  if head_val % core::mem::align_of::<PromiseReactionNode>() != 0 {
    std::process::abort();
  }
  head_val as *mut PromiseReactionNode
}

fn push_reaction(promise: *mut PromiseHeader, node: *mut PromiseReactionNode) {
  if promise.is_null() || node.is_null() {
    return;
  }
  let waiters = unsafe { &(*promise).waiters };
  loop {
    let head_val = waiters.load(Ordering::Acquire);
    let head = decode_waiters_ptr(head_val);
    unsafe {
      (*node).next = head;
    }
    if waiters
      .compare_exchange(head_val, node as usize, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      break;
    }
  }
}

#[test]
fn pending_reactions_tracking_is_gc_safe_for_movable_promises() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  runtime_native::rt_thread_init(0);

  let baseline_handles = runtime_native::roots::global_persistent_handle_table().live_count();

  // Allocate a GC-managed promise header (nursery object).
  let mut promise_obj = runtime_native::rt_alloc(core::mem::size_of::<PromiseHeader>(), RtShapeId(1));
  assert!(!promise_obj.is_null());

  // Keep the promise alive and observe relocation via a registered root slot.
  let root_handle = runtime_native::rt_gc_register_root_slot(&mut promise_obj as *mut *mut u8);
  let promise = promise_obj.cast::<PromiseHeader>();
  unsafe {
    runtime_native::rt_promise_init(AbiPromiseRef(promise.cast()));
  }

  // Spawn a coroutine that awaits the promise; this installs a reaction node and should call
  // `track_pending_reactions(promise)` internally.
  let mut coro = Box::new(AwaitOnceCoro {
    // `Coroutine` embeds a private `ObjHeader` prefix; zero it so the GC header stays inert for this
    // Rust-allocated (non-GC) test frame.
    header: unsafe { core::mem::zeroed() },
    state: 0,
    awaited: promise,
  });
  coro.header.vtable = &AWAIT_ONCE_VTABLE;
  coro.header.promise = null_mut();
  coro.header.next_waiter = null_mut();
  coro.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;
  let coro_ref = Box::into_raw(coro) as CoroutineRef;
  let coro_handle = runtime_native::rt_handle_alloc(coro_ref.cast());
  let _result_promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(coro_handle)) };

  // Attach an additional reaction node with an observable drop hook.
  let drops = AtomicUsize::new(0);
  let node = Box::new(DropCounterNode {
    node: PromiseReactionNode {
      next: null_mut(),
      vtable: &DROP_COUNTER_VTABLE,
    },
    drops: &drops,
  });
  push_reaction(promise, Box::into_raw(node).cast::<PromiseReactionNode>());

  // At this point we should have at least:
  // - the coroutine handle, and
  // - the promise pending-reactions tracking handle.
  assert!(
    runtime_native::roots::global_persistent_handle_table().live_count() >= baseline_handles + 2,
    "expected at least two persistent handles (coroutine + tracked promise)"
  );

  // Trigger a GC collection that relocates nursery objects.
  //
  // Conservative scanning may rewrite any stack word that looks like an object pointer. Tag the
  // "before" value so it is not a plausible object start address.
  let before_tagged = (runtime_native::rt_gc_root_get(root_handle) as usize) | 1;
  runtime_native::rt_gc_collect();
  let after = runtime_native::rt_gc_root_get(root_handle);
  let before = (before_tagged & !1) as *mut u8;
  assert_ne!(after, before, "expected GC to relocate the nursery promise object");

  // Cancellation must still find the tracked promise (via HandleId) and drop its pending reactions
  // (including our custom drop-counter node).
  runtime_native::rt_async_cancel_all();
  assert_eq!(drops.load(Ordering::SeqCst), 1, "expected pending reaction node drop hook to run");

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    baseline_handles,
    "expected cancel_all to free all persistent handles it allocated"
  );

  runtime_native::rt_gc_unregister_root_slot(root_handle);
  runtime_native::rt_thread_deinit();
}
