use std::collections::HashMap;
use std::sync::Arc;

mod common;

use typecheck_ts::codes;
use typecheck_ts::lib_support::{CompilerOptions, FileKind, JsxMode, LibFile};
use typecheck_ts::{FileKey, Host, HostError, Program, TypeKindSummary};

const JSX_LIB: &str = r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: any }
  interface IntrinsicElements {
    div: { id?: string; children?: string };
    span: { children?: string };

    "Svg:Path": {};
    "my-tag": {};
    "My-Tag": {};
  }
}
"#;

#[derive(Default)]
struct TestHost {
  files: HashMap<FileKey, Arc<str>>,
  options: CompilerOptions,
  libs: Vec<LibFile>,
  edges: HashMap<(FileKey, String), FileKey>,
}

impl TestHost {
  fn new(options: CompilerOptions) -> Self {
    let mut libs = Vec::new();
    if options.no_default_lib {
      libs.push(common::core_globals_lib());
    }
    TestHost {
      files: HashMap::new(),
      options,
      libs,
      edges: HashMap::new(),
    }
  }

  fn with_file(mut self, key: FileKey, text: &str) -> Self {
    self.files.insert(key, Arc::from(text));
    self
  }

  fn with_lib(mut self, lib: LibFile) -> Self {
    self.libs.push(lib);
    self
  }

  fn link(mut self, from: FileKey, spec: &str, to: FileKey) -> Self {
    self.edges.insert((from, spec.to_string()), to);
    self
  }
}

impl Host for TestHost {
  fn file_text(&self, file: &FileKey) -> Result<Arc<str>, HostError> {
    self
      .files
      .get(file)
      .cloned()
      .ok_or_else(|| HostError::new(format!("missing file {file:?}")))
  }

  fn resolve(&self, from: &FileKey, specifier: &str) -> Option<FileKey> {
    self
      .edges
      .get(&(from.clone(), specifier.to_string()))
      .cloned()
  }

  fn compiler_options(&self) -> CompilerOptions {
    self.options.clone()
  }

  fn lib_files(&self) -> Vec<LibFile> {
    self.libs.clone()
  }

  fn file_kind(&self, file: &FileKey) -> FileKind {
    if let Some(lib) = self.libs.iter().find(|l| &l.key == file) {
      return lib.kind;
    }
    if file.as_str().ends_with(".tsx") {
      FileKind::Tsx
    } else {
      FileKind::Ts
    }
  }
}

fn jsx_lib_file() -> LibFile {
  LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(JSX_LIB),
  }
}

fn empty_lib_file() -> LibFile {
  LibFile {
    key: FileKey::new("empty.d.ts"),
    name: Arc::from("empty.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(""),
  }
}

#[test]
fn jsx_requires_compiler_option() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = None;

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), "const el = <div />;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::JSX_DISABLED.as_str(),
    "expected JSX_DISABLED, got {diagnostics:?}"
  );
}

#[test]
fn jsx_namespace_missing_emits_diagnostic() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(empty_lib_file())
    .with_file(entry.clone(), "const el = <div />;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::JSX_NAMESPACE_MISSING.as_str(),
    "expected JSX_NAMESPACE_MISSING, got {diagnostics:?}"
  );
}

#[test]
fn react_jsx_runtime_module_missing_emits_ts2875() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::ReactJsx);
  options.jsx_import_source = Some("preact".to_string());

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), "const el = <div />;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::JSX_RUNTIME_MODULE_MISSING.as_str(),
    "expected JSX_RUNTIME_MODULE_MISSING, got {diagnostics:?}"
  );
  assert!(
    diagnostics[0].message.contains("preact/jsx-runtime"),
    "expected TS2875 message to mention preact/jsx-runtime, got {diagnostics:?}"
  );
}

