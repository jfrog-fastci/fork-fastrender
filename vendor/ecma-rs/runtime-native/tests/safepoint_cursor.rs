use runtime_native::FrameCursor;

#[cfg(target_arch = "x86_64")]
mod x86_64 {
  use super::*;
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::threading;
  use runtime_native::threading::ThreadKind;

  static mut EXPECTED_TEST_PC: usize = 0;
  static mut EXPECTED_OUTER_FP: usize = 0;
  static mut EXPECTED_OUTER_PC: usize = 0;
  static mut EXPECTED_INNER_FP: usize = 0;
  static mut EXPECTED_INNER_PC: usize = 0;

  static mut OBSERVED_INNER_FP: usize = 0;
  static mut OBSERVED_INNER_PC: usize = 0;
  static mut OBSERVED_OUTER_FP: usize = 0;
  static mut OBSERVED_OUTER_PC: usize = 0;
  static mut OBSERVED_TEST_PC: usize = 0;
  static mut HOOK_CALLED: bool = false;

  extern "C" fn hook(cursor: FrameCursor) {
    unsafe {
      HOOK_CALLED = true;
      OBSERVED_INNER_FP = cursor.fp;
      OBSERVED_INNER_PC = cursor.pc;

      let inner_fp = cursor.fp as *const usize;
      OBSERVED_OUTER_FP = inner_fp.read();
      OBSERVED_OUTER_PC = inner_fp.add(1).read();

      let outer_fp = OBSERVED_OUTER_FP as *const usize;
      OBSERVED_TEST_PC = outer_fp.add(1).read();
    }

    // Unblock the caller: `rt_gc_safepoint`'s slow path waits for the world to
    // be resumed. For this test we act as our own GC coordinator.
    runtime_native::rt_gc_resume_world();
  }

  #[unsafe(naked)]
  unsafe extern "C" fn inner() {
    core::arch::naked_asm!(
      "push rbp",
      "mov rbp, rsp",
      "mov qword ptr [rip + {expected_fp}], rbp",
      "lea rax, [rip + 2f]",
      "mov qword ptr [rip + {expected_pc}], rax",
      "call {safepoint}",
      "2:",
      "pop rbp",
      "ret",
      expected_fp = sym EXPECTED_INNER_FP,
      expected_pc = sym EXPECTED_INNER_PC,
      safepoint = sym runtime_native::rt_gc_safepoint,
    );
  }

  #[unsafe(naked)]
  unsafe extern "C" fn outer() {
    core::arch::naked_asm!(
      "push rbp",
      "mov rbp, rsp",
      "mov qword ptr [rip + {expected_fp}], rbp",
      "lea rax, [rip + 2f]",
      "mov qword ptr [rip + {expected_pc}], rax",
      "call {inner}",
      "2:",
      "pop rbp",
      "ret",
      expected_fp = sym EXPECTED_OUTER_FP,
      expected_pc = sym EXPECTED_OUTER_PC,
      inner = sym inner,
    );
  }

