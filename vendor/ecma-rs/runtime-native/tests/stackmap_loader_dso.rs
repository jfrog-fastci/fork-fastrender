#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use runtime_native::stackmaps::StackMaps;
use runtime_native::{build_global_stackmap_index, load_all_llvm_stackmaps};
use object::{Object, ObjectSegment};
use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PATCHPOINT_ID_A: u64 = 0x1111_2222_3333_4444;
const PATCHPOINT_ID_B: u64 = 0x2222_3333_4444_5555;

fn command_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn find_tool(candidates: &[&'static str]) -> Option<&'static str> {
  for &cand in candidates {
    if command_works(cand) {
      return Some(cand);
    }
  }
  None
}

fn find_clang() -> Option<&'static str> {
  find_tool(&["clang-18", "clang"])
}

fn find_llc() -> Option<&'static str> {
  find_tool(&["llc-18", "llc"])
}

fn find_llvm_objcopy() -> Option<&'static str> {
  find_tool(&["llvm-objcopy-18", "llvm-objcopy"])
}

fn find_llvm_readobj() -> Option<&'static str> {
  find_tool(&["llvm-readobj-18", "llvm-readobj"])
}

fn find_lld_fuse_arg() -> Option<&'static str> {
  // Prefer the version-suffixed binary when available (matches our exec plan install).
  if command_works("ld.lld-18") {
    Some("lld-18")
  } else if command_works("ld.lld") {
    Some("lld")
  } else {
    None
  }
}

fn run(cmd: &mut Command) {
  let out = cmd.output().unwrap();
  assert!(
    out.status.success(),
    "command failed (status={}):\n  cmd={cmd:?}\n  stdout:\n{}\n  stderr:\n{}\n",
    out.status,
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );
}

fn try_run(cmd: &mut Command) -> Result<(), String> {
  let out = cmd.output().unwrap();
  if out.status.success() {
    Ok(())
  } else {
    Err(format!(
      "command failed (status={}):\n  cmd={cmd:?}\n  stdout:\n{}\n  stderr:\n{}\n",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr),
    ))
  }
}

fn write_ir(out_dir: &Path, module_name: &str, patchpoint_id: u64) -> PathBuf {
  let ll_path = out_dir.join(format!("{module_name}.ll"));
  let ll = format!(
    r#"
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @{module_name}() {{
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 {patchpoint_id}, i32 0)
  ret void
}}
"#
  );
  fs::write(&ll_path, ll).unwrap();
  ll_path
}

fn compile_ir_to_obj(
  llc: &str,
  out_dir: &Path,
  module_name: &str,
  patchpoint_id: u64,
) -> PathBuf {
  let ll_path = write_ir(out_dir, module_name, patchpoint_id);
  let obj_path = out_dir.join(format!("{module_name}.o"));

  let mut cmd = Command::new(llc);
  cmd.arg("-O0")
    .arg("-filetype=obj")
    .arg("-relocation-model=pic")
    .arg("-o")
    .arg(&obj_path)
    .arg(&ll_path);
  run(&mut cmd);
  obj_path
}

fn rename_stackmaps_section_to_data_rel_ro(objcopy: &str, readobj: &str, obj: &Path) {
  // `.llvm_stackmaps` contains absolute code pointers, so it needs relocations under PIE/DSO.
  // Renaming the input section to `.data.rel.ro.llvm_stackmaps` allows relocations to be applied
  // in a writable segment, then protected by RELRO, avoiding DT_TEXTREL.
  let mut cmd = Command::new(objcopy);
  cmd.arg("--rename-section")
    .arg(".llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents")
    .arg(obj);
  run(&mut cmd);

  // Sanity check the rename so this test actually exercises the `.data.rel.ro.*` discovery path.
  let mut check = Command::new(readobj);
  check.arg("--sections").arg(obj);
  let out = check.output().unwrap();
  assert!(
    out.status.success(),
    "command failed (status={}):\n  cmd={check:?}\n  stdout:\n{}\n  stderr:\n{}\n",
    out.status,
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );
  let stdout = String::from_utf8_lossy(&out.stdout);
  assert!(
    stdout.contains(".data.rel.ro.llvm_stackmaps"),
    "expected renamed section .data.rel.ro.llvm_stackmaps in {obj:?}, got:\n{stdout}"
  );
}