#[test]
fn react_jsx_does_not_report_ts2875_when_runtime_module_exists() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::ReactJsx);
  options.jsx_import_source = Some("preact".to_string());

  let entry = FileKey::new("entry.tsx");
  let runtime = FileKey::new("preact_jsx_runtime.ts");
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(runtime.clone(), "export {};")
    .with_file(entry.clone(), "const el = <div />;")
    .link(entry.clone(), "preact/jsx-runtime", runtime);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .all(|diag| diag.code.as_str() != codes::JSX_RUNTIME_MODULE_MISSING.as_str()),
    "did not expect TS2875 when runtime module is available, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_props_checked() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const ok = <div id="x">hi</div>;
const ok2 = <div {...{ id: "y", children: "yo" }} />;
const bad = <div id={123} />;
const bad2 = <div {...{ id: 123 }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected a type mismatch diagnostic, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "did not expect unknown identifiers, got {diagnostics:?}"
  );
}

#[test]
fn jsx_element_type_constraint_rejects_intrinsic_tag() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let lib = r#"
declare namespace JSX {
  interface Element {}
  type ElementType = "div";
  interface IntrinsicElements {
    div: {};
    span: {};
  }
}
"#;

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(LibFile {
      key: FileKey::new("jsx.d.ts"),
      name: Arc::from("jsx.d.ts"),
      kind: FileKind::Dts,
      text: Arc::from(lib),
    })
    .with_file(entry.clone(), "const ok = <div />; const bad = <span />;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_INVALID_ELEMENT_TYPE.as_str()),
    "expected TS18053 diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_attribute_values_are_contextually_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface IntrinsicElements {
    div: { onClick?: (ev: { x: number }) => void };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 <div onClick={(ev) => { const n: number = ev.x; }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed intrinsic attrs, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_attribute_values_are_contextually_typed_for_call_signature_types() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ClickHandler {
    (ev: { x: number }): void;
  }
  interface IntrinsicElements {
    div: { onClick?: ClickHandler };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 <div onClick={(ev) => { const n: number = ev.x; }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed call-signature attrs, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_attribute_values_are_contextually_typed_for_type_aliases() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  type ClickHandler = (ev: { x: number }) => void;
  interface IntrinsicElements {
    div: { onClick?: ClickHandler };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 <div onClick={(ev) => { const n: number = ev.x; }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed type-alias attrs, got {diagnostics:?}"
  );
}

#[test]
fn value_tag_intrinsic_attribute_values_are_contextually_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface IntrinsicElements {
    div: { onClick?: (ev: { x: number }) => void };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 const Tag: "div" = "div";
 <Tag onClick={(ev) => { const n: number = ev.x; }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed value tags, got {diagnostics:?}"
  );
}

#[test]
fn member_value_tag_intrinsic_attribute_values_are_contextually_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface IntrinsicElements {
    div: { onClick?: (ev: { x: number }) => void };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 const Tags: { Div: "div" } = { Div: "div" };
 <Tags.Div onClick={(ev) => { const n: number = ev.x; }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed member value tags, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_children_are_contextually_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
 declare namespace JSX {
   interface Element {}
   interface ElementChildrenAttribute { children: any }
   interface IntrinsicElements {
     div: { children?: (ev: { x: number }) => void };
   }
 }
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 <div>{(ev) => { const n: number = ev.x; }}</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed intrinsic children, got {diagnostics:?}"
  );
}

#[test]
fn jsx_spread_children_invalid_type_emits_ts2609() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"const el = <div>{...123}</div>;"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_SPREAD_CHILD_MUST_BE_ARRAY.as_str()),
    "expected TS2609 diagnostic for invalid spread child, got {diagnostics:?}"
  );
}

#[test]
fn jsx_spread_children_array_does_not_emit_ts2609() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"const el = <div>{...["ok"]}</div>;"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_SPREAD_CHILD_MUST_BE_ARRAY.as_str()),
    "did not expect TS2609 diagnostic for array spread child, got {diagnostics:?}"
  );
}

