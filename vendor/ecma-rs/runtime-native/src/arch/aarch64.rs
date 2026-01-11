use core::arch::global_asm;

global_asm!(
  r#"
  .text

  .globl rt_capture_safepoint_context
rt_capture_safepoint_context:
  // out: x0
  mov x1, sp          // sp_entry
  mov x2, x1          // sp (post-call; stackmap SP)
  mov x3, x29         // fp
  mov x4, x30         // ip (return address)

  str x1, [x0, #0]
  str x2, [x0, #8]
  str x3, [x0, #16]
  str x4, [x0, #24]
  ret

  .globl rt_gc_safepoint_slow
rt_gc_safepoint_slow:
  // epoch: x0
  // Capture SP/FP/LR before touching the stack.
  mov x2, sp          // sp_entry
  mov x3, x2          // sp (post-call; stackmap SP)
  mov x4, x29         // fp
  mov x5, x30         // original return address (ip)

  // Allocate SafepointContext (32 bytes). Keep 16-byte stack alignment.
  sub sp, sp, #32
  str x2, [sp, #0]
  str x3, [sp, #8]
  str x4, [sp, #16]
  str x5, [sp, #24]

  mov x1, sp
  bl rt_gc_safepoint_slow_impl

  // Restore the original link register so `ret` returns to the caller.
  ldr x30, [sp, #24]
  add sp, sp, #32
  ret
  "#
);
