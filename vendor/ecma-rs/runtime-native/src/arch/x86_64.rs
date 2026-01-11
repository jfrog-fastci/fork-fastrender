use core::arch::global_asm;

pub mod regs;

// Assembly is written in Intel syntax for readability.
//
// `rt_capture_safepoint_context(out)`:
//   Captures a stable caller frame for `arch::capture_safepoint_context`:
//   walks the frame-pointer chain to skip the Rust wrapper + runtime helper and
//   records the outer caller's FP + return address.
//
// `rt_gc_safepoint_slow(epoch)`:
//   Assembly shim used by the safepoint slow path to capture the caller context
//   *before* a Rust prologue can clobber SP/FP/RA. It then calls into
//   `runtime_native_gc_safepoint_slow_impl(epoch, ctx_ptr)`.
global_asm!(
  r#"
  .text
  .globl runtime_native_capture_safepoint_context
runtime_native_capture_safepoint_context:
  // out: rdi
  mov rax, rsp                // sp_entry
  lea rdx, [rax + 8]          // sp (post-call; stackmap SP)

  // Walk frame pointers to capture the *outer* caller frame:
  // - RBP is the Rust wrapper's frame pointer.
  // - [RBP + 0] is the runtime helper's frame pointer.
  // - [runtime_fp + 0] is the outer caller's frame pointer.
  // - [runtime_fp + 8] is the return address into the outer caller after calling
  //   the runtime helper.
  mov rcx, qword ptr [rbp]    // runtime_fp
  mov r8, qword ptr [rcx]     // outer_fp
  mov r9, qword ptr [rcx + 8] // outer_ip

  mov qword ptr [rdi + 0], rax
  mov qword ptr [rdi + 8], rdx
  mov qword ptr [rdi + 16], r8
  mov qword ptr [rdi + 24], r9
  mov qword ptr [rdi + 32], 0 // regs (no reg context outside safepoint slow path)
  ret
  .globl runtime_native_gc_safepoint_slow_asm
runtime_native_gc_safepoint_slow_asm:
  // epoch: rdi
  // Stack frame layout (184 bytes total, keeps 16-byte alignment before the call):
  //
  //   [rsp + 0 .. 136)   = stackmap_context::ThreadContext (RegContext)
  //   [rsp + 136 .. 176) = SafepointContext (5 * u64)
  //   [rsp + 176 .. 184) = padding
  //
  // Note: the saved RegContext uses *stackmap semantics* for SP/IP:
  // - rsp = post-call SP (sp_entry + 8)
  // - rip = return address loaded from [sp_entry]
  sub rsp, 184

  // Save GPRs into RegContext (offsets match stackmap-context x86_64 ThreadContext).
  mov qword ptr [rsp + 0], rax
  mov qword ptr [rsp + 8], rdx
  mov qword ptr [rsp + 16], rcx
  mov qword ptr [rsp + 24], rbx
  mov qword ptr [rsp + 32], rsi
  mov qword ptr [rsp + 40], rdi
  mov qword ptr [rsp + 48], rbp
  // rsp (DWARF reg 7) and rip (DWARF reg 16) are filled below with stackmap semantics.
  mov qword ptr [rsp + 64], r8
  mov qword ptr [rsp + 72], r9
  mov qword ptr [rsp + 80], r10
  mov qword ptr [rsp + 88], r11
  mov qword ptr [rsp + 96], r12
  mov qword ptr [rsp + 104], r13
  mov qword ptr [rsp + 112], r14
  mov qword ptr [rsp + 120], r15

  // Compute callee-entry sp and callsite return address.
  lea rax, [rsp + 184]        // sp_entry (original rsp)
  mov rcx, qword ptr [rax]    // ip (return address into caller)
  lea rdx, [rax + 8]          // sp (post-call; stackmap SP)

  // Fill in stackmap-semantics rsp/rip in RegContext.
  mov qword ptr [rsp + 56], rdx  // rsp
  mov qword ptr [rsp + 128], rcx // rip

  // Fill SafepointContext at [rsp + 136].
  mov qword ptr [rsp + 136 + 0], rax  // sp_entry
  mov qword ptr [rsp + 136 + 8], rdx  // sp
  mov qword ptr [rsp + 136 + 16], rbp // fp
  mov qword ptr [rsp + 136 + 24], rcx // ip
  lea r8, [rsp + 0]
  mov qword ptr [rsp + 136 + 32], r8  // regs = &RegContext

  lea rsi, [rsp + 136]         // arg1: &SafepointContext (arg0 already in rdi)
  call runtime_native_gc_safepoint_slow_impl

  // Restore registers from RegContext (potentially rewritten by the GC).
  mov rax, qword ptr [rsp + 0]
  mov rdx, qword ptr [rsp + 8]
  mov rcx, qword ptr [rsp + 16]
  mov rbx, qword ptr [rsp + 24]
  mov rsi, qword ptr [rsp + 32]
  mov rdi, qword ptr [rsp + 40]
  mov rbp, qword ptr [rsp + 48]
  mov r8, qword ptr [rsp + 64]
  mov r9, qword ptr [rsp + 72]
  mov r10, qword ptr [rsp + 80]
  mov r11, qword ptr [rsp + 88]
  mov r12, qword ptr [rsp + 96]
  mov r13, qword ptr [rsp + 104]
  mov r14, qword ptr [rsp + 112]
  mov r15, qword ptr [rsp + 120]

  add rsp, 184
  ret

  // LLVM `place-safepoints` polls this symbol. It must capture the *managed*
  // caller's context at the poll callsite (the statepoint call), not the
  // runtime-internal callsite to `rt_gc_safepoint_slow`.
  //
  // Signature: `void gc.safepoint_poll(void)`.
  .globl runtime_native_gc_safepoint_poll_asm
runtime_native_gc_safepoint_poll_asm:
  // epoch = RT_GC_EPOCH (Acquire)
  // Use GOT-relative addressing so this assembly is PIC-friendly (runtime-native
  // is built as a `cdylib` for some tests/tools).
  mov rax, qword ptr [rip + RT_GC_EPOCH@GOTPCREL]
  mov rax, qword ptr [rax]
  test rax, 1
  jz .Lgc_safepoint_poll_ret

  // Stack frame layout (184 bytes total, keeps 16-byte alignment before the call):
  //
  //   [rsp + 0 .. 136)   = stackmap_context::ThreadContext (RegContext)
  //   [rsp + 136 .. 176) = SafepointContext (5 * u64)
  //   [rsp + 176 .. 184) = padding
  //
  // Note: the saved RegContext uses *stackmap semantics* for SP/IP:
  // - rsp = post-call SP (sp_entry + 8)
  // - rip = return address loaded from [sp_entry]
  sub rsp, 184

  // Save GPRs into RegContext (offsets match stackmap-context x86_64 ThreadContext).
  mov qword ptr [rsp + 0], rax
  mov qword ptr [rsp + 8], rdx
  mov qword ptr [rsp + 16], rcx
  mov qword ptr [rsp + 24], rbx
  mov qword ptr [rsp + 32], rsi
  mov qword ptr [rsp + 40], rdi
  mov qword ptr [rsp + 48], rbp
  // rsp (DWARF reg 7) and rip (DWARF reg 16) are filled below with stackmap semantics.
  mov qword ptr [rsp + 64], r8
  mov qword ptr [rsp + 72], r9
  mov qword ptr [rsp + 80], r10
  mov qword ptr [rsp + 88], r11
  mov qword ptr [rsp + 96], r12
  mov qword ptr [rsp + 104], r13
  mov qword ptr [rsp + 112], r14
  mov qword ptr [rsp + 120], r15

  // Compute callee-entry sp and callsite return address.
  lea rax, [rsp + 184]        // sp_entry (original rsp)
  mov rcx, qword ptr [rax]    // ip (return address into caller)
  lea rdx, [rax + 8]          // sp (post-call; stackmap SP)

  // Fill in stackmap-semantics rsp/rip in RegContext.
  mov qword ptr [rsp + 56], rdx  // rsp
  mov qword ptr [rsp + 128], rcx // rip

  // Fill SafepointContext at [rsp + 136].
  mov qword ptr [rsp + 136 + 0], rax  // sp_entry
  mov qword ptr [rsp + 136 + 8], rdx  // sp
  mov qword ptr [rsp + 136 + 16], rbp // fp
  mov qword ptr [rsp + 136 + 24], rcx // ip
  lea r8, [rsp + 0]
  mov qword ptr [rsp + 136 + 32], r8  // regs = &RegContext

  // Call the Rust slow-path implementation directly so the published context
  // corresponds to the managed poll callsite.
  mov rdi, qword ptr [rsp + 0]
  lea rsi, [rsp + 136]
  call runtime_native_gc_safepoint_slow_impl

  // Restore registers from RegContext (potentially rewritten by the GC).
  mov rax, qword ptr [rsp + 0]
  mov rdx, qword ptr [rsp + 8]
  mov rcx, qword ptr [rsp + 16]
  mov rbx, qword ptr [rsp + 24]
  mov rsi, qword ptr [rsp + 32]
  mov rdi, qword ptr [rsp + 40]
  mov rbp, qword ptr [rsp + 48]
  mov r8, qword ptr [rsp + 64]
  mov r9, qword ptr [rsp + 72]
  mov r10, qword ptr [rsp + 80]
  mov r11, qword ptr [rsp + 88]
  mov r12, qword ptr [rsp + 96]
  mov r13, qword ptr [rsp + 104]
  mov r14, qword ptr [rsp + 112]
  mov r15, qword ptr [rsp + 120]

  add rsp, 184
.Lgc_safepoint_poll_ret:
  ret

  "#
);

// Exported wrappers for cdylib builds:
//
// On ELF, Rust's `cdylib` builds use a linker version script that only exports
// Rust-defined `#[no_mangle]`/`#[export_name]` symbols. The raw `global_asm!`
// labels above would otherwise be present in the object file but omitted from
// the dynamic symbol table, breaking dynamic linking of native codegen modules.
//
// Define tiny naked Rust entrypoints that tail-jump to the assembly
// implementations so these symbols are treated as part of the exported ABI.

#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn rt_gc_safepoint_slow(_epoch: u64) {
  core::arch::naked_asm!("jmp runtime_native_gc_safepoint_slow_asm");
}

#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
#[export_name = "gc.safepoint_poll"]
pub unsafe extern "C" fn gc_safepoint_poll() {
  core::arch::naked_asm!("jmp runtime_native_gc_safepoint_poll_asm");
}
