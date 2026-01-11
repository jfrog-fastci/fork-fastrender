use inkwell::context::Context;

use native_js::llvm::LlvmBackend;
use native_js::CompileOptions;

#[test]
fn emits_valid_ir() {
  native_js::llvm::init_native_target().unwrap();

  let context = Context::create();
  let backend = LlvmBackend::new(&context, "test", &CompileOptions::default()).unwrap();

  let i32_type = context.i32_type();
  let fn_type = i32_type.fn_type(&[], false);
  let function = backend.add_function("main", fn_type, None);
  let entry = backend.append_basic_block(function, "entry");

  backend.builder.position_at_end(entry);
  backend
    .builder
    .build_return(Some(&i32_type.const_int(0, false)))
    .unwrap();

  backend.verify().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn emits_object_file() {
  native_js::llvm::init_native_target().unwrap();

  let context = Context::create();
  let backend = LlvmBackend::new(&context, "test", &CompileOptions::default()).unwrap();

  let i32_type = context.i32_type();
  let fn_type = i32_type.fn_type(&[], false);
  let function = backend.add_function("main", fn_type, None);
  let entry = backend.append_basic_block(function, "entry");

  backend.builder.position_at_end(entry);
  backend
    .builder
    .build_return(Some(&i32_type.const_int(0, false)))
    .unwrap();

  backend.verify().unwrap();

  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("test.o");
  backend.emit_object(&path).unwrap();

  let metadata = std::fs::metadata(&path).unwrap();
  assert!(metadata.len() > 0, "expected emitted object file to be non-empty");
}
