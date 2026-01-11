#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use stackmap_context::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};

static READY: AtomicBool = AtomicBool::new(false);
static RSP_ASM: AtomicU64 = AtomicU64::new(0);
static RSP_CTX: AtomicU64 = AtomicU64::new(0);
static RIP_CTX: AtomicU64 = AtomicU64::new(0);

extern "C" fn handler(_sig: libc::c_int) {
  unsafe {
    let mut uc = MaybeUninit::<libc::ucontext_t>::uninit();
    // `getcontext` is not async-signal-safe, but is sufficient for this test:
    // we only need a `ucontext_t` that reflects the handler's own register state.
    assert_eq!(libc::getcontext(uc.as_mut_ptr()), 0);
    let uc = uc.assume_init();

    let rsp_asm: u64;
    core::arch::asm!("mov {0}, rsp", out(reg) rsp_asm);

    let ctx = ThreadContext::from_ucontext(&uc);
    let rsp_ctx = ctx.get_dwarf_reg_u64(DWARF_REG_SP).unwrap();
    let rip_ctx = ctx.get_dwarf_reg_u64(DWARF_REG_IP).unwrap();

    RSP_ASM.store(rsp_asm, Ordering::Relaxed);
    RSP_CTX.store(rsp_ctx, Ordering::Relaxed);
    RIP_CTX.store(rip_ctx, Ordering::Relaxed);
    READY.store(true, Ordering::Release);
  }
}

#[test]
fn getcontext_based_ucontext_matches_handler_registers() {
  unsafe {
    let mut sa: libc::sigaction = core::mem::zeroed();
    sa.sa_flags = 0;
    sa.sa_sigaction = handler as usize;
    libc::sigemptyset(&mut sa.sa_mask);
    assert_eq!(
      libc::sigaction(libc::SIGUSR1, &sa, core::ptr::null_mut()),
      0
    );

    assert_eq!(libc::raise(libc::SIGUSR1), 0);

    assert!(READY.load(Ordering::Acquire));

    assert_eq!(RSP_CTX.load(Ordering::Relaxed), RSP_ASM.load(Ordering::Relaxed));

    let rip = RIP_CTX.load(Ordering::Relaxed);
    let handler_addr = handler as usize as u64;
    assert!(
      (handler_addr..handler_addr + 4096).contains(&rip),
      "IP {rip:#x} not in handler range {handler_addr:#x}..{:#x}",
      handler_addr + 4096
    );

    // Restore default handler to avoid affecting other tests within the process.
    let mut sa_default: libc::sigaction = core::mem::zeroed();
    sa_default.sa_flags = 0;
    sa_default.sa_sigaction = libc::SIG_DFL;
    libc::sigemptyset(&mut sa_default.sa_mask);
    assert_eq!(
      libc::sigaction(libc::SIGUSR1, &sa_default, core::ptr::null_mut()),
      0
    );
  }
}

