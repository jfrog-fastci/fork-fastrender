#![cfg(target_os = "linux")]

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

use tempfile::TempDir;

use runtime_native::lookup;

unsafe fn dlopen(path: &std::path::Path) -> *mut libc::c_void {
  let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
  let handle = libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW);
  if handle.is_null() {
    let err = libc::dlerror();
    let msg = if err.is_null() {
      "<null dlerror>".to_string()
    } else {
      CStr::from_ptr(err).to_string_lossy().into_owned()
    };
    panic!("dlopen({}): {}", path.display(), msg);
  }
  handle
}

unsafe fn dlsym(handle: *mut libc::c_void, sym: &str) -> *mut libc::c_void {
  // Clear any previous error.
  libc::dlerror();

  let c_sym = CString::new(sym).unwrap();
  let ptr = libc::dlsym(handle, c_sym.as_ptr());
  if ptr.is_null() {
    let err = libc::dlerror();
    let msg = if err.is_null() {
      "<null dlerror>".to_string()
    } else {
      CStr::from_ptr(err).to_string_lossy().into_owned()
    };
    panic!("dlsym({sym}): {msg}");
  }
  ptr
}

#[test]
fn dlopen_registers_multiple_stackmap_modules() {
  // Skip if dlopen is somehow not available.
  if unsafe { libc::dlopen(std::ptr::null(), libc::RTLD_NOW) }.is_null() {
    eprintln!("dlopen unavailable; skipping");
    return;
  }

  let dir = TempDir::new().unwrap();

  let lib1 = build_test_module(&dir, "mod1").unwrap();
  let lib2 = build_test_module(&dir, "mod2").unwrap();

  unsafe {
    let h1 = dlopen(&lib1);
    let h2 = dlopen(&lib2);

    let mod1_registered = dlsym(h1, "mod1_registered") as *const libc::c_int;
    let mod2_registered = dlsym(h2, "mod2_registered") as *const libc::c_int;

    assert_eq!(*mod1_registered, 1, "mod1 constructor should register stackmaps");
    assert_eq!(*mod2_registered, 1, "mod2 constructor should register stackmaps");

    // Read the callsite PCs from the registered stackmaps themselves. This avoids
    // any subtleties around function symbol resolution (PLT vs body address) in
    // hand-crafted test DSOs.
    let pc1 = stackmap_first_callsite_pc(h1) as usize;
    let pc2 = stackmap_first_callsite_pc(h2) as usize;

    assert!(lookup(pc1).is_some(), "mod1 callsite should be registered");
    assert!(lookup(pc2).is_some(), "mod2 callsite should be registered");
  }
}

#[test]
fn dlopen_then_scan_registers_stackmaps_without_constructors() {
  // Skip if dlopen is somehow not available.
  if unsafe { libc::dlopen(std::ptr::null(), libc::RTLD_NOW) }.is_null() {
    eprintln!("dlopen unavailable; skipping");
    return;
  }

  let dir = TempDir::new().unwrap();
  let lib = build_test_module_without_registration(&dir, "mod_scan").unwrap();

  unsafe {
    let h = dlopen(&lib);

    let pc = stackmap_first_callsite_pc(h) as usize;
    assert!(
      lookup(pc).is_none(),
      "module should not be registered before scan"
    );

    runtime_native::global_stackmap_registry()
      .write()
      .load_all_loaded_modules()
      .expect("load_all_loaded_modules should succeed");

    assert!(lookup(pc).is_some(), "callsite should be registered after scan");

    // Clean up so this test doesn't leak global registry state into other tests.
    let start = dlsym(h, "__llvm_stackmaps_start") as *const u8;
    assert!(
      runtime_native::rt_stackmaps_unregister(start),
      "unregister should succeed"
    );
  }
}

unsafe fn stackmap_first_callsite_pc(handle: *mut libc::c_void) -> u64 {
  let start = dlsym(handle, "__llvm_stackmaps_start") as *const u8;
  let end = dlsym(handle, "__llvm_stackmaps_end") as *const u8;

  let len = (end as usize).checked_sub(start as usize).expect("end < start");
  let bytes = std::slice::from_raw_parts(start, len);

  let maps = runtime_native::StackMaps::parse(bytes).expect("parse stackmaps blob");
  maps.callsites().first().expect("expected 1 callsite").pc
}

