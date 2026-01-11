use core::arch::global_asm;

// Assembly is written in Intel syntax for readability.
//
// `rt_capture_safepoint_context(out)`:
//   Captures callee-entry RSP/RBP and the return address at [RSP] into `out`.
//
// `rt_gc_safepoint_slow(epoch)`:
//   Assembly shim used by the safepoint slow path to capture the caller context
//   *before* a Rust prologue can clobber SP/FP/RA. It then calls into
//   `rt_gc_safepoint_slow_impl(epoch, ctx_ptr)`.
global_asm!(
  r#"
  .text
  .globl rt_capture_safepoint_context
rt_capture_safepoint_context:
  // out: rdi
  mov rax, rsp                // sp_entry
  mov rcx, qword ptr [rsp]    // return address (ip)
  lea rdx, [rax + 8]          // sp (post-call; stackmap SP)

  mov qword ptr [rdi + 0], rax
  mov qword ptr [rdi + 8], rdx
  mov qword ptr [rdi + 16], rbp
  mov qword ptr [rdi + 24], rcx
  ret
  .globl rt_gc_safepoint_slow
rt_gc_safepoint_slow:
  // epoch: rdi
  // Capture the entry stack pointer and return address before touching RSP.
  mov rax, rsp                // sp_entry
  mov rcx, qword ptr [rsp]    // ip (return address)
  lea r8, [rax + 8]           // sp (post-call; stackmap SP)

  // Reserve space for SafepointContext (32 bytes) and align stack to 16 bytes
  // before calling into Rust.
  sub rsp, 40

  mov qword ptr [rsp + 0], rax
  mov qword ptr [rsp + 8], r8
  mov qword ptr [rsp + 16], rbp
  mov qword ptr [rsp + 24], rcx

  lea rsi, [rsp]              // arg1: &ctx (arg0 already in rdi)
  call rt_gc_safepoint_slow_impl

  add rsp, 40
  ret

  "#
);
