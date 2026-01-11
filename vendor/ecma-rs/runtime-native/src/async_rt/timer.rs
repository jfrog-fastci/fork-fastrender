use crate::async_rt::Task;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

pub struct Timers {
  next_id: AtomicU64,
  inner: Mutex<TimersInner>,
}

struct TimersInner {
  heap: BinaryHeap<HeapEntry>,
  states: HashMap<TimerId, TimerState>,
}

struct TimerState {
  deadline: Instant,
  seq: u64,
  task: Task,
}

#[derive(Clone, Copy)]
struct HeapEntry {
  deadline: Instant,
  id: TimerId,
  seq: u64,
}

impl PartialEq for HeapEntry {
  fn eq(&self, other: &Self) -> bool {
    self.deadline == other.deadline && self.id == other.id && self.seq == other.seq
  }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for HeapEntry {
  fn cmp(&self, other: &Self) -> Ordering {
    // Reverse ordering so the earliest deadline is popped first (min-heap).
    other
      .deadline
      .cmp(&self.deadline)
      .then_with(|| other.id.0.cmp(&self.id.0))
      .then_with(|| other.seq.cmp(&self.seq))
  }
}

impl Timers {
  pub fn new() -> Self {
    Self {
      next_id: AtomicU64::new(1),
      inner: Mutex::new(TimersInner {
        heap: BinaryHeap::new(),
        states: HashMap::new(),
      }),
    }
  }

  pub fn has_timers(&self) -> bool {
    !self.inner.lock().unwrap().states.is_empty()
  }

  pub fn schedule(&self, deadline: Instant, task: Task) -> TimerId {
    let id = TimerId(self.next_id.fetch_add(1, AtomicOrdering::Relaxed));
    let mut inner = self.inner.lock().unwrap();
    let seq = 0;
    inner.states.insert(
      id,
      TimerState {
        deadline,
        seq,
        task,
      },
    );
    inner.heap.push(HeapEntry { deadline, id, seq });
    inner.maybe_rebuild_heap();
    crate::rt_trace::timer_heap_inc_by(1);
    id
  }

  pub fn cancel(&self, id: TimerId) -> bool {
    let mut inner = self.inner.lock().unwrap();
    let existed = inner.states.remove(&id).is_some();
    if existed {
      crate::rt_trace::timer_heap_dec_by(1);
    }
    inner.maybe_rebuild_heap();
    existed
  }

  pub fn drain_due(&self, now: Instant) -> Vec<Task> {
    let mut ready = Vec::new();
    let mut inner = self.inner.lock().unwrap();
    inner.purge_stale_top();
    while let Some(top) = inner.heap.peek().copied() {
      if top.deadline > now {
        break;
      }
      inner.heap.pop();
      if !inner.is_entry_current(top) {
        inner.purge_stale_top();
        continue;
      }
      // Entry is current; remove state and enqueue the task.
      let state = inner.states.remove(&top.id).expect("entry must be current");
      crate::rt_trace::timer_heap_dec_by(1);
      ready.push(state.task);
      inner.purge_stale_top();
    }
    inner.maybe_rebuild_heap();
    ready
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    let mut inner = self.inner.lock().unwrap();
    inner.purge_stale_top();
    inner.heap.peek().map(|e| e.deadline)
  }

  pub fn clear(&self) {
    let mut inner = self.inner.lock().unwrap();
    let active = inner.states.len() as u64;
    inner.heap.clear();
    inner.states.clear();
    if active > 0 {
      crate::rt_trace::timer_heap_dec_by(active);
    }
  }
}

impl Drop for Timers {
  fn drop(&mut self) {
    let active = self.inner.lock().unwrap().states.len() as u64;
    if active > 0 {
      crate::rt_trace::timer_heap_dec_by(active);
    }
  }
}

impl TimersInner {
  fn is_entry_current(&self, entry: HeapEntry) -> bool {
    self
      .states
      .get(&entry.id)
      .is_some_and(|st| st.seq == entry.seq && st.deadline == entry.deadline)
  }

  fn purge_stale_top(&mut self) {
    while let Some(top) = self.heap.peek().copied() {
      if self.is_entry_current(top) {
        break;
      }
      self.heap.pop();
    }
  }

  fn maybe_rebuild_heap(&mut self) {
    // When timers are cancelled/rescheduled, old heap entries become stale. We
    // opportunistically drop them when they reach the top, but in workloads
    // that create many cancelled timers far in the future, stale entries may
    // otherwise accumulate. Rebuild the heap when it gets significantly larger
    // than the set of active timers.
    let active = self.states.len();
    if self.heap.len() <= active.saturating_mul(4).saturating_add(64) {
      return;
    }

    let mut heap = BinaryHeap::with_capacity(active);
    for (id, st) in self.states.iter() {
      heap.push(HeapEntry {
        deadline: st.deadline,
        id: *id,
        seq: st.seq,
      });
    }
    self.heap = heap;
  }
}
