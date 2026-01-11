//! A user-facing Promise API implemented on top of the low-level async ABI.
//!
//! This module intentionally mirrors the shape of JavaScript promises:
//! - `then`/`catch`/`finally`
//! - `Promise::resolve`/`reject` and combinators (`all`, `race`, `all_settled`, `any`)
//!
//! Internally, it uses:
//! - [`crate::async_abi::PromiseHeader`] as an ABI-stable prefix at offset 0, and
//! - [`crate::promise_reactions::PromiseReactionNode`] stored in
//!   [`PromiseHeader::waiters`] as an intrusive list.
//!
//! All reactions are scheduled onto the global microtask queue (never run inline).

use crate::async_abi::{PromiseHeader, PromiseRef};
use crate::async_rt::{global as async_global, Task};
use crate::promise_reactions::{reverse_list, PromiseReactionNode, PromiseReactionVTable};
use crate::sync::GcAwareMutex;
use once_cell::sync::Lazy;
use std::any::Any;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::future::Future;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ptr::null_mut;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

// Internal settling states. These are *not* part of the public ABI; only
// `PromiseHeader::{PENDING,FULFILLED,REJECTED}` are externally observable.
const STATE_FULFILLING: u8 = 3;
const STATE_REJECTING: u8 = 4;

#[derive(Clone)]
pub struct PromiseRejection {
  inner: Arc<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for PromiseRejection {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_tuple("PromiseRejection").finish()
  }
}

impl PromiseRejection {
  pub fn new<E: Any + Send + Sync>(err: E) -> Self {
    Self {
      inner: Arc::new(err),
    }
  }

  pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
    (&*self.inner).downcast_ref::<T>()
  }
}

#[derive(Clone, Debug)]
pub struct AggregateError {
  pub errors: Vec<PromiseRejection>,
}

#[derive(Clone, Debug)]
pub enum Settled<T> {
  Fulfilled(T),
  Rejected(PromiseRejection),
}

#[derive(Clone)]
pub enum PromiseReturn<T> {
  Value(T),
  Promise(Arc<Promise<T>>),
}

impl<T> From<T> for PromiseReturn<T> {
  fn from(value: T) -> Self {
    Self::Value(value)
  }
}

impl<T> From<Arc<Promise<T>>> for PromiseReturn<T> {
  fn from(promise: Arc<Promise<T>>) -> Self {
    Self::Promise(promise)
  }
}

// -----------------------------------------------------------------------------
// Unhandled rejection tracking.
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct UnhandledRejection {
  pub promise_id: usize,
  pub reason: PromiseRejection,
}

#[derive(Clone, Debug)]
pub struct RejectionHandled {
  pub promise_id: usize,
}

#[derive(Default)]
struct RejectionTracker {
  about_to_be_notified: HashMap<usize, PromiseRejection>,
  unhandled: HashMap<usize, PromiseRejection>,
  unhandled_events: Vec<UnhandledRejection>,
  handled_events: Vec<RejectionHandled>,
}

static REJECTION_TRACKER: Lazy<GcAwareMutex<RejectionTracker>> =
  Lazy::new(|| GcAwareMutex::new(RejectionTracker::default()));

pub fn rt_take_unhandled_rejections() -> Vec<UnhandledRejection> {
  let mut tracker = REJECTION_TRACKER.lock();
  std::mem::take(&mut tracker.unhandled_events)
}

pub fn rt_take_rejection_handled() -> Vec<RejectionHandled> {
  let mut tracker = REJECTION_TRACKER.lock();
  std::mem::take(&mut tracker.handled_events)
}

pub(crate) fn reset_for_tests() {
  *REJECTION_TRACKER.lock() = RejectionTracker::default();
}

pub(crate) fn microtask_checkpoint_end() {
  let mut tracker = REJECTION_TRACKER.lock();
  for (promise_id, reason) in std::mem::take(&mut tracker.about_to_be_notified) {
    tracker.unhandled.insert(promise_id, reason.clone());
    tracker.unhandled_events.push(UnhandledRejection { promise_id, reason });
  }
}