#[derive(Clone, Copy, Debug)]
enum StackmapsLinkerScript {
  /// GNU ld-only fragment (`INSERT BEFORE .dynamic`) to avoid RWX in PIE/DSO mode.
  GnuLd,
  /// lld-friendly fragment (`INSERT BEFORE .dynamic`) that appends stackmaps into `.data.rel.ro`
  /// so they stay inside the RELRO block without triggering lld RELRO contiguity errors.
  Lld,
}

fn link_shared(
  clang: &str,
  out_dir: &Path,
  objs: &[PathBuf],
  lld_script: &Path,
  gnuld_script: &Path,
) -> Option<(PathBuf, StackmapsLinkerScript)> {
  let so_path = out_dir.join("libstackmaps.so");

  // Prefer GNU ld + stackmaps_gnuld.ld when available to avoid RWX in PIE/DSO mode.
  // If the system linker is not GNU ld (e.g. lld or mold), this script may fail
  // to link; fall back to lld + stackmaps.ld in that case.
  let mut cmd = Command::new(clang);
  cmd.arg("-shared").arg("-fPIC").arg("-o").arg(&so_path);
  // Regression guard: section GC can drop unreferenced `.llvm_stackmaps` unless
  // the linker script explicitly `KEEP()`s it.
  cmd.arg("-Wl,--gc-sections");
  cmd.arg(format!("-Wl,-T,{}", gnuld_script.display()));
  for obj in objs {
    cmd.arg(obj);
  }
  match try_run(&mut cmd) {
    Ok(()) => {
      assert!(so_path.exists());
      return Some((so_path, StackmapsLinkerScript::GnuLd));
    }
    Err(gnuld_err) => {
      let Some(lld_fuse) = find_lld_fuse_arg() else {
        eprintln!(
          "skipping: failed to link shared library with system linker + stackmaps_gnuld.ld, and ld.lld not found\n{gnuld_err}"
        );
        return None;
      };

      let mut cmd2 = Command::new(clang);
      cmd2
        .arg(format!("-fuse-ld={lld_fuse}"))
        .arg("-shared")
        .arg("-fPIC")
        .arg("-o")
        .arg(&so_path)
        .arg("-Wl,--gc-sections")
        .arg(format!("-Wl,-T,{}", lld_script.display()));
      for obj in objs {
        cmd2.arg(obj);
      }
      if let Err(lld_err) = try_run(&mut cmd2) {
        panic!(
          "failed to link shared library with either:\n\
           - system linker + stackmaps_gnuld.ld\n\
           - lld + stackmaps.ld\n\n\
           system linker attempt:\n{gnuld_err}\n\
           lld attempt:\n{lld_err}"
        );
      }
      assert!(so_path.exists());
      Some((so_path, StackmapsLinkerScript::Lld))
    }
  }
}

fn has_wx_segment(elf: &object::File<'_>) -> bool {
  // PF_X (execute) is bit 0, PF_W (write) is bit 1 on ELF.
  const PF_X: u32 = 1;
  const PF_W: u32 = 2;

  elf.segments().any(|seg| match seg.flags() {
    object::SegmentFlags::Elf { p_flags } => (p_flags & (PF_W | PF_X)) == (PF_W | PF_X),
    _ => false,
  })
}

unsafe fn dlopen(path: &Path) -> *mut libc::c_void {
  let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
  let handle = libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW);
  if handle.is_null() {
    let err = libc::dlerror();
    let msg = if err.is_null() {
      "<dlerror returned null>".to_string()
    } else {
      CStr::from_ptr(err).to_string_lossy().to_string()
    };
    panic!("dlopen({}) failed: {msg}", path.display());
  }
  handle
}