#[test]
fn jsx_spread_children_any_bypasses_ts2609() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"const el = <div>{...(123 as any)}</div>;"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_SPREAD_CHILD_MUST_BE_ARRAY.as_str()),
    "did not expect TS2609 diagnostic for `any` spread child, got {diagnostics:?}"
  );
}

#[test]
fn component_attribute_values_are_contextually_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
 declare namespace JSX {
   interface Element {}
   interface ElementChildrenAttribute { children: any }
 }
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 function Foo(props: { onClick?: (ev: { x: number }) => void }): JSX.Element { return null as any; }
 <Foo onClick={(ev) => { const n: number = ev.x; }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed component attrs, got {diagnostics:?}"
  );
}

#[test]
fn jsx_discriminated_union_props_contextual_typing() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
type Props =
  | { kind: "a"; onClick: (ev: { x: number }) => void }
  | { kind: "b"; onClick: (ev: { y: string }) => void };
function Foo(props: Props): JSX.Element { return null as any; }
<Foo kind="a" onClick={(ev) => { const n: number = ev.x; }} />;
<Foo kind="b" onClick={(ev) => { const s: string = ev.y; }} />;
"#;
  let host = TestHost::new(options.clone())
    .with_lib(jsx.clone())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry.clone()]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for discriminated union JSX props, got {diagnostics:?}"
  );

  let negative = r#"
type Props =
  | { kind: "a"; onClick: (ev: { x: number }) => void }
  | { kind: "b"; onClick: (ev: { y: string }) => void };
function Foo(props: Props): JSX.Element { return null as any; }
<Foo kind="a" onClick={(ev) => { const s = ev.y; }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), negative);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::PROPERTY_DOES_NOT_EXIST.as_str()),
    "expected PROPERTY_DOES_NOT_EXIST (TS2339) for wrong discriminated union JSX prop usage, got {diagnostics:?}"
  );
}

#[test]
fn jsx_discriminated_union_children_contextual_typing() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: any }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
type Props =
  | { kind: "a"; children: (ev: { x: number }) => void }
  | { kind: "b"; children: (ev: { y: string }) => void };
function Foo(props: Props): JSX.Element { return null as any; }
<Foo kind="a">{(ev) => { const n: number = ev.x; }}</Foo>;
<Foo kind="b">{(ev) => { const s: string = ev.y; }}</Foo>;
"#;
  let host = TestHost::new(options.clone())
    .with_lib(jsx.clone())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry.clone()]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for discriminated union JSX children, got {diagnostics:?}"
  );

  let negative = r#"
type Props =
  | { kind: "a"; children: (ev: { x: number }) => void }
  | { kind: "b"; children: (ev: { y: string }) => void };
function Foo(props: Props): JSX.Element { return null as any; }
<Foo kind="a">{(ev) => { const s = ev.y; }}</Foo>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), negative);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::PROPERTY_DOES_NOT_EXIST.as_str()),
    "expected PROPERTY_DOES_NOT_EXIST (TS2339) for wrong discriminated union JSX children usage, got {diagnostics:?}"
  );
}

#[test]
fn component_children_are_contextually_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: any }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 function Foo(props: { children?: (ev: { x: number }) => void }): JSX.Element { return null as any; }
 <Foo>{(ev) => { const n: number = ev.x; }}</Foo>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed component children, got {diagnostics:?}"
  );
}

#[test]
fn spread_attributes_are_contextually_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: any }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 function Foo(props: { onClick?: (ev: { x: number }) => void }): JSX.Element { return null as any; }
 <Foo {...{ onClick: (ev) => { const n: number = ev.x; } }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed spread attrs, got {diagnostics:?}"
  );
}

#[test]
fn jsx_spread_attr_invalid_type_emits_ts2698() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), "const el = <div {...1} />;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_SPREAD_ATTR_MUST_BE_OBJECT.as_str()),
    "expected TS2698, got {diagnostics:?}"
  );
}

