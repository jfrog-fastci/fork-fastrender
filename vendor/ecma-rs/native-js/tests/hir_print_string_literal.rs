#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use inkwell::context::Context;
use native_js::{codegen, strict, validate};
use std::process::{Command, Stdio};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
    {
      return Some(cand);
    }
  }
  None
}

#[test]
fn hir_print_string_literal_prints_bytes() {
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

  let source = r#"
export function main(): number {
  print("hello");
  return 0;
}
"#;

  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  host.add_lib(native_js::builtins::checked_builtins_lib());

  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  validate::validate_strict_subset(&program).expect("strict-subset validation");

  let file = program.file_id(&key).expect("file id");
  let entrypoint = strict::entrypoint(&program, file).expect("valid entrypoint");

  let context = Context::create();
  let module = codegen::codegen(
    &context,
    &program,
    file,
    entrypoint,
    codegen::CodegenOptions::default(),
  )
  .expect("codegen");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("rt_string_lookup_pinned"),
    "expected IR to reference rt_string_lookup_pinned:\n{ir}"
  );

  let td = tempfile::tempdir().expect("tempdir");
  let ll_path = td.path().join("out.ll");
  std::fs::write(&ll_path, &ir).expect("write ir");

  let obj_path = td.path().join("out.o");
  let exe_path = td.path().join("out");
  let status = Command::new(clang)
    .arg("-Wno-override-module")
    .args(["-x", "ir", "-c"])
    .arg(&ll_path)
    .arg("-O0")
    .arg("-o")
    .arg(&obj_path)
    .status()
    .expect("clang compile");
  assert!(status.success(), "clang failed to compile IR with {status}");

  let status = Command::new(clang)
    .arg("-no-pie")
    .arg(&obj_path)
    .arg(&runtime_native_a)
    .arg("-o")
    .arg(&exe_path)
    .status()
    .expect("clang link");
  assert!(status.success(), "clang failed with {status}");

  let out = Command::new(&exe_path).output().expect("run exe");
  assert_eq!(out.status.code(), Some(0));
  assert_eq!(out.stdout, b"hello\n");
}