fn slice_contains_patchpoint_ids(slice: &[u8], ids: &[u64]) -> bool {
  let Ok(stackmaps) = StackMaps::parse(slice) else {
    return false;
  };

  let mut seen = vec![false; ids.len()];
  for raw in stackmaps.raws() {
    for rec in &raw.records {
      for (i, &id) in ids.iter().enumerate() {
        if rec.patchpoint_id == id {
          seen[i] = true;
        }
      }
    }
  }
  seen.into_iter().all(|b| b)
}

#[test]
fn discovers_stackmaps_in_dlopened_shared_library() {
  let Some(llc) = find_llc() else {
    eprintln!("skipping: llc not found in PATH (need llc-18 or llc)");
    return;
  };
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found in PATH (need clang-18 or clang)");
    return;
  };
  let Some(objcopy) = find_llvm_objcopy() else {
    eprintln!("skipping: llvm-objcopy not found in PATH (need llvm-objcopy-18 or llvm-objcopy)");
    return;
  };
  let Some(readobj) = find_llvm_readobj() else {
    eprintln!("skipping: llvm-readobj not found in PATH (need llvm-readobj-18 or llvm-readobj)");
    return;
  };

  let td = tempfile::tempdir().unwrap();
  let obj_a = compile_ir_to_obj(llc, td.path(), "sm_a", PATCHPOINT_ID_A);
  let obj_b = compile_ir_to_obj(llc, td.path(), "sm_b", PATCHPOINT_ID_B);

  rename_stackmaps_section_to_data_rel_ro(objcopy, readobj, &obj_a);
  rename_stackmaps_section_to_data_rel_ro(objcopy, readobj, &obj_b);

  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let lld_script = manifest_dir.join("link/stackmaps.ld");
  let gnuld_script = manifest_dir.join("link/stackmaps_gnuld.ld");
  assert!(lld_script.exists(), "missing linker script at {lld_script:?}");
  assert!(
    gnuld_script.exists(),
    "missing linker script at {gnuld_script:?}"
  );

  let Some((so, script_used)) = link_shared(clang, td.path(), &[obj_a, obj_b], &lld_script, &gnuld_script)
  else {
    return;
  };

  // Ensure we didn't accidentally introduce an RWX segment (hardening regression).
  let so_bytes = fs::read(&so).expect("read shared library");
  let elf = object::File::parse(&*so_bytes).expect("parse shared library");
  assert!(
    !has_wx_segment(&elf),
    "expected shared library to have no W+X segments (linked with {script_used:?})"
  );

  // Load the shared library so it appears in `dl_iterate_phdr` output.
  let _handle = unsafe { dlopen(&so) };

  let slices = load_all_llvm_stackmaps().expect("load_all_llvm_stackmaps should succeed");
  assert!(
    slices.iter().any(|s| slice_contains_patchpoint_ids(s, &[PATCHPOINT_ID_A, PATCHPOINT_ID_B])),
    "expected to discover .llvm_stackmaps from the dlopened .so (patchpoint IDs not found)"
  );

  // Ensure the global merged index contains callsites from both concatenated blobs.
  let index = build_global_stackmap_index().expect("build_global_stackmap_index should succeed");
  let mut found_a = false;
  let mut found_b = false;
  for (_pc, callsite) in index.iter() {
    if callsite.record.patchpoint_id == PATCHPOINT_ID_A {
      found_a = true;
    }
    if callsite.record.patchpoint_id == PATCHPOINT_ID_B {
      found_b = true;
    }
  }
  assert!(found_a, "missing patchpoint id {PATCHPOINT_ID_A:#x} in global stackmap index");
  assert!(found_b, "missing patchpoint id {PATCHPOINT_ID_B:#x} in global stackmap index");
}