#[test]
fn jsx_spread_attr_object_type_is_allowed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), "const el = <div {...{}} />;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_SPREAD_ATTR_MUST_BE_OBJECT.as_str()),
    "did not expect TS2698, got {diagnostics:?}"
  );
}

#[test]
fn jsx_spread_attr_any_is_allowed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), "const el = <div {...(1 as any)} />;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_SPREAD_ATTR_MUST_BE_OBJECT.as_str()),
    "did not expect TS2698, got {diagnostics:?}"
  );
}

#[test]
fn component_props_checked_for_imported_component_and_imported_value_used_only_in_jsx() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let component = FileKey::new("component.ts");
  let values = FileKey::new("values.ts");
  let main = FileKey::new("main.tsx");

  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(
      component.clone(),
      r#"export function Foo(props: { x: number; children?: string }): JSX.Element { return null as any; }"#,
    )
    .with_file(values.clone(), "export const x: number = 1;")
    .with_file(
      main.clone(),
      r#"
import { Foo } from "./component";
import { x } from "./values";
const ok = <Foo x={x}>hi</Foo>;
const ok2 = <Foo {...{ x }}>hi</Foo>;
const bad = <Foo x={"no"} />;
"#,
    )
    .link(main.clone(), "./component", component)
    .link(main.clone(), "./values", values);

  let program = Program::new(host, vec![main]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "did not expect unknown identifiers, got {diagnostics:?}"
  );
  assert!(
    diagnostics.iter().any(|d| {
      d.code.as_str() == codes::TYPE_MISMATCH.as_str()
        || d.code.as_str() == codes::NO_OVERLOAD.as_str()
    }),
    "expected a prop type error for bad JSX usage, got {diagnostics:?}"
  );
}

#[test]
fn nested_jsx_child_elements_record_types_for_type_at() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = "const el = <div><span /></div>;";
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry.clone()]);
  let file_id = program.file_id(&entry).expect("file id");

  let offset = source.find("<span").expect("span tag") as u32 + 1;
  let ty = program.type_at(file_id, offset).expect("type at <span>");
  assert_ne!(
    program.type_kind(ty),
    TypeKindSummary::Unknown,
    "expected nested JSX element to have a non-unknown type"
  );
}

#[test]
fn jsx_empty_expression_container_is_ignored() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"const el = <div>{/* comment */}</div>;"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics for JSX empty expression container, got {diagnostics:?}"
  );
}

#[test]
fn jsx_spread_attrs_are_merged_before_props_check() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let component = FileKey::new("component.ts");
  let main = FileKey::new("main.tsx");

  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(
      component.clone(),
      r#"export function Foo(props: { x: number }) { return null as any; }"#,
    )
    .with_file(
      main.clone(),
      r#"
import { Foo } from "./component";
const ok = <Foo {...{ x: 1 }} {...{}} />;
"#,
    )
    .link(main.clone(), "./component", component);

  let program = Program::new(host, vec![main]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics for merged spreads, got {diagnostics:?}"
  );
}

#[test]
fn jsx_spread_children_are_checked_against_children_prop() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"const el = <div>{...[1]}</div>;"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected a type mismatch diagnostic for spread children, got {diagnostics:?}"
  );
}

#[test]
fn spread_children_are_contextually_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
 declare namespace JSX {
   interface Element {}
   interface ElementChildrenAttribute { children: any }
   interface IntrinsicElements {
     div: { children?: ((ev: { x: number }) => void)[] };
   }
 }
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 <div>{...[(ev) => { const n: number = ev.x; }]}</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for contextually typed spread children, got {diagnostics:?}"
  );
}

#[test]
fn multiple_jsx_children_become_array() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const ok = <div>hi</div>;
const bad = <div>{"a"}{"b"}</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics
      .iter()
      .filter(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str())
      .count(),
    1,
    "expected exactly one type mismatch diagnostic for multiple children, got {diagnostics:?}"
  );
}

