use inkwell::context::Context;
use native_js::{codegen, strict};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileId, FileKey, MemoryHost, Program};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

fn export_def(program: &Program, file: FileId, name: &str) -> typecheck_ts::DefId {
  let exports = program.exports_of(file);
  let entry = exports
    .get(name)
    .unwrap_or_else(|| panic!("missing export `{name}`"));
  entry
    .def
    .or_else(|| program.symbol_info(entry.symbol).and_then(|info| info.def))
    .unwrap_or_else(|| panic!("failed to resolve export `{name}` to a DefId"))
}

fn function_block(ir: &str, func_name: &str) -> String {
  let mut out = Vec::new();
  let mut in_func = false;

  for line in ir.lines() {
    if !in_func && line.contains("define") && line.contains(func_name) {
      in_func = true;
    }

    if in_func {
      out.push(line);
      if line.trim() == "}" {
        break;
      }
    }
  }

  assert!(in_func, "function {func_name} not found in IR:\n{ir}");
  out.join("\n")
}

fn global_def_line(ir: &str, sym: &str) -> String {
  let needle = format!("@{sym} =");
  let line = ir
    .lines()
    .find(|line| line.contains(&needle))
    .unwrap_or_else(|| panic!("global `{sym}` not found in IR:\n{ir}"));
  line.to_string()
}

#[test]
fn hir_codegen_emits_number_globals_as_double() {
  let mut host = es5_host();

  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
export const n: number = 1;
export function main(): number {
  return n;
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  let file = program.file_id(&key).expect("file id");
  let entrypoint = strict::entrypoint(&program, file).expect("valid entrypoint");

  let n_def = export_def(&program, file, "n");
  let n_sym = native_js::llvm_symbol_for_def(&program, n_def);

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
  let line = global_def_line(&ir, &n_sym);
  assert!(line.contains("global") && line.contains("double"), "unexpected global line for `n`:\n{line}\n\nfull IR:\n{ir}");
}

#[test]
fn hir_codegen_registers_gc_pointer_module_globals_in_c_main() {
  let mut host = es5_host();

  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
// Create a module-level global with an object type (treated as a future GC pointer).
// The initializer is a type assertion; codegen currently ignores the wrapper and keeps the global
// null-initialized.
export const g = 0 as unknown as { x: number };

export function main(): number {
  return 0;
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  let file = program.file_id(&key).expect("file id");
  let entrypoint = strict::entrypoint(&program, file).expect("valid entrypoint");

  let g_def = export_def(&program, file, "g");
  let g_sym = native_js::llvm_symbol_for_def(&program, g_def);

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

  // The global slot must be typed as a GC-managed pointer.
  let g_line = global_def_line(&ir, &g_sym);
  assert!(
    g_line.contains("global") && g_line.contains("ptr addrspace(1)"),
    "expected `g` global to be stored as `ptr addrspace(1)`.\nline:\n{g_line}\n\nfull IR:\n{ir}"
  );

  // The C `main` wrapper must register the root slot before running any module initializers.
  assert!(
    ir.contains("declare void @rt_global_root_register"),
    "expected module to declare rt_global_root_register:\n{ir}"
  );

  let main_ir = function_block(&ir, "@main");
  let register_idx = main_ir
    .find("@rt_global_root_register")
    .expect("missing rt_global_root_register call in main wrapper");
  let init_idx = main_ir
    .find("__nativejs_file_init_")
    .expect("missing file init call in main wrapper");
  assert!(
    register_idx < init_idx,
    "expected global root registration to occur before module initializers.\nmain:\n{main_ir}"
  );
}
