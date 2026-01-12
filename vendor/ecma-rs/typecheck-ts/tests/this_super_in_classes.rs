mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn this_in_instance_method_is_instance_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let src = r#"class C { x: number = 1; m() { return this.x; } }"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("this.x").expect("this.x offset") as u32 + "this.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn this_in_static_method_is_constructor_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let src = r#"class C { static sx: number = 1; static sm() { return this.sx; } }"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("this.sx").expect("this.sx offset") as u32 + "this.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.sx");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_in_derived_instance_method_is_base_instance_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let src = r#"
class B { foo(): number { return 1; } }
class C extends B { bar() { return super.foo(); } }
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("super.foo(").expect("super.foo call offset") as u32 + "super.foo".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at super.foo()");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn arrow_captures_this_from_enclosing_method() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let src = r#"class C { x: number = 1; m() { const f = () => this.x; return f(); } }"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("this.x").expect("this.x offset") as u32 + "this.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x inside arrow");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn arrow_captures_super_from_enclosing_method_for_flow_narrowing() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let src = r#"
class B {
  isString(x: string | number): x is string {
    return typeof x === "string";
  }
}

class C extends B {
  m(x: string | number) {
    const f = () => {
      if (super.isString(x)) {
        return x;
      }
      return x;
    };
    return f();
  }
}
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src
    .find("return x;")
    .expect("return x offset") as u32
    + "return ".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at narrowed x inside arrow");
  assert_eq!(program.display_type(ty).to_string(), "string");
}