#[test]
fn jsx_text_children_are_not_string_literals() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: "hi" };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const el = <div>hi</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected a type mismatch diagnostic for text children vs string-literal prop, got {diagnostics:?}"
  );
}

#[test]
fn tuple_children_pass() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: [number, string] };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const ok = <div>{1}{"x"}</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics for tuple-typed children, got {diagnostics:?}"
  );
}

#[test]
fn tuple_children_fail() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: [number, string] };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const bad = <div>{"x"}{1}</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected at least one type mismatch diagnostic for mis-ordered tuple children, got {diagnostics:?}"
  );
}

#[test]
fn tuple_children_contextual_typing() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: {
      children?: [
        (ev: { x: number }) => void,
        (ev: { y: string }) => void
      ];
    };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
<div>
  {(ev) => { const n: number = ev.x; }}
  {(ev) => { const s: string = ev.y; }}
</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for per-index contextually typed tuple children, got {diagnostics:?}"
  );
}

#[test]
fn tuple_children_rest_pass() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: [number, ...string[]] };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const ok = <div>{1}{"a"}{"b"}</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics for variadic tuple children, got {diagnostics:?}"
  );
}

#[test]
fn tuple_children_rest_fail() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: [number, ...string[]] };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const bad = <div>{1}{2}</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected a type mismatch diagnostic for variadic tuple children, got {diagnostics:?}"
  );
}

#[test]
fn tuple_children_rest_contextual_typing() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);
  options.no_implicit_any = true;

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: {
      children?: [
        (n: number) => void,
        ...((ev: { x: number }) => void)[]
      ];
    };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
<div>
  {(n) => { const x: number = n; }}
  {(ev) => { const x: number = ev.x; }}
  {(ev) => { const x: number = ev.x; }}
</div>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (including implicit any) for variadic tuple children contextual typing, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_namespaced_and_hyphenated_tags_are_not_value_identifiers() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const a = <Svg:Path />;
const b = <my-tag />;
const c = <My-Tag />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "did not expect unknown identifiers, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|d| { d.code.as_str() == codes::JSX_UNKNOWN_INTRINSIC_ELEMENT.as_str() }),
    "did not expect unknown intrinsic elements, got {diagnostics:?}"
  );
}

#[test]
fn unknown_intrinsic_emits_diagnostic() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), "const el = <bogus />;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_UNKNOWN_INTRINSIC_ELEMENT.as_str()),
    "expected JSX_UNKNOWN_INTRINSIC_ELEMENT, got {diagnostics:?}"
  );
}

#[test]
fn component_member_tags_seed_base_identifier() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let component = FileKey::new("component.ts");
  let main = FileKey::new("main.tsx");

  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(
      component.clone(),
      r#"export const Foo = { Bar: (props: { x: number }): JSX.Element => null as any };"#,
    )
    .with_file(
      main.clone(),
      r#"
import { Foo } from "./component";
const ok = <Foo.Bar x={1} />;
"#,
    )
    .link(main.clone(), "./component", component);

  let program = Program::new(host, vec![main]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "did not expect unknown identifiers, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_excess_props_are_reported() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), r#"const bad = <div foo="x" />;"#);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::EXCESS_PROPERTY.as_str()),
    "expected an excess property diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn hyphenated_jsx_attributes_are_not_excess_props() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface IntrinsicElements {
    div: {};
  }
  interface ElementChildrenAttribute {
    children: {};
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), r#"const el = <div data-foo="x" />;"#);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::EXCESS_PROPERTY.as_str()),
    "did not expect an excess property diagnostic for hyphenated JSX attrs, got {diagnostics:?}"
  );
}

#[test]
fn component_excess_props_with_spread_are_reported() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo(props: { x: number }): JSX.Element { return null as any; }
const ok = <Foo {...{ x: 1 }} />;
const bad = <Foo {...{ x: 1 }} y={1} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::EXCESS_PROPERTY.as_str(),
    "expected EXCESS_PROPERTY, got {diagnostics:?}"
  );
}

