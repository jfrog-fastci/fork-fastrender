#![cfg(target_os = "macos")]

use runtime_native::stackwalk::StackBounds;
use runtime_native::threading;
use runtime_native::threading::registry;
use runtime_native::threading::ThreadKind;

#[test]
fn stackwalk_current_thread_bounds_contains_local() {
  let mut local = 123u64;
  let local_addr = (&mut local as *mut u64) as u64;
  std::hint::black_box(local_addr);

  let bounds = StackBounds::current_thread().expect("failed to read current stack bounds");
  assert!(bounds.lo < bounds.hi);
  assert!(bounds.contains_range(
    local_addr,
    core::mem::size_of::<u64>() as u64
  ));
}

#[test]
fn thread_registry_records_stack_bounds() {
  threading::unregister_current_thread();
  threading::register_current_thread(ThreadKind::External);

  let state = registry::current_thread_state().expect("thread should be registered");
  let bounds = state.stack_bounds().expect("stack bounds should be captured on macOS");

  let mut local = 456u64;
  let local_addr = (&mut local as *mut u64) as usize;
  std::hint::black_box(local_addr);
  assert!(bounds.lo <= local_addr && local_addr < bounds.hi);

  threading::unregister_current_thread();
}

