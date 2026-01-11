#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use runtime_native::stackmaps::Location;
use runtime_native::statepoints::{eval_location, RootSlot};
use runtime_native::StackMaps;
use runtime_native::test_util::TestRuntimeGuard;
use stackmap_context::{ThreadContext, DWARF_REG_IP};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use object::{Object, ObjectSection};

// `dlopen`/`dlsym` live in `libdl` on glibc.
#[link(name = "dl")]
extern "C" {}

static STACKMAPS: OnceLock<StackMaps> = OnceLock::new();
static OBSERVED_VALUE: AtomicU64 = AtomicU64::new(0);
static MUTATED_VALUE: AtomicU64 = AtomicU64::new(0);
static HANDLED: AtomicBool = AtomicBool::new(false);
static NEW_VALUE: AtomicU64 = AtomicU64::new(0);

fn tool_available(tool: &str) -> bool {
  Command::new(tool)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if tool_available(cand) {
      return Some(cand);
    }
  }
  None
}

fn run_success(mut cmd: Command) {
  let cmd_str = format!("{cmd:?}");
  let out = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd_str}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd_str}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr),
    );
  }
}

unsafe extern "C" fn sigill_handler(_sig: libc::c_int, _info: *mut libc::siginfo_t, uctx: *mut c_void) {
  let Some(stackmaps) = STACKMAPS.get() else {
    return;
  };

  let uc = uctx as *mut libc::ucontext_t;
  let mut ctx = ThreadContext::from_ucontext(uc);
  let pc = ctx.get_dwarf_reg_u64(DWARF_REG_IP).unwrap();

  // LLVM's stackmap record for `llvm.experimental.stackmap` is emitted at the
  // following instruction (here: `ud2`, i.e. `llvm.trap`).
  //
  // Linux is expected to report the PC at the trapping instruction, but some
  // environments may report the PC *after* it. Be robust by checking PC and
  // PC-2.
  let (trap_pc, callsite) = stackmaps
    .lookup(pc)
    .map(|c| (pc, c))
    .or_else(|| pc.checked_sub(2).and_then(|p| stackmaps.lookup(p).map(|c| (p, c))))
    .expect("callsite record for SIGILL PC");
  assert_eq!(
    callsite.record.locations.len(),
    1,
    "fixture should record exactly one location"
  );
  let loc = &callsite.record.locations[0];
  assert!(
    matches!(loc, Location::Register { .. }),
    "expected root location to be Register, got {loc:?}"
  );

  let slot = eval_location(loc, &ctx).expect("eval register location");
  let RootSlot::Reg { .. } = slot else {
    panic!("expected RootSlot::Reg for Register location");
  };

  let old = slot.read_u64(&ctx);
  OBSERVED_VALUE.store(old, Ordering::Relaxed);
  let new = NEW_VALUE.load(Ordering::Relaxed);
  slot.write_u64(&mut ctx, new);
  MUTATED_VALUE.store(new, Ordering::Relaxed);

  // Skip the `ud2` instruction emitted by `llvm.trap` so execution resumes.
  ctx
    .set_dwarf_reg_u64(DWARF_REG_IP, trap_pc + 2)
    .expect("set RIP");
  ctx.write_to_ucontext(uc);

  HANDLED.store(true, Ordering::Release);
}

unsafe fn dlopen(path: &Path) -> *mut c_void {
  let c_path = CString::new(path.as_os_str().as_bytes()).expect("CString path");
  let handle = libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW);
  if handle.is_null() {
    let err = libc::dlerror();
    if err.is_null() {
      panic!("dlopen failed for {path:?} (dlerror=null)");
    }
    let msg = std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned();
    panic!("dlopen failed for {path:?}: {msg}");
  }
  handle
}

unsafe fn dlsym_fn(handle: *mut c_void, name: &str) -> *mut c_void {
  let c_name = CString::new(name).unwrap();
  libc::dlerror(); // Clear any stale error.
  let sym = libc::dlsym(handle, c_name.as_ptr());
  let err = libc::dlerror();
  if !err.is_null() {
    let msg = std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned();
    panic!("dlsym failed for {name:?}: {msg}");
  }
  sym
}

unsafe fn dl_base_addr(sym: *mut c_void) -> usize {
  let mut info: libc::Dl_info = core::mem::zeroed();
  assert_ne!(libc::dladdr(sym, &mut info), 0, "dladdr failed");
  info.dli_fbase as usize
}

