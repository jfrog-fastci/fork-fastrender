//! GC stress test for `runtime-native`.
//!
//! Builds random heap graphs, triggers frequent minor/major GC cycles, and runs
//! heap verification to catch bugs early.
//!
//! Example bug this should detect:
//! - If the write barrier fails to record an old->nursery pointer in the
//!   remembered set, minor GC will miss that edge. After the nursery is reset
//!   (and poisoned in debug builds), the old object's slot will still point
//!   into nursery memory and `verify_from_roots` will fail.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use runtime_native::gc::GcHeap;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::gc::OBJ_HEADER_SIZE;
use runtime_native::test_util::TestRuntimeGuard;
use std::collections::HashMap;
use std::collections::HashSet;
use std::mem;
use std::ptr;

const ROOT_SLOTS: usize = 256;

// Layout: [ObjHeader][kind: u32][len: u32][payload...]
const META_SIZE: usize = 8;
const META_KIND_OFFSET: usize = OBJ_HEADER_SIZE;
const META_LEN_OFFSET: usize = OBJ_HEADER_SIZE + 4;
const PAYLOAD_OFFSET: usize = OBJ_HEADER_SIZE + META_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ObjKind {
  Fixed = 0,
  PtrArray = 1,
  Bytes = 2,
}

fn align_up(n: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (n + (align - 1)) & !(align - 1)
}

fn write_u32(obj: *mut u8, offset: usize, value: u32) {
  // SAFETY: The object is GC-allocated and this offset is within the payload.
  unsafe { *(obj.add(offset) as *mut u32) = value };
}

fn read_u32(obj: *mut u8, offset: usize) -> u32 {
  // SAFETY: The object is GC-allocated and this offset is within the payload.
  unsafe { *(obj.add(offset) as *const u32) }
}

fn obj_kind(obj: *mut u8) -> ObjKind {
  match read_u32(obj, META_KIND_OFFSET) {
    0 => ObjKind::Fixed,
    1 => ObjKind::PtrArray,
    _ => ObjKind::Bytes,
  }
}

fn obj_len(obj: *mut u8) -> usize {
  read_u32(obj, META_LEN_OFFSET) as usize
}

unsafe fn ptr_slot(obj: *mut u8, idx: usize) -> *mut *mut u8 {
  (obj.add(PAYLOAD_OFFSET) as *mut *mut u8).add(idx)
}

struct DescCache {
  map: HashMap<(ObjKind, usize), &'static TypeDescriptor>,
}

impl DescCache {
  fn new() -> Self {
    Self { map: HashMap::new() }
  }

  fn get(&mut self, kind: ObjKind, len: usize) -> &'static TypeDescriptor {
    if let Some(&d) = self.map.get(&(kind, len)) {
      return d;
    }

    let ptr_size = mem::size_of::<*mut u8>();
    let align = mem::align_of::<*mut u8>();

    let (size, ptr_offsets): (usize, Vec<u32>) = match kind {
      ObjKind::Fixed | ObjKind::PtrArray => {
        let size = align_up(PAYLOAD_OFFSET + len * ptr_size, align);
        let mut offsets = Vec::with_capacity(len);
        for i in 0..len {
          let off = PAYLOAD_OFFSET + i * ptr_size;
          offsets.push(u32::try_from(off).expect("pointer offset must fit in u32"));
        }
        (size, offsets)
      }
      ObjKind::Bytes => (align_up(PAYLOAD_OFFSET + len, align), Vec::new()),
    };

    let offsets: &'static [u32] = Box::leak(ptr_offsets.into_boxed_slice());
    let desc: &'static TypeDescriptor = Box::leak(Box::new(TypeDescriptor::new(size, offsets)));
    self.map.insert((kind, len), desc);
    desc
  }
}

#[derive(Default)]
struct TestRemembered {
  objs: Vec<*mut u8>,
  set: HashSet<usize>,
}

impl TestRemembered {
  fn remember(&mut self, obj: *mut u8) {
    if obj.is_null() {
      return;
    }
    if self.set.insert(obj as usize) {
      self.objs.push(obj);
    }
  }
}

impl RememberedSet for TestRemembered {
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8)) {
    for &obj in &self.objs {
      f(obj);
    }
  }

  fn clear(&mut self) {
    self.objs.clear();
    self.set.clear();
  }

  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

fn collect_reachable(root_slots: &[Box<*mut u8>]) -> Vec<*mut u8> {
  let mut out = Vec::new();
  let mut stack = Vec::new();
  let mut seen: HashSet<usize> = HashSet::new();

  for slot in root_slots {
    let obj = **slot;
    if !obj.is_null() {
      stack.push(obj);
    }
  }

  while let Some(obj) = stack.pop() {
    if !seen.insert(obj as usize) {
      continue;
    }
    out.push(obj);

    if matches!(obj_kind(obj), ObjKind::Bytes) {
      continue;
    }
    let len = obj_len(obj);
    for i in 0..len {
      // SAFETY: We only construct objects with contiguous pointer slots.
      let child = unsafe { *ptr_slot(obj, i) };
      if !child.is_null() {
        stack.push(child);
      }
    }
  }

  out
}

fn pick_ptr_object(rng: &mut impl Rng, objs: &[*mut u8]) -> Option<*mut u8> {
  if objs.is_empty() {
    return None;
  }
  for _ in 0..16 {
    let obj = objs[rng.random_range(0..objs.len())];
    if !matches!(obj_kind(obj), ObjKind::Bytes) && obj_len(obj) != 0 {
      return Some(obj);
    }
  }
  None
}

