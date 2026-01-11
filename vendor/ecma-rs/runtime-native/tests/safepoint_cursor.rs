#[cfg(target_arch = "x86_64")]
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
  use std::process::Command;

  fn cmd_exists(cmd: &str) -> bool {
    Command::new(cmd)
      .arg("--version")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
  }

  fn find_on_path(candidates: &[&'static str]) -> Option<&'static str> {
    for &cand in candidates {
      if cmd_exists(cand) {
        return Some(cand);
      }
    }
    None
  }

  let tempdir = tempfile::tempdir().expect("tempdir");
  let shim_rs = tempdir.path().join("safepoint_shim.rs");
  let obj_path = tempdir.path().join("safepoint_shim.o");

  let safepoint_asm = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("src")
    .join("arch")
    .join("aarch64")
    .join("rt_gc_safepoint.S");
  let safepoint_asm = safepoint_asm.to_string_lossy();

  // Compile the real AArch64 `rt_gc_safepoint` assembly stub in isolation. This
  // is a cheap disassembly sanity check (it does not link the full runtime).
  std::fs::write(
    &shim_rs,
    format!(
      r#"
      use core::arch::global_asm;

      global_asm!(include_str!({safepoint_asm:?}));
      "#
    ),
  )
  .expect("write shim");

  let rustc_out = Command::new("rustc")
    .arg("--crate-name=runtime_native_safepoint_shim")
    .arg("--crate-type=lib")
    .arg("--edition=2021")
    .arg("--target=aarch64-unknown-linux-gnu")
    .arg("-Copt-level=0")
    .arg("--emit=obj")
    .arg("-o")
    .arg(&obj_path)
    .arg(&shim_rs)
    .output()
    .expect("run rustc");
  if !rustc_out.status.success() {
    let stderr = String::from_utf8_lossy(&rustc_out.stderr);
    // Toolchain setup varies: allow this test to be skipped when the AArch64
    // standard library isn't installed (common on x86_64 hosts without the
    // cross target added via rustup).
    if stderr.contains("target may not be installed") || stderr.contains("can't find crate for `std`") {
      eprintln!("skipping: rustc could not build for aarch64-unknown-linux-gnu (missing target std?):\n{stderr}");
      return;
    }
    panic!(
      "rustc failed for aarch64 (status={})\nstdout:\n{}\nstderr:\n{}",
      rustc_out.status,
      String::from_utf8_lossy(&rustc_out.stdout),
      stderr
    );
  }

  let Some(objdump) = find_on_path(&["llvm-objdump-18", "llvm-objdump"]) else {
    eprintln!("skipping: llvm-objdump not found in PATH (need llvm-objdump-18/llvm-objdump)");
    return;
  };

  let output = Command::new(objdump)
    .arg("-d")
    .arg("--disassemble-symbols=rt_gc_safepoint")
    .arg(&obj_path)
    .output()
    .unwrap_or_else(|e| panic!("failed to run {objdump}: {e}"));
  assert!(
    output.status.success(),
    "{objdump} failed: {}",
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
