pub mod regs;
pub use regs::RegContext;

#[cfg(target_arch = "aarch64")]
use core::arch::global_asm;

// `rt_gc_safepoint` assembly stub.
//
// Only compile this on AArch64 so x86_64 CI doesn't attempt to assemble it.
//
// NOTE: The AArch64 GOT relocation syntax differs between ELF and Mach-O.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
global_asm!(include_str!("rt_gc_safepoint_macos.S"));
#[cfg(all(target_arch = "aarch64", not(target_os = "macos")))]
global_asm!(include_str!("rt_gc_safepoint.S"));

// Minimal context capture used by runtime code paths that need the callee-entry
// SP/FP/LR values (see `arch::capture_safepoint_context`).
#[cfg(target_arch = "aarch64")]
#[cfg(target_os = "macos")]
global_asm!(
  r#"
  .text

  .globl runtime_native_capture_safepoint_context
runtime_native_capture_safepoint_context:
  // out: x0
  mov x1, sp          // sp_entry
  mov x2, x1          // sp (stackmap SP)
  // Walk frame pointers to capture the *outer* caller frame:
  // - X29 is the Rust wrapper's frame pointer.
  // - [X29 + 0] is the runtime helper's frame pointer.
  // - [runtime_fp + 0] is the outer caller's frame pointer.
  // - [runtime_fp + 8] is the saved LR (return address) into the outer caller
  //   after calling the runtime helper.
  ldr x3, [x29, #0]    // runtime_fp
  ldr x4, [x3, #0]     // outer_fp
  ldr x5, [x3, #8]     // outer_ip

  str x1, [x0, #0]
  str x2, [x0, #8]
  str x4, [x0, #16]
  str x5, [x0, #24]
  ret

  // Legacy slow-path entrypoint used by some tests and runtime-internal polls.
  .globl runtime_native_gc_safepoint_slow_asm
runtime_native_gc_safepoint_slow_asm:
  // epoch: x0
  // Capture SP/FP/LR before touching the stack.
  mov x2, sp          // sp_entry
  mov x3, x2          // sp (stackmap SP)
  mov x4, x29         // fp
  mov x5, x30         // original return address (ip)

  // Allocate SafepointContext (32 bytes). Keep 16-byte stack alignment.
  sub sp, sp, #32
  str x2, [sp, #0]
  str x3, [sp, #8]
  str x4, [sp, #16]
  str x5, [sp, #24]

  mov x1, sp
  bl runtime_native_gc_safepoint_slow_impl

  // Restore the original link register so `ret` returns to the caller.
  ldr x30, [sp, #24]
  add sp, sp, #32
  ret

  // LLVM `place-safepoints` poll hook.
  //
  // Signature: `void gc.safepoint_poll(void)`.
  .globl runtime_native_gc_safepoint_poll_asm
runtime_native_gc_safepoint_poll_asm:
  // epoch = RT_GC_EPOCH (Acquire)
  // Use GOT-relative addressing so this assembly is PIC-friendly (runtime-native
  // is built as a `cdylib` for some tests/tools).
  adrp x9, RT_GC_EPOCH@GOTPAGE
  ldr x9, [x9, RT_GC_EPOCH@GOTPAGEOFF]
  ldar x0, [x9]
  tbz x0, #0, .Lgc_safepoint_poll_ret

  // Capture SP/FP/LR before touching the stack.
  mov x2, sp          // sp_entry
  mov x3, x2          // sp (stackmap SP)
  mov x4, x29         // fp
  mov x5, x30         // original return address (ip)

  // Allocate SafepointContext (32 bytes). Keep 16-byte stack alignment.
  sub sp, sp, #32
  str x2, [sp, #0]
  str x3, [sp, #8]
  str x4, [sp, #16]
  str x5, [sp, #24]

  mov x1, sp
  bl runtime_native_gc_safepoint_slow_impl

  // Restore the original link register so `ret` returns to the caller.
  ldr x30, [sp, #24]
  add sp, sp, #32
.Lgc_safepoint_poll_ret:
  ret
  "#
);

#[cfg(target_arch = "aarch64")]
#[cfg(not(target_os = "macos"))]
global_asm!(
  r#"
  .text

  .globl runtime_native_capture_safepoint_context
runtime_native_capture_safepoint_context:
  // out: x0
  mov x1, sp          // sp_entry
  mov x2, x1          // sp (stackmap SP)
  // Walk frame pointers to capture the *outer* caller frame:
  // - X29 is the Rust wrapper's frame pointer.
  // - [X29 + 0] is the runtime helper's frame pointer.
  // - [runtime_fp + 0] is the outer caller's frame pointer.
  // - [runtime_fp + 8] is the saved LR (return address) into the outer caller
  //   after calling the runtime helper.
  ldr x3, [x29, #0]    // runtime_fp
  ldr x4, [x3, #0]     // outer_fp
  ldr x5, [x3, #8]     // outer_ip

  str x1, [x0, #0]
  str x2, [x0, #8]
  str x4, [x0, #16]
  str x5, [x0, #24]
  ret

  // Legacy slow-path entrypoint used by some tests and runtime-internal polls.
  .globl runtime_native_gc_safepoint_slow_asm
runtime_native_gc_safepoint_slow_asm:
  // epoch: x0
  // Capture SP/FP/LR before touching the stack.
  mov x2, sp          // sp_entry
  mov x3, x2          // sp (stackmap SP)
  mov x4, x29         // fp
  mov x5, x30         // original return address (ip)

  // Allocate SafepointContext (32 bytes). Keep 16-byte stack alignment.
  sub sp, sp, #32
  str x2, [sp, #0]
  str x3, [sp, #8]
  str x4, [sp, #16]
  str x5, [sp, #24]

  mov x1, sp
  bl runtime_native_gc_safepoint_slow_impl

  // Restore the original link register so `ret` returns to the caller.
  ldr x30, [sp, #24]
  add sp, sp, #32
  ret

  // LLVM `place-safepoints` poll hook.
  //
  // Signature: `void gc.safepoint_poll(void)`.
  .globl runtime_native_gc_safepoint_poll_asm
runtime_native_gc_safepoint_poll_asm:
  // epoch = RT_GC_EPOCH (Acquire)
  // Use GOT-relative addressing so this assembly is PIC-friendly (runtime-native
  // is built as a `cdylib` for some tests/tools).
  adrp x9, :got:RT_GC_EPOCH
  ldr x9, [x9, :got_lo12:RT_GC_EPOCH]
  ldar x0, [x9]
  tbz x0, #0, .Lgc_safepoint_poll_ret

  // Capture SP/FP/LR before touching the stack.
  mov x2, sp          // sp_entry
  mov x3, x2          // sp (stackmap SP)
  mov x4, x29         // fp
  mov x5, x30         // original return address (ip)

  // Allocate SafepointContext (32 bytes). Keep 16-byte stack alignment.
  sub sp, sp, #32
  str x2, [sp, #0]
  str x3, [sp, #8]
  str x4, [sp, #16]
  str x5, [sp, #24]

  mov x1, sp
  bl runtime_native_gc_safepoint_slow_impl

  // Restore the original link register so `ret` returns to the caller.
  ldr x30, [sp, #24]
  add sp, sp, #32
.Lgc_safepoint_poll_ret:
  ret
  "#
);

// Exported wrappers for cdylib builds: see `arch/x86_64.rs` for rationale.

#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn rt_gc_safepoint_slow(_epoch: u64) {
  core::arch::naked_asm!("b runtime_native_gc_safepoint_slow_asm");
}

#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
#[export_name = "gc.safepoint_poll"]
pub unsafe extern "C" fn gc_safepoint_poll() {
  core::arch::naked_asm!("b runtime_native_gc_safepoint_poll_asm");
}