#[test]
fn component_excess_props_with_any_spread_are_not_reported() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo(props: { x: number }): JSX.Element { return null as any; }
const ok = <Foo {...({ x: 1 } as any)} />;
const bad = <Foo {...({ x: 1 } as any)} y={1} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics (any spread makes props `any`), got {diagnostics:?}"
  );
}

#[test]
fn component_attribute_object_literal_excess_props_are_reported() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
 declare namespace JSX {
   interface Element {}
   interface ElementChildrenAttribute { children: any }
 }
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo(props: { style?: { foo: number } }): JSX.Element { return null as any; }
const ok = <Foo style={{ foo: 1 }} />;
const bad = <Foo style={{ foo: 1, bar: 2 }} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::EXCESS_PROPERTY.as_str(),
    "expected excess property diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn component_children_object_literal_excess_props_are_reported() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: any }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo(props: { children?: { foo: number } }): JSX.Element { return null as any; }
const ok = <Foo>{({ foo: 1 })}</Foo>;
const bad = <Foo>{({ foo: 1, bar: 2 })}</Foo>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::EXCESS_PROPERTY.as_str(),
    "expected excess property diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn component_without_props_param_allows_empty_props() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo() { return null as any; }
const ok = <Foo />;
const bad = <Foo x={1} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::EXCESS_PROPERTY.as_str(),
    "expected EXCESS_PROPERTY, got {diagnostics:?}"
  );
}

#[test]
fn component_return_type_must_be_valid_jsx_element() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element { readonly __tag: "jsx" }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo(): string { return "hi"; }
function Bar(): JSX.Element | null { return null; }
const ok = <Bar />;
const bad = <Foo />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::NO_OVERLOAD.as_str(),
    "expected NO_OVERLOAD, got {diagnostics:?}"
  );
}

#[test]
fn element_children_attribute_controls_children_prop_name() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { kid: {} }
  interface IntrinsicElements {
    div: { kid?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = "const el = <div>hi</div>;";
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics for ElementChildrenAttribute, got {diagnostics:?}"
  );
}

#[test]
fn element_children_attribute_with_multiple_properties_emits_ts2608() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.skip_lib_check = false;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { a: {}; b: {} }
  interface IntrinsicElements {
    div: { children?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = "const el = <div>hi</div>;";
  let host = TestHost::new(options)
    .with_lib(jsx.clone())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.iter().any(|d| {
      d.code.as_str() == codes::JSX_GLOBAL_TYPE_MAY_NOT_HAVE_MORE_THAN_ONE_PROPERTY.as_str()
    }),
    "expected TS2608 for invalid ElementChildrenAttribute, got {diagnostics:?}"
  );

  // tsc suppresses TS2608 when it originates from `.d.ts` files under
  // `skipLibCheck: true`.
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.skip_lib_check = true;
  options.jsx = Some(JsxMode::React);
  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options).with_lib(jsx).with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected TS2608 to be suppressed under skipLibCheck, got {diagnostics:?}"
  );
}

#[test]
fn empty_element_children_attribute_disables_children_prop_injection() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute {}
  interface IntrinsicElements {
    div: { children?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = "const ok = <div>{123}</div>;";
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics for empty ElementChildrenAttribute, got {diagnostics:?}"
  );
}

#[test]
fn element_children_attribute_is_ignored_in_react_jsx() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::ReactJsx);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { offspring: {} }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
 declare function Title(props: { children: string }): JSX.Element;
 declare function Wrong(props: { offspring: string }): JSX.Element;
 const ok = <Title>Hello</Title>;
 const bad = <Wrong>Byebye</Wrong>;
 "#;
  let runtime = FileKey::new("react_jsx_runtime.ts");
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(runtime.clone(), "export {};")
    .with_file(entry.clone(), source)
    .link(entry.clone(), "react/jsx-runtime", runtime);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::MISSING_REQUIRED_PROPERTY.as_str(),
    "expected MISSING_REQUIRED_PROPERTY for `offspring` prop, got {diagnostics:?}"
  );
}

