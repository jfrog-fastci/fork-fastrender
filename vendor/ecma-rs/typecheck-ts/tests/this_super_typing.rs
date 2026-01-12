mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn instance_method_this_is_instance_type() {
  let source = r#"
class C {
  x: number = 1;
  m() {
    return this.x;
  }
}

const c = new C();
const y = c.m();
"#;
  let file = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let offset = source
    .find("this.x")
    .expect("this.x")
    .saturating_add("this.".len()) as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn static_method_this_is_static_type() {
  let source = r#"
class C {
  static x: number = 1;
  static m() {
    return this.x;
  }
}

const y = C.m();
"#;
  let file = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let offset = source
    .find("this.x")
    .expect("this.x")
    .saturating_add("this.".len()) as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn arrow_function_this_is_lexical() {
  let source = r#"
class C {
  x: number = 1;
  m() {
    const f = () => this.x;
    return f();
  }
}
"#;
  let file = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let offset = source
    .find("this.x")
    .expect("this.x")
    .saturating_add("this.".len()) as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_in_instance_member_is_base_instance_type() {
  let source = r#"
class Base {
  x: number = 1;
}

class Derived extends Base {
  m() {
    return super.x;
  }
}
"#;
  let file = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let offset = source
    .find("super.x")
    .expect("super.x")
    .saturating_add("super.".len()) as u32;
  let ty = program.type_at(file_id, offset).expect("type at super.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_in_static_member_is_base_static_type() {
  let source = r#"
class Base {
  static y: string = "a";
}

class Derived extends Base {
  static m() {
    return super.y;
  }
}
"#;
  let file = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let offset = source
    .find("super.y")
    .expect("super.y")
    .saturating_add("super.".len()) as u32;
  let ty = program.type_at(file_id, offset).expect("type at super.y");
  assert_eq!(program.display_type(ty).to_string(), "string");
}

#[test]
fn contextual_callback_this_param_is_used_in_body() {
  let source = r#"
declare function withThis(cb: (this: { x: number }) => number): number;

withThis(function() {
  return this.x;
});
"#;
  let file = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let offset = source
    .find("this.x")
    .expect("this.x")
    .saturating_add("this.".len()) as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn explicit_this_param_in_function_expression_is_used_in_body() {
  let source = r#"
declare function takes(cb: (this: { x: number }) => number): number;

takes(function(this: { x: number }) {
  return this.x;
});
"#;
  let file = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let offset = source
    .find("this.x")
    .expect("this.x")
    .saturating_add("this.".len()) as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}
