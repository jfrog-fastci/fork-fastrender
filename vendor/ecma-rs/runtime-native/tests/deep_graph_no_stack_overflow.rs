use std::mem;
use std::process::Command;
use std::ptr;
use std::thread;

use runtime_native::gc::{ObjHeader, RememberedSet, RootStack, TypeDescriptor};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;

const CHILD_ENV: &str = "ECMA_RS_RUNTIME_NATIVE_DEEP_GRAPH_CHILD";
// Keep this large enough to overflow a recursive marker but small enough to keep the test fast.
const NODES: usize = 50_000;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
}

static NODE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(Node, next) as u32];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn deep_graph_child() {
  if std::env::var_os(CHILD_ENV).is_none() {
    return;
  }

  // Run in a small-stack thread so a recursive mark implementation reliably overflows (and does so
  // quickly) while keeping this regression test fast.
  let handle = thread::Builder::new()
    .name("deep_graph_child".to_string())
    .stack_size(512 * 1024)
    .spawn(|| {
      let _rt = TestRuntimeGuard::new();
      let mut heap = GcHeap::new();
      let mut remembered = NullRememberedSet::default();

      let mut head: *mut u8 = ptr::null_mut();
      for _ in 0..NODES {
        let node = heap.alloc_young(&NODE_DESC);
        unsafe {
          (*(node as *mut Node)).next = head;
        }
        head = node;
      }

      let mut root_head = head;
      let mut roots = RootStack::new();
      roots.push(&mut root_head as *mut *mut u8);

      // Previously this would recurse once per node and overflow the Rust call stack.
      heap.collect_minor(&mut roots, &mut remembered).unwrap();
      assert!(!heap.is_in_nursery(root_head));

      // Major marking must also be iterative; a deep old-gen graph should not overflow either.
      heap.collect_major(&mut roots, &mut remembered).unwrap();
      assert!(!heap.is_in_nursery(root_head));
    })
    .expect("spawn deep_graph_child thread");
  handle.join().expect("deep_graph_child thread panicked");
}

#[test]
fn deep_graph_no_stack_overflow() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env(CHILD_ENV, "1")
    .arg("--exact")
    .arg("deep_graph_child")
    .status()
    .expect("spawn child");

  assert!(
    status.success(),
    "deep graph child process failed (likely stack overflow): {status:?}"
  );
}