#[test]
fn jsx_children_specified_twice_emits_diagnostic() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"const el = <div children="x">y</div>;"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::JSX_CHILDREN_SPECIFIED_TWICE.as_str()),
    "expected TS2710 diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn jsx_children_specified_twice_ignores_spread_children_attribute() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementChildrenAttribute { children: {} }
  interface IntrinsicElements {
    div: { children?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"const el = <div {...{ children: "x" }}>y</div>;"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics for spread-only children attribute, got {diagnostics:?}"
  );
}

#[test]
fn qualified_jsx_element_return_type_is_resolved() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo(): JSX.Element { return null as any; }
const ok = <Foo />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics, got {diagnostics:?}"
  );
}

#[test]
fn generic_component_respects_type_param_constraints() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo<T extends number>(props: { x: T }): JSX.Element { return null as any; }
const ok = <Foo x={1} />;
const bad = <Foo x={"no"} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected a type mismatch diagnostic for constrained generic props, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_attributes_are_merged_into_expected_props() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface IntrinsicAttributes { key?: string }
  interface IntrinsicElements {
    div: { id?: string };
  }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo(props: { x: number }): JSX.Element { return null as any; }
const ok = <div key="x" id="y" />;
const ok2 = <Foo x={1} key="k" />;
const bad = <div key={123} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics
      .iter()
      .filter(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str())
      .count(),
    1,
    "expected exactly one type mismatch diagnostic, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::EXCESS_PROPERTY.as_str()),
    "did not expect excess property diagnostics, got {diagnostics:?}"
  );
}

#[test]
fn intrinsic_class_attributes_apply_to_construct_signatures() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface IntrinsicAttributes { key?: string }
  interface IntrinsicClassAttributes<T> { ref?: T }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
interface FooInstance { readonly _tag: "Foo" }
declare const Foo: { new (props: { x: number }): FooInstance };
declare const inst: FooInstance;
const ok = <Foo x={1} ref={inst} key="k" />;
const bad = <Foo x={1} ref={123} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected a type mismatch diagnostic for bad ref type, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::EXCESS_PROPERTY.as_str()),
    "did not expect excess property diagnostics, got {diagnostics:?}"
  );
}

#[test]
fn element_attributes_property_controls_class_component_props() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementAttributesProperty { props: {} }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
interface FooProps { x: number }
interface FooInstance { props: FooProps }
declare const Foo: { new (): FooInstance };
const ok = <Foo x={1} />;
const bad = <Foo y={1} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::MISSING_REQUIRED_PROPERTY.as_str(),
    "expected missing required property diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn element_attributes_property_empty_uses_instance_type() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementAttributesProperty {}
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
declare const Foo: { new(): { x: number } };
const ok = <Foo x={1} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics for empty ElementAttributesProperty (instance type is props), got {diagnostics:?}"
  );
}

#[test]
fn element_attributes_property_multiple_properties_errors() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.skip_lib_check = false;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementAttributesProperty { a: {}; b: {} }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
declare const Foo: { new(): {} };
const el = <Foo />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.iter().any(|d| {
      d.code.as_str() == codes::JSX_GLOBAL_TYPE_MAY_NOT_HAVE_MORE_THAN_ONE_PROPERTY.as_str()
    }),
    "expected TS2608 for ElementAttributesProperty with multiple properties, got {diagnostics:?}"
  );
}

#[test]
fn missing_required_props_member_errors_ts2607() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementAttributesProperty { props: {} }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
declare const Foo: { new(): {} };
const el = <Foo x={1} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::JSX_ELEMENT_CLASS_DOES_NOT_SUPPORT_ATTRIBUTES.as_str(),
    "expected TS2607, got {diagnostics:?}"
  );
}