fn mark_handled(header: &PromiseHeader) {
  if !header.mark_handled() {
    return;
  }

  let id = header as *const PromiseHeader as usize;
  let mut tracker = REJECTION_TRACKER.lock();

  if tracker.about_to_be_notified.remove(&id).is_some() {
    return;
  }
  if tracker.unhandled.remove(&id).is_some() {
    tracker.handled_events.push(RejectionHandled { promise_id: id });
  }
}

fn record_rejection_if_unhandled(header: &PromiseHeader, reason: &PromiseRejection) {
  let id = header as *const PromiseHeader as usize;
  let mut tracker = REJECTION_TRACKER.lock();

  if header.is_handled() {
    return;
  }

  tracker
    .about_to_be_notified
    .entry(id)
    .or_insert_with(|| reason.clone());
}

// -----------------------------------------------------------------------------
// Promise object + reactions.
// -----------------------------------------------------------------------------

#[repr(C)]
union PromisePayload<T> {
  fulfilled: ManuallyDrop<T>,
  rejected: ManuallyDrop<PromiseRejection>,
}

/// A Rust Promise type intended to mirror JavaScript’s Promise semantics.
///
/// The stable ABI contract is that the object begins with a [`PromiseHeader`] at offset 0.
#[repr(C)]
pub struct Promise<T> {
  pub header: PromiseHeader,
  payload: UnsafeCell<MaybeUninit<PromisePayload<T>>>,
  wakers: GcAwareMutex<Vec<Waker>>,
}

// Safety: `Promise` uses atomics for its externally-visible state and stores the payload exactly
// once before publishing the final state. `T: Send` is required because the value may be provided
// by a resolver on another thread. `T: Sync` is required because fulfilled values may be observed
// (cloned) from other threads once settled.
unsafe impl<T: Send> Send for Promise<T> {}
unsafe impl<T: Send + Sync> Sync for Promise<T> {}

impl<T> Drop for Promise<T> {
  fn drop(&mut self) {
    let state = self.header.state.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED {
      unsafe {
        let payload = &mut *(*self.payload.get()).as_mut_ptr();
        ManuallyDrop::drop(&mut payload.fulfilled);
      }
    } else if state == PromiseHeader::REJECTED {
      unsafe {
        let payload = &mut *(*self.payload.get()).as_mut_ptr();
        ManuallyDrop::drop(&mut payload.rejected);
      }
    }

    // Drop any reaction nodes that never ran (e.g. if the promise was dropped while pending).
    let mut head_val = self.header.waiters.swap(0, Ordering::AcqRel);
    if head_val == PromiseHeader::WAITERS_CLOSED {
      head_val = 0;
    }
    let mut head = head_val as *mut PromiseReactionNode;
    while !head.is_null() {
      let next = unsafe { (*head).next };
      let vtable = unsafe { (*head).vtable };
      if vtable.is_null() {
        std::process::abort();
      }
      ((unsafe { &*vtable }).drop)(head);
      head = next;
    }
  }
}

