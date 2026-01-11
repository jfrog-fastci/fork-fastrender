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
  str xzr, [x0, #32]  // regs (no reg context outside safepoint slow path)
  ret

  // Legacy slow-path entrypoint used by some tests and runtime-internal polls.
  .globl runtime_native_gc_safepoint_slow_asm
runtime_native_gc_safepoint_slow_asm:
  // epoch: x0
  // Stack frame layout (304 bytes total, maintains 16-byte SP alignment):
  //
  //   [sp + 0 .. 264)   = stackmap_context::ThreadContext (RegContext)
  //   [sp + 264 .. 304) = SafepointContext (5 * u64)
  //
  // Note: the saved RegContext uses *stackmap semantics* for SP/IP:
  // - sp = callsite SP (same as callee-entry SP on AArch64)
  // - pc = return address (captured from x30)
  sub sp, sp, #304

  // Save X0..X30 into RegContext.x[0..30].
  str x0, [sp, #0]
  str x1, [sp, #8]
  str x2, [sp, #16]
  str x3, [sp, #24]
  str x4, [sp, #32]
  str x5, [sp, #40]
  str x6, [sp, #48]
  str x7, [sp, #56]
  str x8, [sp, #64]
  str x9, [sp, #72]
  str x10, [sp, #80]
  str x11, [sp, #88]
  str x12, [sp, #96]
  str x13, [sp, #104]
  str x14, [sp, #112]
  str x15, [sp, #120]
  str x16, [sp, #128]
  str x17, [sp, #136]
  str x18, [sp, #144]
  str x19, [sp, #152]
  str x20, [sp, #160]
  str x21, [sp, #168]
  str x22, [sp, #176]
  str x23, [sp, #184]
  str x24, [sp, #192]
  str x25, [sp, #200]
  str x26, [sp, #208]
  str x27, [sp, #216]
  str x28, [sp, #224]
  str x29, [sp, #232]
  str x30, [sp, #240]

  // Compute original/callsite SP (sp_entry) and return address (x30).
  add x2, sp, #304      // sp_entry (original sp)
  mov x3, x2            // sp (stackmap SP)
  mov x4, x29           // fp
  mov x5, x30           // ip (return address)

  // Fill RegContext.sp and RegContext.pc.
  str x3, [sp, #248]    // sp
  str x5, [sp, #256]    // pc

  // Fill SafepointContext at [sp + 264].
  str x2, [sp, #264]    // sp_entry
  str x3, [sp, #272]    // sp
  str x4, [sp, #280]    // fp
  str x5, [sp, #288]    // ip
  add x6, sp, #0
  str x6, [sp, #296]    // regs = &RegContext

  add x1, sp, #264
  bl runtime_native_gc_safepoint_slow_impl

  // Restore registers from RegContext (potentially rewritten by the GC).
  ldr x0, [sp, #0]
  ldr x1, [sp, #8]
  ldr x2, [sp, #16]
  ldr x3, [sp, #24]
  ldr x4, [sp, #32]
  ldr x5, [sp, #40]
  ldr x6, [sp, #48]
  ldr x7, [sp, #56]
  ldr x8, [sp, #64]
  ldr x9, [sp, #72]
  ldr x10, [sp, #80]
  ldr x11, [sp, #88]
  ldr x12, [sp, #96]
  ldr x13, [sp, #104]
  ldr x14, [sp, #112]
  ldr x15, [sp, #120]
  ldr x16, [sp, #128]
  ldr x17, [sp, #136]
  ldr x18, [sp, #144]
  ldr x19, [sp, #152]
  ldr x20, [sp, #160]
  ldr x21, [sp, #168]
  ldr x22, [sp, #176]
  ldr x23, [sp, #184]
  ldr x24, [sp, #192]
  ldr x25, [sp, #200]
  ldr x26, [sp, #208]
  ldr x27, [sp, #216]
  ldr x28, [sp, #224]
  ldr x29, [sp, #232]
  ldr x30, [sp, #240]

  add sp, sp, #304
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

  // Stack frame layout (304 bytes total, maintains 16-byte SP alignment):
  //
  //   [sp + 0 .. 264)   = stackmap_context::ThreadContext (RegContext)
  //   [sp + 264 .. 304) = SafepointContext (5 * u64)
  //
  // Note: the saved RegContext uses *stackmap semantics* for SP/IP:
  // - sp = callsite SP (same as callee-entry SP on AArch64)
  // - pc = return address (captured from x30)
  sub sp, sp, #304

  // Save X0..X30 into RegContext.x[0..30].
  str x0, [sp, #0]
  str x1, [sp, #8]
  str x2, [sp, #16]
  str x3, [sp, #24]
  str x4, [sp, #32]
  str x5, [sp, #40]
  str x6, [sp, #48]
  str x7, [sp, #56]
  str x8, [sp, #64]
  str x9, [sp, #72]
  str x10, [sp, #80]
  str x11, [sp, #88]
  str x12, [sp, #96]
  str x13, [sp, #104]
  str x14, [sp, #112]
  str x15, [sp, #120]
  str x16, [sp, #128]
  str x17, [sp, #136]
  str x18, [sp, #144]
  str x19, [sp, #152]
  str x20, [sp, #160]
  str x21, [sp, #168]
  str x22, [sp, #176]
  str x23, [sp, #184]
  str x24, [sp, #192]
  str x25, [sp, #200]
  str x26, [sp, #208]
  str x27, [sp, #216]
  str x28, [sp, #224]
  str x29, [sp, #232]
  str x30, [sp, #240]

  // Compute original/callsite SP (sp_entry) and return address (x30).
  add x2, sp, #304      // sp_entry (original sp)
  mov x3, x2            // sp (stackmap SP)
  mov x4, x29           // fp
  mov x5, x30           // ip (return address)

  // Fill RegContext.sp and RegContext.pc.
  str x3, [sp, #248]    // sp
  str x5, [sp, #256]    // pc

  // Fill SafepointContext at [sp + 264].
  str x2, [sp, #264]    // sp_entry
  str x3, [sp, #272]    // sp
  str x4, [sp, #280]    // fp
  str x5, [sp, #288]    // ip
  add x6, sp, #0
  str x6, [sp, #296]    // regs = &RegContext

  add x1, sp, #264
  bl runtime_native_gc_safepoint_slow_impl

  // Restore registers from RegContext (potentially rewritten by the GC).
  ldr x0, [sp, #0]
  ldr x1, [sp, #8]
  ldr x2, [sp, #16]
  ldr x3, [sp, #24]
  ldr x4, [sp, #32]
  ldr x5, [sp, #40]
  ldr x6, [sp, #48]
  ldr x7, [sp, #56]
  ldr x8, [sp, #64]
  ldr x9, [sp, #72]
  ldr x10, [sp, #80]
  ldr x11, [sp, #88]
  ldr x12, [sp, #96]
  ldr x13, [sp, #104]
  ldr x14, [sp, #112]
  ldr x15, [sp, #120]
  ldr x16, [sp, #128]
  ldr x17, [sp, #136]
  ldr x18, [sp, #144]
  ldr x19, [sp, #152]
  ldr x20, [sp, #160]
  ldr x21, [sp, #168]
  ldr x22, [sp, #176]
  ldr x23, [sp, #184]
  ldr x24, [sp, #192]
  ldr x25, [sp, #200]
  ldr x26, [sp, #208]
  ldr x27, [sp, #216]
  ldr x28, [sp, #224]
  ldr x29, [sp, #232]
  ldr x30, [sp, #240]

  add sp, sp, #304
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
  str xzr, [x0, #32]  // regs (no reg context outside safepoint slow path)
  ret

  // Legacy slow-path entrypoint used by some tests and runtime-internal polls.
  .globl runtime_native_gc_safepoint_slow_asm
runtime_native_gc_safepoint_slow_asm:
  // epoch: x0
  // Stack frame layout (304 bytes total, maintains 16-byte SP alignment):
  //
  //   [sp + 0 .. 264)   = stackmap_context::ThreadContext (RegContext)
  //   [sp + 264 .. 304) = SafepointContext (5 * u64)
  //
  // Note: the saved RegContext uses *stackmap semantics* for SP/IP:
  // - sp = callsite SP (same as callee-entry SP on AArch64)
  // - pc = return address (captured from x30)
  sub sp, sp, #304

  // Save X0..X30 into RegContext.x[0..30].
  str x0, [sp, #0]
  str x1, [sp, #8]
  str x2, [sp, #16]
  str x3, [sp, #24]
  str x4, [sp, #32]
  str x5, [sp, #40]
  str x6, [sp, #48]
  str x7, [sp, #56]
  str x8, [sp, #64]
  str x9, [sp, #72]
  str x10, [sp, #80]
  str x11, [sp, #88]
  str x12, [sp, #96]
  str x13, [sp, #104]
  str x14, [sp, #112]
  str x15, [sp, #120]
  str x16, [sp, #128]
  str x17, [sp, #136]
  str x18, [sp, #144]
  str x19, [sp, #152]
  str x20, [sp, #160]
  str x21, [sp, #168]
  str x22, [sp, #176]
  str x23, [sp, #184]
  str x24, [sp, #192]
  str x25, [sp, #200]
  str x26, [sp, #208]
  str x27, [sp, #216]
  str x28, [sp, #224]
  str x29, [sp, #232]
  str x30, [sp, #240]

  // Compute original/callsite SP (sp_entry) and return address (x30).
  add x2, sp, #304      // sp_entry (original sp)
  mov x3, x2            // sp (stackmap SP)
  mov x4, x29           // fp
  mov x5, x30           // ip (return address)

  // Fill RegContext.sp and RegContext.pc.
  str x3, [sp, #248]    // sp
  str x5, [sp, #256]    // pc

  // Fill SafepointContext at [sp + 264].
  str x2, [sp, #264]    // sp_entry
  str x3, [sp, #272]    // sp
  str x4, [sp, #280]    // fp
  str x5, [sp, #288]    // ip
  add x6, sp, #0
  str x6, [sp, #296]    // regs = &RegContext

  add x1, sp, #264
  bl runtime_native_gc_safepoint_slow_impl

  // Restore registers from RegContext (potentially rewritten by the GC).
  ldr x0, [sp, #0]
  ldr x1, [sp, #8]
  ldr x2, [sp, #16]
  ldr x3, [sp, #24]
  ldr x4, [sp, #32]
  ldr x5, [sp, #40]
  ldr x6, [sp, #48]
  ldr x7, [sp, #56]
  ldr x8, [sp, #64]
  ldr x9, [sp, #72]
  ldr x10, [sp, #80]
  ldr x11, [sp, #88]
  ldr x12, [sp, #96]
  ldr x13, [sp, #104]
  ldr x14, [sp, #112]
  ldr x15, [sp, #120]
  ldr x16, [sp, #128]
  ldr x17, [sp, #136]
  ldr x18, [sp, #144]
  ldr x19, [sp, #152]
  ldr x20, [sp, #160]
  ldr x21, [sp, #168]
  ldr x22, [sp, #176]
  ldr x23, [sp, #184]
  ldr x24, [sp, #192]
  ldr x25, [sp, #200]
  ldr x26, [sp, #208]
  ldr x27, [sp, #216]
  ldr x28, [sp, #224]
  ldr x29, [sp, #232]
  ldr x30, [sp, #240]

  add sp, sp, #304
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

  // Stack frame layout (304 bytes total, maintains 16-byte SP alignment):
  //
  //   [sp + 0 .. 264)   = stackmap_context::ThreadContext (RegContext)
  //   [sp + 264 .. 304) = SafepointContext (5 * u64)
  //
  // Note: the saved RegContext uses *stackmap semantics* for SP/IP:
  // - sp = callsite SP (same as callee-entry SP on AArch64)
  // - pc = return address (captured from x30)
  sub sp, sp, #304

  // Save X0..X30 into RegContext.x[0..30].
  str x0, [sp, #0]
  str x1, [sp, #8]
  str x2, [sp, #16]
  str x3, [sp, #24]
  str x4, [sp, #32]
  str x5, [sp, #40]
  str x6, [sp, #48]
  str x7, [sp, #56]
  str x8, [sp, #64]
  str x9, [sp, #72]
  str x10, [sp, #80]
  str x11, [sp, #88]
  str x12, [sp, #96]
  str x13, [sp, #104]
  str x14, [sp, #112]
  str x15, [sp, #120]
  str x16, [sp, #128]
  str x17, [sp, #136]
  str x18, [sp, #144]
  str x19, [sp, #152]
  str x20, [sp, #160]
  str x21, [sp, #168]
  str x22, [sp, #176]
  str x23, [sp, #184]
  str x24, [sp, #192]
  str x25, [sp, #200]
  str x26, [sp, #208]
  str x27, [sp, #216]
  str x28, [sp, #224]
  str x29, [sp, #232]
  str x30, [sp, #240]

  // Compute original/callsite SP (sp_entry) and return address (x30).
  add x2, sp, #304      // sp_entry (original sp)
  mov x3, x2            // sp (stackmap SP)
  mov x4, x29           // fp
  mov x5, x30           // ip (return address)

  // Fill RegContext.sp and RegContext.pc.
  str x3, [sp, #248]    // sp
  str x5, [sp, #256]    // pc

  // Fill SafepointContext at [sp + 264].
  str x2, [sp, #264]    // sp_entry
  str x3, [sp, #272]    // sp
  str x4, [sp, #280]    // fp
  str x5, [sp, #288]    // ip
  add x6, sp, #0
  str x6, [sp, #296]    // regs = &RegContext

  add x1, sp, #264
  bl runtime_native_gc_safepoint_slow_impl

  // Restore registers from RegContext (potentially rewritten by the GC).
  ldr x0, [sp, #0]
  ldr x1, [sp, #8]
  ldr x2, [sp, #16]
  ldr x3, [sp, #24]
  ldr x4, [sp, #32]
  ldr x5, [sp, #40]
  ldr x6, [sp, #48]
  ldr x7, [sp, #56]
  ldr x8, [sp, #64]
  ldr x9, [sp, #72]
  ldr x10, [sp, #80]
  ldr x11, [sp, #88]
  ldr x12, [sp, #96]
  ldr x13, [sp, #104]
  ldr x14, [sp, #112]
  ldr x15, [sp, #120]
  ldr x16, [sp, #128]
  ldr x17, [sp, #136]
  ldr x18, [sp, #144]
  ldr x19, [sp, #152]
  ldr x20, [sp, #160]
  ldr x21, [sp, #168]
  ldr x22, [sp, #176]
  ldr x23, [sp, #184]
  ldr x24, [sp, #192]
  ldr x25, [sp, #200]
  ldr x26, [sp, #208]
  ldr x27, [sp, #216]
  ldr x28, [sp, #224]
  ldr x29, [sp, #232]
  ldr x30, [sp, #240]

  add sp, sp, #304
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
