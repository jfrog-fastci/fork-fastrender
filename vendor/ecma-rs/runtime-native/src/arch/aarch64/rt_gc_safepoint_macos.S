.text

// AArch64 safepoint stub for stop-the-world GC (Mach-O variant).
//
// This is identical to `rt_gc_safepoint.S` but uses Mach-O GOT relocation syntax.
//
// Fast path:
//   - Load `RT_GC_EPOCH` and test the low bit.
//   - If not requested, return immediately.
//
// Slow path:
//   - Spill GP registers into a `RegContext` on the stack.
//   - Call into Rust (`rt_gc_safepoint_impl`) to publish the safepoint context
//     and block until the world is resumed.
//   - Restore registers from the `RegContext` and return to the caller.
//
// This must be written in assembly so we can capture the caller's FP/LR/SP
// before a Rust prologue can clobber them.

// Actual implementation is `runtime_native_gc_safepoint_asm`; the public ABI symbol
// `rt_gc_safepoint` is a tiny Rust wrapper that tail-branches here so the symbol is exported from
// `cdylib` builds.
.globl runtime_native_gc_safepoint_asm
runtime_native_gc_safepoint_asm:
  // Fast path: if (RT_GC_EPOCH & 1) == 0 return;
  // Load through the GOT so this remains PIC-friendly when runtime-native is
  // built as a `cdylib`.
  adrp x16, RT_GC_EPOCH@GOTPAGE
  ldr x16, [x16, RT_GC_EPOCH@GOTPAGEOFF]
  ldar x17, [x16]
  tbz x17, #0, .Lrt_gc_safepoint_ret

  // Slow path.
  //
  // Capture the original SP (caller's SP at the safepoint callsite).
  mov x16, sp

  // Allocate space for RegContext on the stack. We use 272 bytes to keep SP
  // 16-byte aligned while storing:
  // - 31 GPRs (x0..x30) = 248 bytes
  // - sp = 8 bytes
  // - pc = 8 bytes
  // Total struct size = 264 bytes, plus 8 bytes padding for alignment.
  sub sp, sp, #272

  // Spill x0..x29 in pairs.
  stp x0, x1, [sp, #0]
  stp x2, x3, [sp, #16]
  stp x4, x5, [sp, #32]
  stp x6, x7, [sp, #48]
  stp x8, x9, [sp, #64]
  stp x10, x11, [sp, #80]
  stp x12, x13, [sp, #96]
  stp x14, x15, [sp, #112]
  stp x16, x17, [sp, #128]
  stp x18, x19, [sp, #144]
  stp x20, x21, [sp, #160]
  stp x22, x23, [sp, #176]
  stp x24, x25, [sp, #192]
  stp x26, x27, [sp, #208]
  stp x28, x29, [sp, #224]

  // Spill x30 (LR).
  str x30, [sp, #240]

  // Spill SP and PC at the callsite.
  // - SP (caller's stack pointer at callsite) is in x16 from above.
  // - PC is represented by the return address in LR (x30).
  str x16, [sp, #248] // RegContext.sp
  str x30, [sp, #256] // RegContext.pc

  // Call Rust: rt_gc_safepoint_impl(caller_fp, caller_pc, regs_ptr)
  mov x0, x29 // caller FP at entry (requires frame pointers enabled)
  mov x1, x30 // caller PC / return address at entry
  mov x2, sp  // regs_ptr = &RegContext
  bl rt_gc_safepoint_impl

  // Restore registers from RegContext and return.
  ldp x0, x1, [sp, #0]
  ldp x2, x3, [sp, #16]
  ldp x4, x5, [sp, #32]
  ldp x6, x7, [sp, #48]
  ldp x8, x9, [sp, #64]
  ldp x10, x11, [sp, #80]
  ldp x12, x13, [sp, #96]
  ldp x14, x15, [sp, #112]
  ldp x16, x17, [sp, #128]
  ldp x18, x19, [sp, #144]
  ldp x20, x21, [sp, #160]
  ldp x22, x23, [sp, #176]
  ldp x24, x25, [sp, #192]
  ldp x26, x27, [sp, #208]
  ldp x28, x29, [sp, #224]
  ldr x30, [sp, #240]

  add sp, sp, #272
  ret

.Lrt_gc_safepoint_ret:
  ret
