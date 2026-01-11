#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use runtime_native::stackmaps::StackMaps;
use runtime_native::{build_global_stackmap_index, load_all_llvm_stackmaps};
use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PATCHPOINT_ID_A: u64 = 0x1111_2222_3333_4444;
const PATCHPOINT_ID_B: u64 = 0x2222_3333_4444_5555;

fn find_tool(candidates: &[&'static str]) -> &'static str {
  for &cand in candidates {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return cand;
    }
  }
  panic!("unable to locate required tool (tried {candidates:?})");
}

fn find_clang() -> &'static str {
  find_tool(&["clang-18", "clang"])
}

fn find_llc() -> &'static str {
  find_tool(&["llc-18", "llc"])
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

fn compile_ir_to_obj(out_dir: &Path, module_name: &str, patchpoint_id: u64) -> PathBuf {
  let ll_path = write_ir(out_dir, module_name, patchpoint_id);
  let obj_path = out_dir.join(format!("{module_name}.o"));

  let mut cmd = Command::new(find_llc());
  cmd.arg("-O0")
    .arg("-filetype=obj")
    .arg("-relocation-model=pic")
    .arg("-o")
    .arg(&obj_path)
    .arg(&ll_path);
  run(&mut cmd);
  obj_path
}

fn link_shared(out_dir: &Path, objs: &[PathBuf]) -> PathBuf {
  let so_path = out_dir.join("libstackmaps.so");
  let mut cmd = Command::new(find_clang());
  cmd.arg("-shared").arg("-fPIC").arg("-o").arg(&so_path);
  for obj in objs {
    cmd.arg(obj);
  }
  run(&mut cmd);
  assert!(so_path.exists());
  so_path
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
  let td = tempfile::tempdir().unwrap();
  let obj_a = compile_ir_to_obj(td.path(), "sm_a", PATCHPOINT_ID_A);
  let obj_b = compile_ir_to_obj(td.path(), "sm_b", PATCHPOINT_ID_B);
  let so = link_shared(td.path(), &[obj_a, obj_b]);

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
