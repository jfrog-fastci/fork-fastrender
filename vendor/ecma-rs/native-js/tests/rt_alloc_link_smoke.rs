#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use inkwell::context::Context;
use inkwell::IntPredicate;
use native_js::runtime_abi::RuntimeAbi;
use std::io::Read;
use std::path::PathBuf;
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
      .is_ok()
    {
      return Some(candidate);
    }
  }
  None
}

#[test]
fn links_and_runs_rt_alloc_with_u32_shape_id_abi() {
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

  // Locate runtime-native's public C headers for the link-time smoke binary.
  let native_js_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let ecma_rs_root = native_js_dir
    .parent()
    .expect("native-js should be nested under vendor/ecma-rs");
  let include_dir = ecma_rs_root.join("runtime-native/include");
  assert!(
    include_dir.is_dir(),
    "missing runtime-native include dir: {}",
    include_dir.display()
  );

  // Build an LLVM module that:
  // - uses `RuntimeAbi` wrappers (so the test follows whatever ABI native-js believes),
  // - exposes an exported `alloc_smoke` function whose second parameter type matches
  //   `rt_alloc`'s shape-id type.
  //
  // The smoke binary's C `main` calls `alloc_smoke` using a `uint32_t` shape id,
  // and deliberately places non-zero junk in `%rdx` before the call. If native-js
  // ever regresses to using an `i128` shape-id ABI, the callee will observe the
  // high 64 bits from `%rdx` and fail the `shape == 1` check.
  let context = Context::create();
  let module = context.create_module("rt_alloc_link_smoke");
  let builder = context.create_builder();

  let wrappers = RuntimeAbi::new(&context, &module).ensure_wrappers();
  let rt_alloc = wrappers.rt_alloc;

  let params = rt_alloc.get_type().get_param_types();
  assert_eq!(params.len(), 2, "rt_alloc should take exactly 2 params");
  let shape_ty = params[1]
    .into_int_type();

  let alloc_smoke_ty = context
    .i32_type()
    .fn_type(&[params[0].into(), params[1].into()], false);
  let alloc_smoke = module.add_function("alloc_smoke", alloc_smoke_ty, None);

  let entry = context.append_basic_block(alloc_smoke, "entry");
  let ok_block = context.append_basic_block(alloc_smoke, "ok");
  let fail_block = context.append_basic_block(alloc_smoke, "fail");

  builder.position_at_end(entry);
  let size = alloc_smoke
    .get_nth_param(0)
    .expect("alloc_smoke size param")
    .into_int_value();
  let shape = alloc_smoke
    .get_nth_param(1)
    .expect("alloc_smoke shape param")
    .into_int_value();

  let one = shape_ty.const_int(1, false);
  let shape_is_one = builder
    .build_int_compare(IntPredicate::EQ, shape, one, "shape_is_one")
    .expect("build shape compare");
  builder
    .build_conditional_branch(shape_is_one, ok_block, fail_block)
    .expect("build branch");

  builder.position_at_end(fail_block);
  builder
    .build_return(Some(&context.i32_type().const_int(1, false)))
    .expect("return fail");

  builder.position_at_end(ok_block);
  let _ = builder
    .build_call(rt_alloc, &[size.into(), shape.into()], "obj")
    .expect("call rt_alloc");
  builder
    .build_return(Some(&context.i32_type().const_int(0, false)))
    .expect("return ok");

  let ir = module.print_to_string().to_string();

  // Compile + link a tiny C driver against runtime-native and our generated object.
  let td = tempfile::tempdir().expect("create tempdir");
  let ll_path = td.path().join("alloc.ll");
  let alloc_o = td.path().join("alloc.o");
  let main_c = td.path().join("main.c");
  let main_o = td.path().join("main.o");
  let exe = td.path().join("smoke");

  std::fs::write(&ll_path, &ir).expect("write alloc.ll");
  std::fs::write(
    &main_c,
    format!(
      r#"#include "runtime_native.h"

#include <stdint.h>

// `alloc_smoke` is implemented in LLVM IR (native-js) and calls `rt_alloc`.
extern int32_t alloc_smoke(uint64_t size, uint32_t shape);

int main(void) {{
  rt_thread_init(0);

  static const RtShapeDescriptor kShapes[1] = {{
    {{
      .size = 16,
      .align = 16,
      .flags = 0,
      .ptr_offsets = (const uint32_t*)0,
      .ptr_offsets_len = 0,
      .reserved = 0,
    }},
  }};
  rt_register_shape_table(kShapes, 1);

  // Force `%rdx` non-zero before the call. If `alloc_smoke` (and therefore
  // `rt_alloc`) ever takes an `i128` shape id, the high 64 bits will be read
  // from `%rdx` and the `shape == 1` check will fail.
  int32_t res;
  __asm__ __volatile__(
    "mov $0x1122334455667788, %%rdx\n\t"
    "call alloc_smoke\n\t"
    : "=a"(res)
    : "D"((uint64_t)16), "S"((uint64_t)1)
    : "rdx", "rcx", "r8", "r9", "r10", "r11", "memory"
  );

  rt_thread_deinit();
  return res;
}}
"#
    ),
  )
  .expect("write main.c");

  let out = Command::new(clang)
    .args(["-x", "ir", "-c"])
    .arg(&ll_path)
    .arg("-o")
    .arg(&alloc_o)
    .output()
    .expect("compile alloc.ll");
  assert!(
    out.status.success(),
    "clang failed to compile alloc.ll:\nstdout:\n{}\nstderr:\n{}\nIR:\n{ir}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );

  let out = Command::new(clang)
    .arg("-std=c11")
    .arg("-I")
    .arg(&include_dir)
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
    .arg(&alloc_o)
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