impl<T> Promise<T>
where
  T: Clone + Send + Sync + 'static,
{
  pub fn new() -> (Arc<Self>, PromiseResolver<T>, PromiseRejector<T>) {
    let p = Arc::new(Self {
      header: PromiseHeader {
        state: std::sync::atomic::AtomicU8::new(PromiseHeader::PENDING),
        waiters: std::sync::atomic::AtomicUsize::new(0),
        flags: std::sync::atomic::AtomicU8::new(0),
      },
      payload: UnsafeCell::new(MaybeUninit::uninit()),
      wakers: GcAwareMutex::new(Vec::new()),
    });
    let resolve = PromiseResolver { promise: p.clone() };
    let reject = PromiseRejector { promise: p.clone() };
    (p, resolve, reject)
  }

  pub fn resolve(value: T) -> Arc<Self> {
    let (p, resolve, _reject) = Self::new();
    resolve.resolve(value);
    p
  }

  pub fn reject(reason: PromiseRejection) -> Arc<Self> {
    let (p, _resolve, reject) = Self::new();
    reject.reject(reason);
    p
  }

  pub fn all<I>(iter: I) -> Arc<Promise<Vec<T>>>
  where
    I: IntoIterator<Item = Arc<Promise<T>>>,
  {
    let promises: Vec<_> = iter.into_iter().collect();
    let (out, resolve_out, reject_out) = Promise::<Vec<T>>::new();
    if promises.is_empty() {
      resolve_out.resolve(Vec::new());
      return out;
    }

    struct AllState<T> {
      remaining: usize,
      done: bool,
      results: Vec<Option<T>>,
    }

    let state = Arc::new(GcAwareMutex::new(AllState {
      remaining: promises.len(),
      done: false,
      results: vec![None; promises.len()],
    }));

    for (idx, p) in promises.into_iter().enumerate() {
      let state_fulfill = state.clone();
      let state_reject = state.clone();
      let resolve_out_fulfill = resolve_out.clone();
      let reject_out_reject = reject_out.clone();

      let _ = p.then(
        move |v| {
          let mut to_resolve: Option<Vec<T>> = None;
          {
            let mut st = state_fulfill.lock();
            if st.done {
              return ();
            }
            st.results[idx] = Some(v);
            st.remaining -= 1;
            if st.remaining == 0 {
              st.done = true;
              let mut vals = Vec::with_capacity(st.results.len());
              for slot in st.results.iter_mut() {
                vals.push(slot.take().unwrap());
              }
              to_resolve = Some(vals);
            }
          }
          if let Some(vals) = to_resolve {
            resolve_out_fulfill.resolve(vals);
          }
          ()
        },
        Some(move |e| {
          let mut should_reject = false;
          {
            let mut st = state_reject.lock();
            if !st.done {
              st.done = true;
              should_reject = true;
            }
          }
          if should_reject {
            reject_out_reject.reject(e);
          }
          ()
        }),
      );
    }

    out
  }

  pub fn race<I>(iter: I) -> Arc<Promise<T>>
  where
    I: IntoIterator<Item = Arc<Promise<T>>>,
  {
    let (out, resolve_out, reject_out) = Promise::<T>::new();
    for p in iter {
      let resolve_out = resolve_out.clone();
      let reject_out = reject_out.clone();
      let _ = p.then(
        move |v| {
          resolve_out.resolve(v);
          ()
        },
        Some(move |e| {
          reject_out.reject(e);
          ()
        }),
      );
    }
    out
  }

  pub fn all_settled<I>(iter: I) -> Arc<Promise<Vec<Settled<T>>>>
  where
    I: IntoIterator<Item = Arc<Promise<T>>>,
  {
    let promises: Vec<_> = iter.into_iter().collect();
    let (out, resolve_out, _reject_out) = Promise::<Vec<Settled<T>>>::new();
    if promises.is_empty() {
      resolve_out.resolve(Vec::new());
      return out;
    }

    struct State<T> {
      remaining: usize,
      results: Vec<Option<Settled<T>>>,
    }

    let state = Arc::new(GcAwareMutex::new(State {
      remaining: promises.len(),
      results: vec![None; promises.len()],
    }));

    for (idx, p) in promises.into_iter().enumerate() {
      let state_ok = state.clone();
      let state_err = state.clone();
      let resolve_ok = resolve_out.clone();
      let resolve_err = resolve_out.clone();

      let _ = p.then(
        move |v| {
          finish_all_settled(&state_ok, idx, Settled::Fulfilled(v), &resolve_ok);
          ()
        },
        Some(move |e| {
          finish_all_settled(&state_err, idx, Settled::Rejected(e), &resolve_err);
          ()
        }),
      );
    }

    fn finish_all_settled<T: Clone + Send + Sync + 'static>(
      state: &Arc<GcAwareMutex<State<T>>>,
      idx: usize,
      val: Settled<T>,
      resolve_out: &PromiseResolver<Vec<Settled<T>>>,
    ) {
      let mut to_resolve: Option<Vec<Settled<T>>> = None;
      {
        let mut st = state.lock();
        st.results[idx] = Some(val);
        st.remaining -= 1;
        if st.remaining == 0 {
          let mut out = Vec::with_capacity(st.results.len());
          for slot in st.results.iter_mut() {
            out.push(slot.take().unwrap());
          }
          to_resolve = Some(out);
        }
      }
      if let Some(v) = to_resolve {
        resolve_out.resolve(v);
      }
    }

    out
  }

  pub fn any<I>(iter: I) -> Arc<Promise<T>>
  where
    I: IntoIterator<Item = Arc<Promise<T>>>,
  {
    let promises: Vec<_> = iter.into_iter().collect();
    let (out, resolve_out, reject_out) = Promise::<T>::new();
    if promises.is_empty() {
      reject_out.reject(PromiseRejection::new(AggregateError { errors: Vec::new() }));
      return out;
    }

    struct AnyState {
      remaining: usize,
      done: bool,
      errors: Vec<Option<PromiseRejection>>,
    }

    let state = Arc::new(GcAwareMutex::new(AnyState {
      remaining: promises.len(),
      done: false,
      errors: vec![None; promises.len()],
    }));

    for (idx, p) in promises.into_iter().enumerate() {
      let state_fulfill = state.clone();
      let state_reject = state.clone();
      let resolve_out_fulfill = resolve_out.clone();
      let reject_out_reject = reject_out.clone();

      let _ = p.then(
        move |v| {
          let mut should_resolve = false;
          {
            let mut st = state_fulfill.lock();
            if !st.done {
              st.done = true;
              should_resolve = true;
            }
          }
          if should_resolve {
            resolve_out_fulfill.resolve(v);
          }
          ()
        },
        Some(move |e| {
          let mut to_reject: Option<PromiseRejection> = None;
          {
            let mut st = state_reject.lock();
            if st.done {
              return ();
            }
            st.errors[idx] = Some(e);
            st.remaining -= 1;
            if st.remaining == 0 {
              st.done = true;
              let mut errors = Vec::with_capacity(st.errors.len());
              for slot in st.errors.iter_mut() {
                errors.push(slot.take().unwrap());
              }
              to_reject = Some(PromiseRejection::new(AggregateError { errors }));
            }
          }
          if let Some(e) = to_reject {
            reject_out_reject.reject(e);
          }
          ()
        }),
      );
    }

    out
  }

  fn clone_fulfilled(&self) -> T {
    unsafe {
      let payload = &*(*self.payload.get()).as_ptr();
      let v = payload.fulfilled.clone();
      ManuallyDrop::into_inner(v)
    }
  }

  fn clone_rejected(&self) -> PromiseRejection {
    unsafe {
      let payload = &*(*self.payload.get()).as_ptr();
      let v = payload.rejected.clone();
      ManuallyDrop::into_inner(v)
    }
  }

  fn register_reaction(self: &Arc<Self>, node: *mut PromiseReactionNode) {
    if node.is_null() {
      return;
    }

    // Attaching any reaction (then/catch/finally/await) counts as "handled": rejections will be
    // observed by some continuation, even if the continuation simply propagates them to another
    // promise.
    mark_handled(&self.header);

    let waiters = &self.header.waiters;
    loop {
      let head_val = waiters.load(Ordering::Acquire);
      if head_val == PromiseHeader::WAITERS_CLOSED {
        // The list is closed (reserved for future async ABI); schedule directly.
        unsafe {
          (*node).next = null_mut();
        }
        let promise_ptr: PromiseRef = Arc::as_ptr(self).cast::<PromiseHeader>() as *mut PromiseHeader;
        enqueue_reaction_job(self.clone(), promise_ptr, node);
        return;
      }
      let head = head_val as *mut PromiseReactionNode;
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

    // If already settled, schedule immediately.
    let state = self.header.state.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
      self.drain_reactions();
    }
  }

  fn drain_reactions(self: &Arc<Self>) {
    let head_val = self.header.waiters.swap(0, Ordering::AcqRel);
    if head_val == 0 || head_val == PromiseHeader::WAITERS_CLOSED {
      return;
    }
    let mut head = head_val as *mut PromiseReactionNode;

    // Reactions are pushed in LIFO order; reverse to preserve FIFO registration order.
    head = unsafe { reverse_list(head) };

    let promise_ptr: PromiseRef = Arc::as_ptr(self).cast::<PromiseHeader>() as *mut PromiseHeader;
    while !head.is_null() {
      let next = unsafe { (*head).next };
      unsafe {
        (*head).next = null_mut();
      }
      enqueue_reaction_job(self.clone(), promise_ptr, head);
      head = next;
    }
  }

  fn fulfill(self: &Arc<Self>, value: T) {
    let state = &self.header.state;
    if state
      .compare_exchange(
        PromiseHeader::PENDING,
        STATE_FULFILLING,
        Ordering::AcqRel,
        Ordering::Acquire,
      )
      .is_err()
    {
      return;
    }

    unsafe {
      (*self.payload.get()).write(PromisePayload {
        fulfilled: ManuallyDrop::new(value),
      });
    }

    state.store(PromiseHeader::FULFILLED, Ordering::Release);
    self.drain_reactions();
    wake_waiters(&self.wakers);
  }

  fn settle_reject(self: &Arc<Self>, reason: PromiseRejection) {
    let state = &self.header.state;
    if state
      .compare_exchange(
        PromiseHeader::PENDING,
        STATE_REJECTING,
        Ordering::AcqRel,
        Ordering::Acquire,
      )
      .is_err()
    {
      return;
    }

    unsafe {
      (*self.payload.get()).write(PromisePayload {
        rejected: ManuallyDrop::new(reason.clone()),
      });
    }

    state.store(PromiseHeader::REJECTED, Ordering::Release);
    record_rejection_if_unhandled(&self.header, &reason);
    self.drain_reactions();
    wake_waiters(&self.wakers);
  }
}

