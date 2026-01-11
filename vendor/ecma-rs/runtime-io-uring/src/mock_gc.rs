//! A small moving/compacting GC simulation used by tests.
//!
//! - Objects are identified by [`MockGcHandle`].
//! - Handles do **not** keep objects alive across [`MockGc::collect`] (roots do).
//! - Unpinned rooted objects relocate on collection (their backing store pointer changes).
//! - Pinned rooted objects do not relocate.

use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use crate::gc::{GcHooks, GcPinGuard, GcRoot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MockGcHandle(u64);

#[derive(Debug)]
struct Obj {
    data: Vec<u8>,
    roots: usize,
    pins: usize,
    root_drops: usize,
    pin_drops: usize,
}

#[derive(Debug, Default)]
struct Inner {
    next_id: u64,
    objs: HashMap<u64, Obj>,
}

/// A mock moving GC.
#[derive(Clone, Debug, Default)]
pub struct MockGc {
    inner: Arc<Mutex<Inner>>,
}

impl MockGc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc(&self, data: Vec<u8>) -> MockGcHandle {
        let mut inner = self.inner.lock().expect("poisoned mutex");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.objs.insert(
            id,
            Obj {
                data,
                roots: 0,
                pins: 0,
                root_drops: 0,
                pin_drops: 0,
            },
        );
        MockGcHandle(id)
    }

    pub fn alloc_zeroed(&self, len: usize) -> MockGcHandle {
        self.alloc(vec![0; len])
    }

    pub fn root_count(&self, h: MockGcHandle) -> usize {
        self.with_obj(h, |o| o.roots).unwrap_or(0)
    }

    pub fn pin_count(&self, h: MockGcHandle) -> usize {
        self.with_obj(h, |o| o.pins).unwrap_or(0)
    }

    pub fn root_drops(&self, h: MockGcHandle) -> usize {
        self.with_obj(h, |o| o.root_drops).unwrap_or(0)
    }

    pub fn pin_drops(&self, h: MockGcHandle) -> usize {
        self.with_obj(h, |o| o.pin_drops).unwrap_or(0)
    }

    pub fn len(&self, h: MockGcHandle) -> Option<usize> {
        self.with_obj(h, |o| o.data.len())
    }

    pub fn ptr(&self, h: MockGcHandle) -> Option<NonNull<u8>> {
        self.with_obj(h, |o| {
            NonNull::new(o.data.as_ptr() as *mut u8).expect("Vec pointer is never null")
        })
    }

    fn with_obj<T>(&self, h: MockGcHandle, f: impl FnOnce(&Obj) -> T) -> Option<T> {
        let inner = self.inner.lock().expect("poisoned mutex");
        inner.objs.get(&h.0).map(f)
    }

    fn with_obj_mut<T>(&self, h: MockGcHandle, f: impl FnOnce(&mut Obj) -> T) -> Option<T> {
        let mut inner = self.inner.lock().expect("poisoned mutex");
        inner.objs.get_mut(&h.0).map(f)
    }

    /// Run a simulated GC collection:
    /// - unrooted objects are collected
    /// - rooted but unpinned objects relocate (pointer changes)
    /// - pinned objects do not relocate
    pub fn collect(&self) {
        let mut inner = self.inner.lock().expect("poisoned mutex");
        let ids: Vec<u64> = inner.objs.keys().copied().collect();
        for id in ids {
            let Some(obj) = inner.objs.get_mut(&id) else {
                continue;
            };

            if obj.roots == 0 {
                inner.objs.remove(&id);
                continue;
            }

            if obj.pins == 0 {
                // Relocate by allocating a new buffer before dropping the old one, to avoid the
                // allocator reusing the same address.
                let old = std::mem::take(&mut obj.data);
                let mut new = vec![0u8; old.len()];
                new.copy_from_slice(&old);
                obj.data = new;
                drop(old);
            }
        }
    }
}

impl GcHooks for MockGc {
    type Buffer = MockGcHandle;
    type Root = MockGcRoot;

    fn root(&self, buffer: Self::Buffer) -> Self::Root {
        // Rooting is fallible in real GCs; for the mock we just panic on invalid handles.
        self.with_obj_mut(buffer, |o| o.roots += 1)
            .expect("invalid MockGcHandle");
        MockGcRoot {
            gc: self.clone(),
            handle: buffer,
        }
    }
}

#[derive(Debug)]
pub struct MockGcRoot {
    gc: MockGc,
    handle: MockGcHandle,
}

impl Drop for MockGcRoot {
    fn drop(&mut self) {
        self.gc
            .with_obj_mut(self.handle, |o| {
                o.roots -= 1;
                o.root_drops += 1;
            })
            .expect("invalid MockGcHandle");
    }
}

impl GcRoot for MockGcRoot {
    type PinGuard = MockGcPinGuard;

    fn len(&self) -> usize {
        self.gc.len(self.handle).expect("invalid MockGcHandle")
    }

    fn pin(&self) -> Self::PinGuard {
        self.gc
            .with_obj_mut(self.handle, |o| o.pins += 1)
            .expect("invalid MockGcHandle");
        MockGcPinGuard {
            gc: self.gc.clone(),
            handle: self.handle,
        }
    }

    fn stable_ptr(&self, _pin: &Self::PinGuard) -> *mut u8 {
        self.gc
            .ptr(self.handle)
            .expect("invalid MockGcHandle")
            .as_ptr()
    }
}

#[derive(Debug)]
pub struct MockGcPinGuard {
    gc: MockGc,
    handle: MockGcHandle,
}

impl Drop for MockGcPinGuard {
    fn drop(&mut self) {
        self.gc
            .with_obj_mut(self.handle, |o| {
                o.pins -= 1;
                o.pin_drops += 1;
            })
            .expect("invalid MockGcHandle");
    }
}

impl GcPinGuard for MockGcPinGuard {}

