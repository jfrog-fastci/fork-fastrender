use core::ptr::null_mut;
use runtime_native::async_abi::{PromiseHeader, PromiseRef as PromiseHeaderRef};
use runtime_native::promise_reactions::{PromiseReactionNode, PromiseReactionVTable};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseRef as AbiPromiseRef;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[repr(C)]
struct IndexedReaction {
  node: PromiseReactionNode,
  counters: *const AtomicUsize,
  idx: usize,
}

extern "C" fn indexed_reaction_run(node: *mut PromiseReactionNode, _promise: PromiseHeaderRef) {
  // Safety: allocated by `alloc_indexed_reaction`.
  let node = unsafe { &*(node as *const IndexedReaction) };
  let counter = unsafe { &*node.counters.add(node.idx) };
  counter.fetch_add(1, Ordering::Relaxed);
}

extern "C" fn indexed_reaction_drop(node: *mut PromiseReactionNode) {
  // Safety: allocated by `alloc_indexed_reaction`.
  unsafe {
    drop(Box::from_raw(node as *mut IndexedReaction));
  }
}

static INDEXED_REACTION_VTABLE: PromiseReactionVTable = PromiseReactionVTable {
  run: indexed_reaction_run,
  drop: indexed_reaction_drop,
};

fn alloc_indexed_reaction(counters: *const AtomicUsize, idx: usize) -> *mut PromiseReactionNode {
  let node = Box::new(IndexedReaction {
    node: PromiseReactionNode {
      next: null_mut(),
      vtable: &INDEXED_REACTION_VTABLE,
    },
    counters,
    idx,
  });
  Box::into_raw(node) as *mut PromiseReactionNode
}

unsafe fn push_reaction(p: PromiseHeaderRef, node: *mut PromiseReactionNode) {
  let hdr = unsafe { &*p };
  let reactions = &hdr.reactions;
  loop {
    let head = reactions.load(Ordering::Acquire) as *mut PromiseReactionNode;
    unsafe {
      (*node).next = head;
    }
    if reactions
      .compare_exchange(head as usize, node as usize, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      break;
    }
  }
}

fn drain_microtasks_until(timeout: Duration, done: impl Fn() -> bool) {
  let start = Instant::now();
  while !done() {
    while runtime_native::rt_drain_microtasks() {}
    if done() {
      return;
    }
    if start.elapsed() > timeout {
      panic!("timeout waiting for microtasks to complete");
    }
    std::thread::yield_now();
  }
}

#[test]
fn promise_settle_is_first_wins_and_wakes_reactions_once() {
  let _rt = TestRuntimeGuard::new();

  const ITERS: usize = 50;
  const WAITERS: usize = 128;
  const SETTLERS: usize = 16;

  for _ in 0..ITERS {
    let mut promise = Box::new(PromiseHeader {
      state: AtomicU8::new(0),
      reactions: AtomicUsize::new(0),
      flags: AtomicU8::new(0),
    });
    let p_hdr: PromiseHeaderRef = &mut *promise;
    let p = AbiPromiseRef(p_hdr.cast());
    unsafe { runtime_native::rt_promise_init(p) };

    let counters: Vec<AtomicUsize> = (0..WAITERS).map(|_| AtomicUsize::new(0)).collect();

    for idx in 0..WAITERS {
      let node = alloc_indexed_reaction(counters.as_ptr(), idx);
      unsafe { push_reaction(p_hdr, node) };
    }

    let settle_winners = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(SETTLERS));
    std::thread::scope(|s| {
      for i in 0..SETTLERS {
        let barrier = Arc::clone(&barrier);
        let winners = Arc::clone(&settle_winners);
        s.spawn(move || {
          barrier.wait();
          let won = unsafe {
            if i % 2 == 0 {
              runtime_native::rt_promise_try_fulfill(p)
            } else {
              runtime_native::rt_promise_try_reject(p)
            }
          };
          if won {
            winners.fetch_add(1, Ordering::Relaxed);
          }
        });
      }
    });

    drain_microtasks_until(Duration::from_secs(5), || {
      counters.iter().all(|c| c.load(Ordering::Relaxed) == 1)
    });

    assert_eq!(
      settle_winners.load(Ordering::Relaxed),
      1,
      "expected exactly one settle winner"
    );

    // Promise must be settled and reactions drained.
    let st = promise.state.load(Ordering::Acquire);
    assert!(st == PromiseHeader::FULFILLED || st == PromiseHeader::REJECTED);
    assert_eq!(promise.reactions.load(Ordering::Acquire), 0);

    // Losing/duplicate settle calls must be no-ops.
    assert!(!unsafe { runtime_native::rt_promise_try_fulfill(p) });
    assert!(!unsafe { runtime_native::rt_promise_try_reject(p) });
  }
}