fn wake_waiters(wakers: &GcAwareMutex<Vec<Waker>>) {
  let to_wake = std::mem::take(&mut *wakers.lock());
  for w in to_wake {
    w.wake();
  }
}

#[derive(Clone)]
pub struct PromiseResolver<T> {
  promise: Arc<Promise<T>>,
}

impl<T> PromiseResolver<T>
where
  T: Clone + Send + Sync + 'static,
{
  pub fn resolve(&self, value: T) {
    self.promise.fulfill(value);
  }
}

#[derive(Clone)]
pub struct PromiseRejector<T> {
  promise: Arc<Promise<T>>,
}

impl<T> PromiseRejector<T>
where
  T: Clone + Send + Sync + 'static,
{
  pub fn reject(&self, reason: PromiseRejection) {
    self.promise.settle_reject(reason);
  }
}

// A microtask job that owns a reaction node and keeps the promise allocation alive until the job
// completes.
#[repr(C)]
struct PromiseReactionJob {
  node: *mut PromiseReactionNode,
  promise: PromiseRef,
  // Holds a strong reference to the promise so the `PromiseRef` pointer remains valid.
  _keepalive: Arc<dyn Any + Send + Sync>,
}

extern "C" fn run_promise_reaction_job(data: *mut u8) {
  let job = unsafe { &mut *(data as *mut PromiseReactionJob) };
  let node = job.node;
  if node.is_null() {
    return;
  }
  let vtable = unsafe { (*node).vtable };
  if vtable.is_null() {
    std::process::abort();
  }
  ((unsafe { &*vtable }).run)(node, job.promise);
}