#[test]
fn missing_required_props_member_allows_children_without_error() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementAttributesProperty { props: {} }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
declare const Foo: { new(): {} };
const el = <Foo>hi</Foo>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics when props member is missing but JSX uses only children (no explicit attrs), got {diagnostics:?}"
  );
}

#[test]
fn library_managed_attributes_are_applied_to_component_props() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  type LibraryManagedAttributes<C, P> = P & { managed?: string };
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
function Foo(props: { x: number }): JSX.Element { return null as any; }
const ok = <Foo x={1} managed="yes" />;
const bad = <Foo x={1} managed={123} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::EXCESS_PROPERTY.as_str()),
    "did not expect excess property diagnostics, got {diagnostics:?}"
  );
  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected a type mismatch diagnostic for managed, got {diagnostics:?}"
  );
}

#[test]
fn element_class_filters_invalid_class_components() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
  interface ElementClass { render(): Element }
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
interface FooInstance { props: { x: number } }
declare const Foo: { new (props: { x: number }): FooInstance };
const el = <Foo x={1} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::NO_OVERLOAD.as_str(),
    "expected NO_OVERLOAD, got {diagnostics:?}"
  );
}

#[test]
fn value_tag_string_literal_is_treated_as_intrinsic_element() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
const Tag = "div";
const ok = <Tag id="x">hi</Tag>;
const bad = <Tag foo="x" />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::EXCESS_PROPERTY.as_str(),
    "expected EXCESS_PROPERTY, got {diagnostics:?}"
  );
}

#[test]
fn value_tag_union_of_string_literals_requires_common_props() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
declare const cond: boolean;
const Tag = cond ? "div" : "span";
const ok = <Tag children="hi" />;
const bad = <Tag id="x" />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::EXCESS_PROPERTY.as_str(),
    "expected EXCESS_PROPERTY, got {diagnostics:?}"
  );
}

#[test]
fn value_tag_union_of_components_requires_common_props() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let entry = FileKey::new("entry.tsx");
  let source = r#"
declare const cond: boolean;
function Foo(props: { x: number }): JSX.Element { return null as any; }
function Bar(props: { x: number; y?: number }): JSX.Element { return null as any; }
const Comp = cond ? Foo : Bar;
const ok = <Comp x={1} />;
const bad = <Comp x={1} y={2} />;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx_lib_file())
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected exactly one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::EXCESS_PROPERTY.as_str(),
    "expected EXCESS_PROPERTY, got {diagnostics:?}"
  );
}

#[test]
fn fragment_in_react_mode_requires_react_binding() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), "const el = <></>;");
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    2,
    "expected two diagnostics, got {diagnostics:?}"
  );
  let codes: Vec<_> = diagnostics.iter().map(|d| d.code.as_str()).collect();
  assert!(
    codes.contains(&codes::JSX_FACTORY_MISSING.as_str()),
    "expected JSX_FACTORY_MISSING, got {diagnostics:?}"
  );
  assert!(
    codes.contains(&codes::JSX_FRAGMENT_FACTORY_MISSING.as_str()),
    "expected JSX_FRAGMENT_FACTORY_MISSING, got {diagnostics:?}"
  );
}

#[test]
fn fragment_children_are_checked_against_fragment_props_when_react_fragment_typed() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.jsx = Some(JsxMode::React);

  let jsx = LibFile {
    key: FileKey::new("jsx.d.ts"),
    name: Arc::from("jsx.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
declare namespace JSX {
  interface Element {}
}
"#,
    ),
  };

  let entry = FileKey::new("entry.tsx");
  let source = r#"
declare const React: { Fragment: (props: { children?: string }) => JSX.Element };
const el = <>{123}</>;
"#;
  let host = TestHost::new(options)
    .with_lib(jsx)
    .with_file(entry.clone(), source);
  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics[0].code.as_str(),
    codes::TYPE_MISMATCH.as_str(),
    "expected TYPE_MISMATCH, got {diagnostics:?}"
  );
}