fn build_test_module(dir: &TempDir, name: &str) -> std::io::Result<std::path::PathBuf> {
  let src_path = dir.path().join(format!("{name}.c"));
  let out_path = dir.path().join(format!("lib{name}.so"));

  let code = format!(
    r#"
      #include <stdbool.h>
      #include <stdint.h>

      // Provided by the host runtime-native.
      bool rt_stackmaps_register(const uint8_t* start, const uint8_t* end);

      extern uint8_t __llvm_stackmaps_start;
      extern uint8_t __llvm_stackmaps_end;

      int {name}_registered = 0;

      void {name}_target(void) {{}}

      __attribute__((constructor))
      static void {name}_ctor(void) {{
        {name}_registered = rt_stackmaps_register(&__llvm_stackmaps_start, &__llvm_stackmaps_end) ? 1 : 0;
      }}

      // Hand-crafted minimal LLVM stackmap blob with one function and one record.
      __asm__(
        ".section .llvm_stackmaps,\"aw\",@progbits\n"
        ".globl __llvm_stackmaps_start\n"
        "__llvm_stackmaps_start:\n"
        // Header
        ".byte 3\n"          // Version
        ".byte 0\n"          // Reserved
        ".short 0\n"         // Reserved
        ".long 1\n"          // NumFunctions
        ".long 0\n"          // NumConstants
        ".long 1\n"          // NumRecords
        // FunctionInfo[0]
        ".quad {name}_target\n" // FunctionAddress (relocated)
        ".quad 0\n"          // StackSize
        ".quad 1\n"          // RecordCount
        // Record[0]
        ".quad 1\n"          // PatchPointID
        ".long 0\n"          // InstructionOffset
        ".short 0\n"         // Reserved
        ".short 0\n"         // NumLocations
        ".short 0\n"         // Padding
        ".short 0\n"         // NumLiveOuts
        ".long 0\n"          // Record padding to 8-byte alignment
        ".globl __llvm_stackmaps_end\n"
        "__llvm_stackmaps_end:\n"
        ".previous\n"
      );
    "#
  );

  std::fs::write(&src_path, code)?;

  let status = Command::new("cc")
    .arg("-shared")
    .arg("-fPIC")
    .arg("-O0")
    .arg("-o")
    .arg(&out_path)
    .arg(&src_path)
    .status()?;

  assert!(status.success(), "cc failed for {name}");
  Ok(out_path)
}

fn build_test_module_without_registration(
  dir: &TempDir,
  name: &str,
) -> std::io::Result<std::path::PathBuf> {
  let src_path = dir.path().join(format!("{name}.c"));
  let out_path = dir.path().join(format!("lib{name}.so"));

  let code = format!(
    r#"
      #include <stdint.h>

      void {name}_target(void) {{}}

      // Hand-crafted minimal LLVM stackmap blob with one function and one record.
      __asm__(
        ".section .llvm_stackmaps,\"aw\",@progbits\n"
        ".globl __llvm_stackmaps_start\n"
        "__llvm_stackmaps_start:\n"
        // Header
        ".byte 3\n"          // Version
        ".byte 0\n"          // Reserved
        ".short 0\n"         // Reserved
        ".long 1\n"          // NumFunctions
        ".long 0\n"          // NumConstants
        ".long 1\n"          // NumRecords
        // FunctionInfo[0]
        ".quad {name}_target\n" // FunctionAddress (relocated)
        ".quad 0\n"          // StackSize
        ".quad 1\n"          // RecordCount
        // Record[0]
        ".quad 1\n"          // PatchPointID
        ".long 0\n"          // InstructionOffset
        ".short 0\n"         // Reserved
        ".short 0\n"         // NumLocations
        ".short 0\n"         // Padding
        ".short 0\n"         // NumLiveOuts
        ".long 0\n"          // Record padding to 8-byte alignment
        ".globl __llvm_stackmaps_end\n"
        "__llvm_stackmaps_end:\n"
        ".previous\n"
      );
    "#
  );

  std::fs::write(&src_path, code)?;

  let status = Command::new("cc")
    .arg("-shared")
    .arg("-fPIC")
    .arg("-O0")
    .arg("-o")
    .arg(&out_path)
    .arg(&src_path)
    .status()?;

  assert!(status.success(), "cc failed for {name}");
  Ok(out_path)
}