extern "C" fn drop_promise_reaction_job(data: *mut u8) {
  let job = unsafe { Box::from_raw(data as *mut PromiseReactionJob) };
  let node = job.node;
  if node.is_null() {
    return;
  }
  let vtable = unsafe { (*node).vtable };
  if vtable.is_null() {
    std::process::abort();
  }
  ((unsafe { &*vtable }).drop)(node);
}

fn enqueue_reaction_job<T: Any + Send + Sync + 'static>(
  promise: Arc<Promise<T>>,
  promise_ref: PromiseRef,
  node: *mut PromiseReactionNode,
) {
  let keepalive: Arc<dyn Any + Send + Sync> = promise.clone();
  let job = Box::new(PromiseReactionJob {
    node,
    promise: promise_ref,
    _keepalive: keepalive,
  });
  async_global().enqueue_microtask(Task::new_with_drop(
    run_promise_reaction_job,
    Box::into_raw(job) as *mut u8,
    drop_promise_reaction_job,
  ));
}

// -----------------------------------------------------------------------------
// `then` / `catch` / `finally`.
// -----------------------------------------------------------------------------

#[repr(C)]
struct ClosureReaction {
  node: PromiseReactionNode,
  vtable: PromiseReactionVTable,
  callback: Option<Box<dyn FnOnce(PromiseRef) + Send + 'static>>,
}

extern "C" fn closure_reaction_run(node: *mut PromiseReactionNode, promise: PromiseRef) {
  let reaction = unsafe { &mut *(node as *mut ClosureReaction) };
  let Some(cb) = reaction.callback.take() else {
    return;
  };
  cb(promise);
}

