#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use stackmap_context::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};

static READY: AtomicBool = AtomicBool::new(false);
static RSP_ASM: AtomicU64 = AtomicU64::new(0);
static RSP_CTX: AtomicU64 = AtomicU64::new(0);
static RIP_CTX: AtomicU64 = AtomicU64::new(0);
static RAX_CTX: AtomicU64 = AtomicU64::new(0);

const MAGIC_RAX: u64 = 0x0123_4567_89ab_cdef;
const UD2_OPCODE: u16 = 0x0b0f;

fn trigger_sigill() {
  unsafe {
    let rsp: u64;
    core::arch::asm!("mov {0}, rsp", out(reg) rsp);
    RSP_ASM.store(rsp, Ordering::Relaxed);

    // Synchronous trap so the signal ucontext reflects this frame's registers.
    core::arch::asm!("ud2", in("rax") MAGIC_RAX);
  }
}

unsafe extern "C" fn sigill_handler(
  _sig: libc::c_int,
  _info: *mut libc::siginfo_t,
  uctx: *mut c_void,
) {
  unsafe {
    let uc = uctx as *mut libc::ucontext_t;
    let mut ctx = ThreadContext::from_ucontext(uc);
    let rsp_ctx = ctx.get_dwarf_reg_u64(DWARF_REG_SP).unwrap_or(0);
    let rip_ctx = ctx.get_dwarf_reg_u64(DWARF_REG_IP).unwrap_or(0);
    let rax_ctx = ctx.get_dwarf_reg_u64(0).unwrap_or(0);

    // `rip` should point at the faulting `ud2`, but some environments may report the PC
    // *after* the trapping instruction. Normalize using the instruction bytes so we can
    // reliably skip exactly the trap.
    let mut trap_rip = rip_ctx;
    if trap_rip != 0 {
      // SAFETY: `trap_rip` should point within this process' executable text mapping.
      let instr_at_pc = (trap_rip as *const u16).read_unaligned();
      if instr_at_pc != UD2_OPCODE {
        let prev = trap_rip.wrapping_sub(2);
        let instr_prev = (prev as *const u16).read_unaligned();
        if instr_prev == UD2_OPCODE {
          trap_rip = prev;
        }
      }
    }

    // Skip the `ud2` instruction (2 bytes) so execution can resume.
    let _ = ctx.set_dwarf_reg_u64(DWARF_REG_IP, trap_rip.wrapping_add(2));
    ctx.write_to_ucontext(uc);

    RSP_CTX.store(rsp_ctx, Ordering::Relaxed);
    RIP_CTX.store(trap_rip, Ordering::Relaxed);
    RAX_CTX.store(rax_ctx, Ordering::Relaxed);
    READY.store(true, Ordering::Release);
  }
}

#[test]
fn ucontext_extraction_matches_handler_registers() {
  unsafe {
    // Install SIGILL handler with SA_SIGINFO so we can read/write the ucontext.
    let mut sa: libc::sigaction = core::mem::zeroed();
    sa.sa_flags = libc::SA_SIGINFO;
    sa.sa_sigaction = sigill_handler as usize;
    libc::sigemptyset(&mut sa.sa_mask);
    assert_eq!(
      libc::sigaction(libc::SIGILL, &sa, core::ptr::null_mut()),
      0
    );

    trigger_sigill();

    assert!(READY.load(Ordering::Acquire));

    let rsp_asm = RSP_ASM.load(Ordering::Relaxed);
    let rsp_ctx = RSP_CTX.load(Ordering::Relaxed);
    assert_eq!(rsp_ctx, rsp_asm);

    let rip = RIP_CTX.load(Ordering::Relaxed);
    let handler_addr = trigger_sigill as usize as u64;
    assert!(
      (handler_addr..handler_addr + 4096).contains(&rip),
      "IP {rip:#x} not in handler range {handler_addr:#x}..{:#x}",
      handler_addr + 4096
    );

    assert_eq!(RAX_CTX.load(Ordering::Relaxed), MAGIC_RAX);

    // Restore default handler to avoid affecting other tests within the process.
    let mut sa_default: libc::sigaction = core::mem::zeroed();
    sa_default.sa_flags = 0;
    sa_default.sa_sigaction = libc::SIG_DFL;
    libc::sigemptyset(&mut sa_default.sa_mask);
    assert_eq!(
      libc::sigaction(libc::SIGILL, &sa_default, core::ptr::null_mut()),
      0
    );
  }
}