fn write_ptr_slot(
  heap: &GcHeap,
  remembered: &mut TestRemembered,
  obj: *mut u8,
  idx: usize,
  value: *mut u8,
) {
  // SAFETY: The slot is in-bounds by construction and `obj` is a valid object.
  unsafe {
    *ptr_slot(obj, idx) = value;
  }

  // Manual write barrier: old -> nursery edge.
  if !value.is_null() && !heap.is_in_nursery(obj) && heap.is_in_nursery(value) {
    remembered.remember(obj);
  }
}

fn run_stress(seed: u64, ops: usize) {
  let mut heap = GcHeap::with_nursery_size(64 * 1024);
  let mut descs = DescCache::new();
  let mut remembered = TestRemembered::default();

  // Stable GC root slots.
  let mut root_slots: Vec<Box<*mut u8>> = (0..ROOT_SLOTS).map(|_| Box::new(ptr::null_mut())).collect();
  let mut roots = RootStack::new();
  for slot in &mut root_slots {
    roots.push(&mut **slot as *mut *mut u8);
  }

  let mut rng = StdRng::seed_from_u64(seed);
  let mut reachable: Vec<*mut u8> = Vec::new();

  for i in 0..ops {
    // Periodic collections.
    if i % 97 == 0 {
      reachable.clear();
      heap.collect_minor(&mut roots, &mut remembered).unwrap();
      heap.verify_from_roots(&mut roots);
    }
    if i % 997 == 0 {
      reachable.clear();
      heap.collect_major(&mut roots, &mut remembered).unwrap();
      heap.verify_from_roots(&mut roots);
    }

    if reachable.is_empty() {
      reachable = collect_reachable(&root_slots);
    }

    match rng.random_range(0..100u32) {
      // Allocate new objects.
      0..=39 => {
        let kind_roll = rng.random_range(0..100u32);
        let force_old = rng.random_ratio(1, 10);

        let (kind, len, alloc_old) = match kind_roll {
          0..=49 => (ObjKind::Fixed, rng.random_range(0..=8usize), force_old),
          50..=79 => (ObjKind::PtrArray, rng.random_range(0..=16usize), force_old),
          _ => {
            // Allocate some large objects directly into old/LOS.
            let len = rng.random_range(0..=2048usize);
            let alloc_old = force_old || len >= 1024;
            (ObjKind::Bytes, len, alloc_old)
          }
        };

        let desc = descs.get(kind, len);
        let obj = if alloc_old { heap.alloc_old(desc) } else { heap.alloc_young(desc) };

        write_u32(obj, META_KIND_OFFSET, kind as u32);
        write_u32(obj, META_LEN_OFFSET, len as u32);

        // Randomly make it a root or link it from another reachable pointer object.
        if rng.random_bool(0.6) {
          let slot_idx = rng.random_range(0..ROOT_SLOTS);
          *root_slots[slot_idx] = obj;
        } else if let Some(container) = pick_ptr_object(&mut rng, &reachable) {
          let slots = obj_len(container);
          if slots != 0 {
            let idx = rng.random_range(0..slots);
            write_ptr_slot(&heap, &mut remembered, container, idx, obj);
          }
        }

        reachable.push(obj);
      }

      // Wire pointers between objects (including cycles).
      40..=69 => {
        let Some(container) = pick_ptr_object(&mut rng, &reachable) else { continue };
        let slots = obj_len(container);
        if slots == 0 {
          continue;
        }
        let idx = rng.random_range(0..slots);
        let value = if rng.random_ratio(1, 6) || reachable.is_empty() {
          ptr::null_mut()
        } else {
          reachable[rng.random_range(0..reachable.len())]
        };
        write_ptr_slot(&heap, &mut remembered, container, idx, value);
      }

      // Drop roots (set root slots to null).
      70..=84 => {
        let slot_idx = rng.random_range(0..ROOT_SLOTS);
        *root_slots[slot_idx] = ptr::null_mut();
        reachable.clear();
      }

      // Overwrite a field with null.
      85..=94 => {
        let Some(container) = pick_ptr_object(&mut rng, &reachable) else { continue };
        let slots = obj_len(container);
        if slots == 0 {
          continue;
        }
        let idx = rng.random_range(0..slots);
        write_ptr_slot(&heap, &mut remembered, container, idx, ptr::null_mut());
      }

      // Explicit minor.
      95..=98 => {
        reachable.clear();
        heap.collect_minor(&mut roots, &mut remembered).unwrap();
        heap.verify_from_roots(&mut roots);
      }

      // Explicit major.
      _ => {
        reachable.clear();
        heap.collect_major(&mut roots, &mut remembered).unwrap();
        heap.verify_from_roots(&mut roots);
      }
    }
  }

  reachable.clear();
  heap.collect_major(&mut roots, &mut remembered).unwrap();
  heap.verify_from_roots(&mut roots);
}

#[test]
fn gc_stress() {
  let _rt = TestRuntimeGuard::new();
  // Keep this small so it runs quickly in debug builds.
  for seed in 0..8u64 {
    run_stress(seed, 2_500);
  }
}

#[test]
#[ignore]
fn gc_stress_soak() {
  let _rt = TestRuntimeGuard::new();
  run_stress(0xA11C_E5ED_u64, 200_000);
}