extern "C" fn closure_reaction_drop(node: *mut PromiseReactionNode) {
  unsafe {
    drop(Box::from_raw(node as *mut ClosureReaction));
  }
}

fn alloc_closure_reaction(cb: Box<dyn FnOnce(PromiseRef) + Send + 'static>) -> *mut PromiseReactionNode {
  let mut boxed = Box::new(ClosureReaction {
    node: PromiseReactionNode {
      next: null_mut(),
      vtable: std::ptr::null(),
    },
    vtable: PromiseReactionVTable {
      run: closure_reaction_run,
      drop: closure_reaction_drop,
    },
    callback: Some(cb),
  });
  let vtable_ptr: *const PromiseReactionVTable = &boxed.vtable;
  boxed.node.vtable = vtable_ptr;
  Box::into_raw(boxed) as *mut PromiseReactionNode
}

unsafe fn promise_from_header<'a, T>(p: PromiseRef) -> &'a Promise<T> {
  &*(p as *const PromiseHeader as *const Promise<T>)
}

fn forward_into<U>(
  promise: &Arc<Promise<U>>,
  resolve: PromiseResolver<U>,
  reject: PromiseRejector<U>,
) where
  U: Clone + Send + Sync + 'static,
{
  let node = alloc_closure_reaction(Box::new(move |p| unsafe {
    let src = promise_from_header::<U>(p);
    let state = src.header.state.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED {
      resolve.resolve(src.clone_fulfilled());
    } else if state == PromiseHeader::REJECTED {
      reject.reject(src.clone_rejected());
    }
  }));
  promise.register_reaction(node);
}

pub struct PromiseFuture<T> {
  promise: Arc<Promise<T>>,
}

impl<T> Future for PromiseFuture<T>
where
  T: Clone + Send + Sync + 'static,
{
  type Output = Result<T, PromiseRejection>;

  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let state = self.promise.header.state.load(Ordering::Acquire);
    match state {
      PromiseHeader::FULFILLED => Poll::Ready(Ok(self.promise.clone_fulfilled())),
      PromiseHeader::REJECTED => Poll::Ready(Err(self.promise.clone_rejected())),
      _ => {
        // Awaiting a promise counts as handling it (rejections are observed by the awaiting
        // continuation and propagate outward).
        mark_handled(&self.promise.header);

        let mut wakers = self.promise.wakers.lock();
        if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
          wakers.push(cx.waker().clone());
        }
        Poll::Pending
      }
    }
  }
}

pub trait PromiseExt<T> {
  fn then<U, F, R, G, S>(&self, on_fulfilled: F, on_rejected: Option<G>) -> Arc<Promise<U>>
  where
    T: Clone + Send + Sync + 'static,
    U: Clone + Send + Sync + 'static,
    F: FnOnce(T) -> R + Send + 'static,
    R: Into<PromiseReturn<U>>,
    G: FnOnce(PromiseRejection) -> S + Send + 'static,
    S: Into<PromiseReturn<U>>;

  fn then_ok<U, F, R>(&self, on_fulfilled: F) -> Arc<Promise<U>>
  where
    T: Clone + Send + Sync + 'static,
    U: Clone + Send + Sync + 'static,
    F: FnOnce(T) -> R + Send + 'static,
    R: Into<PromiseReturn<U>>,
  {
    self.then(on_fulfilled, Option::<fn(PromiseRejection) -> U>::None)
  }

  fn catch<F, R>(&self, on_rejected: F) -> Arc<Promise<T>>
  where
    T: Clone + Send + Sync + 'static,
    F: FnOnce(PromiseRejection) -> R + Send + 'static,
    R: Into<PromiseReturn<T>>,
  {
    self.then(|v| v, Some(on_rejected))
  }

  fn finally<F, R>(&self, on_finally: F) -> Arc<Promise<T>>
  where
    T: Clone + Send + Sync + 'static,
    F: FnOnce() -> R + Send + 'static,
    R: Into<PromiseReturn<()>>;

  fn into_future(self) -> PromiseFuture<T>
  where
    T: Clone + Send + Sync + 'static;
}

