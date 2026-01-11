#![cfg(all(
  target_os = "linux",
  target_arch = "aarch64",
  feature = "aarch64-signal-test"
))]

use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use stackmap_context::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};

static READY: AtomicBool = AtomicBool::new(false);
static SP_ASM: AtomicU64 = AtomicU64::new(0);
static SP_CTX: AtomicU64 = AtomicU64::new(0);
static PC_CTX: AtomicU64 = AtomicU64::new(0);

extern "C" fn handler(_sig: libc::c_int) {
  unsafe {
    let mut uc = MaybeUninit::<libc::ucontext_t>::uninit();
    // `getcontext` is not async-signal-safe, but is sufficient for this test:
    // we only need a `ucontext_t` that reflects the handler's own register state.
    assert_eq!(libc::getcontext(uc.as_mut_ptr()), 0);
    let uc = uc.assume_init();

    let sp_asm: u64;
    core::arch::asm!("mov {0}, sp", out(reg) sp_asm);

    let ctx = ThreadContext::from_ucontext(&uc);
    let sp_ctx = ctx.get_dwarf_reg_u64(DWARF_REG_SP).unwrap();
    let pc_ctx = ctx.get_dwarf_reg_u64(DWARF_REG_IP).unwrap();

    SP_ASM.store(sp_asm, Ordering::Relaxed);
    SP_CTX.store(sp_ctx, Ordering::Relaxed);
    PC_CTX.store(pc_ctx, Ordering::Relaxed);
    READY.store(true, Ordering::Release);
  }
}

#[test]
fn ucontext_extraction_matches_handler_registers() {
  unsafe {
    let mut sa: libc::sigaction = core::mem::zeroed();
    sa.sa_flags = 0;
    sa.sa_sigaction = handler as usize;
    libc::sigemptyset(&mut sa.sa_mask);
    assert_eq!(libc::sigaction(libc::SIGUSR1, &sa, core::ptr::null_mut()), 0);

    assert_eq!(libc::raise(libc::SIGUSR1), 0);
    assert!(READY.load(Ordering::Acquire));

    assert_eq!(SP_CTX.load(Ordering::Relaxed), SP_ASM.load(Ordering::Relaxed));

    let pc = PC_CTX.load(Ordering::Relaxed);
    let handler_addr = handler as usize as u64;
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
      libc::sigaction(libc::SIGUSR1, &sa_default, core::ptr::null_mut()),
      0
    );
  }
}

