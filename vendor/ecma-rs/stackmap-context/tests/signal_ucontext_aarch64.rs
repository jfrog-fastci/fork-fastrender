#![cfg(all(
  target_os = "linux",
  target_arch = "aarch64",
  feature = "aarch64-signal-test"
))]

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use stackmap_context::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};

static READY: AtomicBool = AtomicBool::new(false);
static SP_ASM: AtomicU64 = AtomicU64::new(0);
static SP_CTX: AtomicU64 = AtomicU64::new(0);
static PC_CTX: AtomicU64 = AtomicU64::new(0);

fn trigger_sigtrap() {
  unsafe {
    let sp: u64;
    core::arch::asm!("mov {0}, sp", out(reg) sp);
    SP_ASM.store(sp, Ordering::Relaxed);

    // Breakpoint trap so the signal ucontext reflects this frame's registers.
    core::arch::asm!("brk #0");
  }
}

unsafe extern "C" fn sigtrap_handler(
  _sig: libc::c_int,
  _info: *mut libc::siginfo_t,
  uctx: *mut c_void,
) {
  unsafe {
    let uc = uctx as *mut libc::ucontext_t;
    let mut ctx = ThreadContext::from_ucontext(uc);
    let sp_ctx = ctx.get_dwarf_reg_u64(DWARF_REG_SP).unwrap_or(0);
    let pc_ctx = ctx.get_dwarf_reg_u64(DWARF_REG_IP).unwrap_or(0);

    // Skip the `brk` instruction (4 bytes) so execution can resume.
    let _ = ctx.set_dwarf_reg_u64(DWARF_REG_IP, pc_ctx.wrapping_add(4));
    ctx.write_to_ucontext(uc);

    SP_CTX.store(sp_ctx, Ordering::Relaxed);
    PC_CTX.store(pc_ctx, Ordering::Relaxed);
    READY.store(true, Ordering::Release);
  }
}

#[test]
fn ucontext_extraction_matches_handler_registers() {
  unsafe {
    let mut sa: libc::sigaction = core::mem::zeroed();
    sa.sa_flags = libc::SA_SIGINFO;
    sa.sa_sigaction = sigtrap_handler as usize;
    libc::sigemptyset(&mut sa.sa_mask);
    assert_eq!(libc::sigaction(libc::SIGTRAP, &sa, core::ptr::null_mut()), 0);

    trigger_sigtrap();
    assert!(READY.load(Ordering::Acquire));

    assert_eq!(SP_CTX.load(Ordering::Relaxed), SP_ASM.load(Ordering::Relaxed));

    let pc = PC_CTX.load(Ordering::Relaxed);
    let handler_addr = trigger_sigtrap as usize as u64;
    assert!(
      (handler_addr..handler_addr + 4096).contains(&pc),
      "PC {pc:#x} not in handler range {handler_addr:#x}..{:#x}",
      handler_addr + 4096
    );

    let mut sa_default: libc::sigaction = core::mem::zeroed();
    sa_default.sa_flags = 0;
    sa_default.sa_sigaction = libc::SIG_DFL;
    libc::sigemptyset(&mut sa_default.sa_mask);
    assert_eq!(
      libc::sigaction(libc::SIGTRAP, &sa_default, core::ptr::null_mut()),
      0
    );
  }
}