impl<T> PromiseExt<T> for Arc<Promise<T>>
where
  T: Clone + Send + Sync + 'static,
{
  fn then<U, F, R, G, S>(&self, on_fulfilled: F, on_rejected: Option<G>) -> Arc<Promise<U>>
  where
    U: Clone + Send + Sync + 'static,
    F: FnOnce(T) -> R + Send + 'static,
    R: Into<PromiseReturn<U>>,
    G: FnOnce(PromiseRejection) -> S + Send + 'static,
    S: Into<PromiseReturn<U>>,
  {
    let (out, resolve_out, reject_out) = Promise::<U>::new();

    let on_rejected: Option<Box<dyn FnOnce(PromiseRejection) -> PromiseReturn<U> + Send + 'static>> =
      on_rejected.map(|f| Box::new(move |e| f(e).into()) as _);
    let on_fulfilled: Box<dyn FnOnce(T) -> PromiseReturn<U> + Send + 'static> =
      Box::new(move |v| on_fulfilled(v).into());

    let node = alloc_closure_reaction(Box::new(move |p| unsafe {
      let src = promise_from_header::<T>(p);
      let state = src.header.state.load(Ordering::Acquire);

      if state == PromiseHeader::FULFILLED {
        let v = src.clone_fulfilled();
        let ret = on_fulfilled(v);
        match ret {
          PromiseReturn::Value(v) => resolve_out.resolve(v),
          PromiseReturn::Promise(p) => forward_into(&p, resolve_out, reject_out),
        }
        return;
      }

      if state == PromiseHeader::REJECTED {
        let e = src.clone_rejected();
        let Some(on_rejected) = on_rejected else {
          reject_out.reject(e);
          return;
        };
        let ret = on_rejected(e);
        match ret {
          PromiseReturn::Value(v) => resolve_out.resolve(v),
          PromiseReturn::Promise(p) => forward_into(&p, resolve_out, reject_out),
        }
      }
    }));

    self.register_reaction(node);
    out
  }

  fn finally<F, R>(&self, on_finally: F) -> Arc<Promise<T>>
  where
    F: FnOnce() -> R + Send + 'static,
    R: Into<PromiseReturn<()>>,
  {
    let on_finally = Arc::new(GcAwareMutex::new(Some(on_finally)));

    self.then(
      {
        let on_finally = on_finally.clone();
        move |v: T| {
          let fin = on_finally
            .lock()
            .take()
            .expect("finally callback should only run once")();
          match fin.into() {
            PromiseReturn::Value(()) => PromiseReturn::Value(v),
            PromiseReturn::Promise(p) => PromiseReturn::Promise(p.then_ok(move |_| v)),
          }
        }
      },
      Some({
        let on_finally = on_finally.clone();
        move |e: PromiseRejection| {
          let fin = on_finally
            .lock()
            .take()
            .expect("finally callback should only run once")();
          match fin.into() {
            PromiseReturn::Value(()) => PromiseReturn::Promise(Promise::<T>::reject(e)),
            PromiseReturn::Promise(p) => PromiseReturn::Promise(p.then(
              move |_| Promise::<T>::reject(e.clone()),
              Some(|fin_err| Promise::<T>::reject(fin_err)),
            )),
          }
        }
      }),
    )
  }

  fn into_future(self) -> PromiseFuture<T> {
    PromiseFuture { promise: self }
  }
}

// -----------------------------------------------------------------------------
// Debug / test hooks
// -----------------------------------------------------------------------------

/// Test-only hook: execute `f` while holding the global unhandled-rejection tracker lock.
///
/// This exists to deterministically force contention on the promise API's internal rejection
/// tracker for stop-the-world safepoint tests.
#[doc(hidden)]
pub fn debug_with_rejection_tracker_lock<R>(f: impl FnOnce() -> R) -> R {
  let _guard = REJECTION_TRACKER.lock();
  f()
}

/// Test-only hook: execute `f` while holding a promise's waker list lock.
///
/// This exists to deterministically force contention on the per-promise waker mutex for
/// stop-the-world safepoint tests.
#[doc(hidden)]
pub fn debug_with_promise_wakers_lock<T, R>(promise: &Arc<Promise<T>>, f: impl FnOnce() -> R) -> R {
  let _guard = promise.wakers.lock();
  f()
}
