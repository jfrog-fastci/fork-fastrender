#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use inkwell::context::Context;
use native_js::runtime_abi::RuntimeAbi;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

fn find_clang() -> Option<&'static str> {
  for candidate in ["clang-18", "clang"] {
    if Command::new(candidate)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
    {
      return Some(candidate);
    }
  }
  None
}

#[test]
fn links_and_runs_rt_thread_init_and_rt_thread_deinit() {
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found in PATH");
    return;
  };

  // `runtime-native` is a dev-dependency of `native-js`, so cargo will already
  // have built its `staticlib` artifact in the same `target/**/deps` directory
  // as this test binary.
  let deps_dir = std::env::current_exe()
    .ok()
    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    .expect("current_exe parent dir");
  let runtime_native_a = deps_dir.join("libruntime_native.a");
  if !runtime_native_a.is_file() {
    eprintln!(
      "skipping: expected runtime-native staticlib at {}",
      runtime_native_a.display()
    );
    return;
  }

  // Build a minimal module that calls `rt_thread_init` + `rt_thread_deinit` via `RuntimeAbi`.
  let context = Context::create();
  let module = context.create_module("rt_thread_link_smoke");
  let builder = context.create_builder();

  let fns = RuntimeAbi::new(&context, &module).ensure_wrappers();

  let i32 = context.i32_type();
  let smoke_ty = i32.fn_type(&[], false);
  let smoke = module.add_function("thread_smoke", smoke_ty, None);
  let entry = context.append_basic_block(smoke, "entry");
  builder.position_at_end(entry);

  // `rt_thread_init(kind: u32)`; pass main-thread kind (0).
  let kind = context.i32_type().const_int(0, false);
  let _ = builder
    .build_call(fns.rt_thread_init, &[kind.into()], "")
    .expect("call rt_thread_init");
  let _ = builder
    .build_call(fns.rt_thread_deinit, &[], "")
    .expect("call rt_thread_deinit");
  builder
    .build_return(Some(&i32.const_int(0, false)))
    .expect("ret 0");

  let ir = module.print_to_string().to_string();

  // Compile + link a tiny C driver against runtime-native and our generated object.
  let td = tempfile::tempdir().expect("create tempdir");
  let ll_path = td.path().join("thread.ll");
  let thread_o = td.path().join("thread.o");
  let main_c = td.path().join("main.c");
  let main_o = td.path().join("main.o");
  let exe = td.path().join("smoke");

  std::fs::write(&ll_path, &ir).expect("write thread.ll");
  std::fs::write(
    &main_c,
    br#"
#include <stdint.h>

extern int32_t thread_smoke(void);

int main(void) { return thread_smoke(); }
"#,
  )
  .expect("write main.c");

  let out = Command::new(clang)
    .args(["-x", "ir", "-c"])
    .arg(&ll_path)
    .arg("-o")
    .arg(&thread_o)
    .output()
    .expect("compile thread.ll");
  assert!(
    out.status.success(),
    "clang failed to compile thread.ll:\nstdout:\n{}\nstderr:\n{}\nIR:\n{ir}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );

  let out = Command::new(clang)
    .arg("-std=c11")
    .arg("-c")
    .arg(&main_c)
    .arg("-o")
    .arg(&main_o)
    .output()
    .expect("compile main.c");
  assert!(
    out.status.success(),
    "clang failed to compile main.c:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );

  let out = Command::new(clang)
    .arg("-no-pie")
    .arg(&main_o)
    .arg(&thread_o)
    .arg(&runtime_native_a)
    .arg("-o")
    .arg(&exe)
    .output()
    .expect("link smoke binary");
  assert!(
    out.status.success(),
    "clang failed to link smoke binary:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );

  let mut child = Command::new(&exe)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("run smoke binary");

  let Some(status) = child.wait_timeout(Duration::from_secs(5)).unwrap() else {
    let _ = child.kill();
    let _ = child.wait();
    panic!("smoke binary timed out");
  };

  let mut stdout = String::new();
  if let Some(mut out) = child.stdout.take() {
    let _ = out.read_to_string(&mut stdout);
  }
  let mut stderr = String::new();
  if let Some(mut err) = child.stderr.take() {
    let _ = err.read_to_string(&mut stderr);
  }

  assert!(
    status.success(),
    "smoke binary failed (status={status:?})\nstdout:\n{}\nstderr:\n{}",
    stdout,
    stderr,
  );
}

