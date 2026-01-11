use core::arch::global_asm;

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
  ret
  .globl runtime_native_gc_safepoint_slow_asm
runtime_native_gc_safepoint_slow_asm:
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
  call runtime_native_gc_safepoint_slow_impl

  add rsp, 40
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

  // Capture caller SP/FP/RA at poll entry.
  mov rcx, rsp                // sp_entry
  mov rdx, qword ptr [rsp]    // ip (return address to caller)
  lea r8, [rcx + 8]           // sp (post-call; stackmap SP)

  // Reserve space for SafepointContext (32 bytes) and align stack to 16 bytes
  // before calling into Rust.
  sub rsp, 40

  mov qword ptr [rsp + 0], rcx
  mov qword ptr [rsp + 8], r8
  mov qword ptr [rsp + 16], rbp
  mov qword ptr [rsp + 24], rdx

  // Call the Rust slow-path implementation directly so the published context
  // corresponds to the managed poll callsite.
  mov rdi, rax
  lea rsi, [rsp]
  call runtime_native_gc_safepoint_slow_impl

  add rsp, 40
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