  #[test]
  fn captures_mutator_caller_cursor_and_walks_frames() {
    let _rt = TestRuntimeGuard::new();
    threading::register_current_thread(ThreadKind::Main);

    runtime_native::rt_gc_request_stop_the_world();
    runtime_native::set_rt_gc_safepoint_hook(Some(hook));

    unsafe {
      core::arch::asm!(
        "lea rax, [rip + 2f]",
        "mov qword ptr [rip + {expected_pc}], rax",
        "call {outer}",
        "2:",
        expected_pc = sym EXPECTED_TEST_PC,
        outer = sym outer,
        out("rax") _,
        clobber_abi("C"),
      );
    }

    runtime_native::set_rt_gc_safepoint_hook(None);
    runtime_native::rt_gc_resume_world();

    let cursor = runtime_native::current_thread_safepoint_cursor();
    unsafe {
      let hook_called = HOOK_CALLED;
      let expected_inner_fp = EXPECTED_INNER_FP;
      let expected_inner_pc = EXPECTED_INNER_PC;
      let expected_outer_fp = EXPECTED_OUTER_FP;
      let expected_outer_pc = EXPECTED_OUTER_PC;
      let expected_test_pc = EXPECTED_TEST_PC;

      let observed_inner_fp = OBSERVED_INNER_FP;
      let observed_inner_pc = OBSERVED_INNER_PC;
      let observed_outer_fp = OBSERVED_OUTER_FP;
      let observed_outer_pc = OBSERVED_OUTER_PC;
      let observed_test_pc = OBSERVED_TEST_PC;

      assert!(hook_called, "expected safepoint hook to run");

      assert_eq!(cursor.fp, expected_inner_fp);
      assert_eq!(cursor.pc, expected_inner_pc);

      assert_eq!(observed_inner_fp, expected_inner_fp);
      assert_eq!(observed_inner_pc, expected_inner_pc);
      assert_eq!(observed_outer_fp, expected_outer_fp);
      assert_eq!(observed_outer_pc, expected_outer_pc);
      assert_eq!(observed_test_pc, expected_test_pc);
    }

    threading::unregister_current_thread();
  }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[test]
fn aarch64_safepoint_stub_disassembles_with_fp_lr_capture() {
  use std::path::PathBuf;
  use std::process::Command;

  let tempdir = tempfile::tempdir().expect("tempdir");
  let shim_rs = tempdir.path().join("safepoint_shim.rs");
  let obj_path = tempdir.path().join("safepoint_shim.o");

  let safepoint_rs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("src")
    .join("safepoint.rs");
  let safepoint_rs = safepoint_rs.to_string_lossy();

  // Compile the real `runtime-native/src/safepoint.rs` in isolation. We stub the
  // `crate::threading` APIs it depends on so this remains a cheap disassembly
  // sanity check without pulling in runtime-native's full dependency graph.
  std::fs::write(
    &shim_rs,
    format!(
      r#"
      // Minimal stubs for `runtime-native/src/safepoint.rs`.
       pub mod arch {{
         pub const WORD_SIZE: usize = 8;
         #[derive(Clone, Copy, Debug, Default)]
         #[repr(C)]
         pub struct SafepointContext {{
           pub sp_entry: usize,
           pub sp: usize,
           pub fp: usize,
           pub ip: usize,
         }}
         pub fn capture_safepoint_context() -> SafepointContext {{
           SafepointContext::default()
         }}
       }}

       pub mod thread_stack {{
         #[derive(Clone, Copy, Debug)]
         pub struct StackBounds {{
           pub low: usize,
           pub high: usize,
         }}
         pub fn current_thread_stack_bounds() -> Result<StackBounds, ()> {{
           Err(())
         }}
       }}

       pub mod stackwalk {{
         #[derive(Clone, Copy, Debug)]
         pub struct StackBounds {{
           pub lo: u64,
           pub hi: u64,
         }}
         impl StackBounds {{
           pub fn new(lo: u64, hi: u64) -> Result<Self, ()> {{
             let _ = (lo, hi);
             Ok(Self {{ lo, hi }})
           }}

           pub fn current_thread() -> Result<Self, ()> {{
             Err(())
           }}
         }}

         #[derive(Clone, Copy, Debug)]
         pub struct ManagedCursor {{
           pub sp: Option<u64>,
           pub fp: u64,
           pub pc: u64,
         }}

         pub fn find_nearest_managed_cursor_from_here(
           _stackmaps: &crate::stackmap::StackMaps,
         ) -> Option<ManagedCursor> {{
           None
         }}
       }}

       #[derive(Debug)]
       pub struct WalkError;

       pub mod stackmap {{
         pub struct StackMaps;
         pub fn try_stackmaps() -> Option<&'static StackMaps> {{
           None
         }}
       }}

       pub unsafe fn walk_gc_roots_from_fp(
         _start_fp: u64,
         _bounds: Option<crate::stackwalk::StackBounds>,
         _stackmaps: &crate::stackmap::StackMaps,
         _visit: impl FnMut(*mut u8),
       ) -> Result<(), crate::WalkError> {{
         Ok(())
       }}

       pub mod stackwalk_fp {{
         pub unsafe fn walk_gc_roots_from_safepoint_context(
           _ctx: &crate::arch::SafepointContext,
           _bounds: Option<crate::stackwalk::StackBounds>,
           _stackmaps: &crate::stackmap::StackMaps,
           _visit: impl FnMut(*mut u8),
         ) -> Result<(), crate::WalkError> {{
           Ok(())
         }}
       }}

       pub fn rt_gc_request_stop_the_world() -> u64 {{
         0
       }}
       pub fn rt_gc_wait_for_world_stopped_timeout(_timeout: std::time::Duration) -> bool {{
         true
       }}
       pub fn rt_gc_wait_for_world_resumed_timeout(_timeout: std::time::Duration) -> bool {{
         true
       }}
       pub fn rt_gc_resume_world() -> u64 {{
         0
       }}

       pub mod threading {{
        pub mod registry {{
          use std::sync::Arc;
          use crate::arch::SafepointContext;

          #[derive(Clone, Copy, Debug)]
          pub struct ThreadId(u64);

          #[derive(Clone, Copy, Debug)]
          pub struct StackBounds {{
            pub lo: usize,
            pub hi: usize,
          }}

          pub struct ThreadState;
          impl ThreadState {{
            pub fn safepoint_cursor(&self) -> Option<crate::safepoint::FrameCursor> {{
              None
            }}

            pub fn safepoint_context(&self) -> Option<SafepointContext> {{
              None
            }}

            pub fn stack_bounds(&self) -> Option<StackBounds> {{
              None
            }}
          }}
          pub fn current_thread_state() -> Option<Arc<ThreadState>> {{
            None
          }}
          pub fn current_thread_id() -> Option<ThreadId> {{
            None
          }}
          pub(crate) fn set_current_thread_safepoint_cursor(_cursor: crate::safepoint::FrameCursor) {{}}
          pub(crate) fn set_current_thread_safepoint_context(_ctx: SafepointContext) {{}}
          pub(crate) fn set_current_thread_safepoint_epoch_observed(_epoch: u64) {{}}
        }}
         pub mod safepoint {{
           pub(crate) fn current_epoch() -> u64 {{
             0
           }}
           pub(crate) fn notify_state_change() {{}}
           pub(crate) fn wait_while_stop_the_world() {{}}
           pub(crate) fn wait_while_epoch_is(_expected: u64) {{}}
           pub(crate) fn dump_stop_the_world_timeout(_stop_epoch: u64, _timeout: std::time::Duration) {{}}
           pub(crate) fn for_each_root_slot_world_stopped(
             _stop_epoch: u64,
             _f: impl FnMut(*mut *mut u8),
           ) -> Result<(), crate::WalkError> {{
             Ok(())
          }}
        }}
      }}

      #[path = "{safepoint_rs}"]
      pub mod safepoint;
      "#
    ),
  )
  .expect("write shim");

  let status = Command::new("rustc")
    .arg("--crate-name=runtime_native_safepoint_shim")
    .arg("--crate-type=lib")
    .arg("--edition=2021")
    .arg("--target=aarch64-unknown-linux-gnu")
    .arg("-Copt-level=0")
    .arg("--emit=obj")
    .arg("-o")
    .arg(&obj_path)
    .arg(&shim_rs)
    .status()
    .expect("run rustc");
  assert!(status.success(), "rustc failed for aarch64");

  let output = Command::new("llvm-objdump")
    .arg("-d")
    .arg("--disassemble-symbols=rt_gc_safepoint")
    .arg(&obj_path)
    .output()
    .expect("run llvm-objdump");
  assert!(
    output.status.success(),
    "llvm-objdump failed: {}",
    String::from_utf8_lossy(&output.stderr)
  );

  let disasm = String::from_utf8_lossy(&output.stdout);
  assert!(
    disasm.contains("mov\tx0, x29") || disasm.contains("mov x0, x29"),
    "expected `mov x0, x29` in rt_gc_safepoint stub, got:\n{disasm}"
  );
  assert!(
    disasm.contains("mov\tx1, x30") || disasm.contains("mov x1, x30"),
    "expected `mov x1, x30` in rt_gc_safepoint stub, got:\n{disasm}"
  );
}