fn stackmaps_section_bytes_from_loaded_elf(path: &Path, base_addr: usize) -> &'static [u8] {
  let bytes = fs::read(path).expect("read shared library");
  let obj = object::File::parse(&*bytes).expect("parse shared library");
  let section = obj
    .section_by_name(".data.rel.ro.llvm_stackmaps")
    .or_else(|| obj.section_by_name(".llvm_stackmaps"))
    .expect("shared library should contain a stackmaps section");
  let addr = section.address() as usize;
  let size = section.size() as usize;
  assert!(size > 0, "stackmaps section is empty");

  unsafe { core::slice::from_raw_parts((base_addr + addr) as *const u8, size) }
}

#[test]
fn signal_handler_can_rewrite_register_root_via_stackmaps() {
  let _rt = TestRuntimeGuard::new();
  for tool in ["llvm-as-18", "llc-18"] {
    if !tool_available(tool) {
      eprintln!("skipping: {tool} not available in PATH");
      return;
    }
  }
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not available in PATH (expected `clang-18` or `clang`)");
    return;
  };

  let tmp = tempfile::tempdir().expect("create tempdir");
  let ll = tmp.path().join("reg_root.ll");
  let bc = tmp.path().join("reg_root.bc");
  let obj = tmp.path().join("reg_root.o");
  let so = tmp.path().join("libreg_root.so");

  // Minimal IR that:
  // - records one live value in a stackmap (often a register), and
  // - immediately traps with `ud2` so our SIGILL handler runs at the stackmap PC.
  fs::write(
    &ll,
    r#"
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64 immarg, i32 immarg, ...)
declare void @llvm.trap()

define ptr @foo(ptr %x) {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0, ptr %x)
  call void @llvm.trap()
  ret ptr %x
}
"#,
  )
  .expect("write fixture IR");

  let mut as_cmd = Command::new("llvm-as-18");
  as_cmd.arg(&ll).arg("-o").arg(&bc);
  run_success(as_cmd);

  let mut llc_cmd = Command::new("llc-18");
  llc_cmd
    .arg("-O0")
    .arg("-filetype=obj")
    .arg("-relocation-model=pic")
    .arg(&bc)
    .arg("-o")
    .arg(&obj);
  run_success(llc_cmd);

  let mut clang_cmd = Command::new(clang);
  clang_cmd.arg("-shared").arg("-o").arg(&so).arg(&obj);
  run_success(clang_cmd);

  unsafe {
    let handle = dlopen(&so);
    let sym = dlsym_fn(handle, "foo");
    assert!(!sym.is_null(), "dlsym returned null for foo");
    let base = dl_base_addr(sym);

    let stackmap_bytes = stackmaps_section_bytes_from_loaded_elf(&so, base);
    let stackmaps = StackMaps::parse(stackmap_bytes).expect("parse relocated stackmaps section");
    STACKMAPS.set(stackmaps).ok().expect("STACKMAPS initialized once");

    // Install SIGILL handler with SA_SIGINFO so we can modify the ucontext.
    let mut sa: libc::sigaction = core::mem::zeroed();
    sa.sa_flags = libc::SA_SIGINFO;
    sa.sa_sigaction = sigill_handler as usize;
    libc::sigemptyset(&mut sa.sa_mask);
    assert_eq!(libc::sigaction(libc::SIGILL, &sa, core::ptr::null_mut()), 0);

    let foo: extern "C" fn(*mut u8) -> *mut u8 = core::mem::transmute(sym);
    let mut old_box = Box::new(0x1111_2222_3333_4444u64);
    let mut new_box = Box::new(0xaaaa_bbbb_cccc_ddddu64);
    let input = (&mut *old_box) as *mut u64 as *mut u8;
    let expected = (&mut *new_box) as *mut u64 as *mut u8;
    NEW_VALUE.store(expected as u64, Ordering::Relaxed);

    let ret = foo(input);

    assert!(HANDLED.load(Ordering::Acquire), "SIGILL handler did not run");
    assert_eq!(OBSERVED_VALUE.load(Ordering::Relaxed), input as u64);
    assert_eq!(ret, expected);
    // Safety: we wrote a valid pointer value into the register root.
    assert_eq!(*(ret as *const u64), *new_box);

    // Restore default handler to avoid affecting other tests.
    let mut sa_default: libc::sigaction = core::mem::zeroed();
    sa_default.sa_flags = 0;
    sa_default.sa_sigaction = libc::SIG_DFL;
    libc::sigemptyset(&mut sa_default.sa_mask);
    assert_eq!(
      libc::sigaction(libc::SIGILL, &sa_default, core::ptr::null_mut()),
      0
    );

    assert_eq!(libc::dlclose(handle), 0);
  }
}
