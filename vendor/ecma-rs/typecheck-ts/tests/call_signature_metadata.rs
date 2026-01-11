use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::FileId;
use hir_js::{lower_from_source, BodyKind};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use typecheck_ts::check::caches::CheckerCaches;
use typecheck_ts::check::hir_body::{check_body, AstIndex};
use typecheck_ts::lib_support::ScriptTarget;
use typecheck_ts::{FileKey, MemoryHost, Program};
use types_ts_interned::{
  ObjectType, Param, PropData, PropKey, Property, Shape, Signature, SignatureId, TypeId, TypeKind,
  TypeStore,
};

fn top_level_body<'a>(lowered: &'a hir_js::LowerResult) -> (hir_js::BodyId, &'a hir_js::Body) {
  lowered
    .bodies
    .iter()
    .enumerate()
    .find(|(_, body)| matches!(body.kind, BodyKind::TopLevel))
    .map(|(idx, body)| (lowered.hir.bodies[idx], body.as_ref()))
    .expect("top-level body")
}

fn check_top_level(
  source: &str,
  store: &Arc<TypeStore>,
  bindings: &HashMap<String, TypeId>,
) -> typecheck_ts::BodyCheckResult {
  let lowered = lower_from_source(source).expect("lower");
  let (body_id, body) = top_level_body(&lowered);

  let ast = parse_with_options(
    source,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  )
  .expect("parse");
  let ast = Arc::new(ast);
  let ast_index = AstIndex::new(Arc::clone(&ast), FileId(0), None);

  let caches = CheckerCaches::new(Default::default()).for_body();
  check_body(
    body_id,
    body,
    &lowered.names,
    FileId(0),
    &ast_index,
    Arc::clone(store),
    ScriptTarget::Es2015,
    true,
    &caches,
    bindings,
    None,
  )
}

fn signature_1(store: &Arc<TypeStore>, param_ty: TypeId, ret_ty: TypeId) -> SignatureId {
  let sig = Signature {
    params: vec![Param {
      name: None,
      ty: param_ty,
      optional: false,
      rest: false,
    }],
    ret: ret_ty,
    type_params: Vec::new(),
    this_param: None,
  };
  store.intern_signature(sig)
}

#[test]
fn overload_call_records_selected_signature() {
  let source = r#"const value = foo("hi");"#;
  let store = TypeStore::new();
  let prim = store.primitive_ids();

  let sig_number = signature_1(&store, prim.number, prim.number);
  let sig_string = signature_1(&store, prim.string, prim.string);
  let foo_ty = store.intern_type(TypeKind::Callable {
    overloads: vec![sig_number, sig_string],
  });

  let mut bindings = HashMap::new();
  bindings.insert("foo".to_string(), foo_ty);

  let result = check_top_level(source, &store, &bindings);
  assert!(
    result.diagnostics().is_empty(),
    "unexpected diagnostics: {:?}",
    result.diagnostics()
  );

  let call_start = source.find("foo(\"hi\")").expect("call exists") as u32;
  let call_offset = call_start + 3; // points at `(` in `foo("hi")`
  let (expr, _) = result.expr_at(call_offset).expect("call expr");
  let sig_id = result.call_signature(expr).expect("call signature recorded");

  let sig = store.signature(sig_id);
  assert_eq!(sig.params.len(), 1);
  assert_eq!(sig.params[0].ty, prim.string);
  assert_eq!(sig.ret, prim.string);
}

#[test]
fn narrowing_updates_call_signature_in_checked_body() {
  let source = r#"
declare function foo(x: string): string;
declare function foo(x: number): number;

export function f(x: string | number) {
  if (typeof x === "string") {
    return foo(x);
  }
  return foo(x);
}
"#;
  let mut host = MemoryHost::new();
  let key = FileKey::new("entry.ts");
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key.clone()]);
  let _diagnostics = program.check();

  let file = program.file_id(&key).expect("entry.ts file id");
  let call_sites: Vec<u32> = source
    .match_indices("foo(x)")
    .map(|(idx, _)| idx as u32)
    .collect();
  assert_eq!(call_sites.len(), 2, "expected 2 foo(x) call sites");

  let sigs_for = |call_start: u32| -> (SignatureId, String) {
    let call_offset = call_start + 3; // points at `(` in `foo(x)`
    let sig_id = program
      .call_signature_at(file, call_offset)
      .expect("call signature recorded");
    let sig = program
      .signature(sig_id)
      .unwrap_or_else(|| panic!("missing signature info for {sig_id:?}"));
    (
      sig_id,
      program.display_type(sig.params[0].ty).to_string(),
    )
  };

  let (sig_then, param_then) = sigs_for(call_sites[0]);
  let (sig_else, param_else) = sigs_for(call_sites[1]);

  assert_ne!(sig_then, sig_else, "expected branch calls to pick distinct overloads");
  assert_eq!(param_then, "string");
  assert_eq!(param_else, "number");
}

#[test]
fn generic_call_records_instantiated_signature() {
  let source = r#"
export function id<T>(value: T): T { return value; }

const n: number = 1;
const s: string = "hi";

export const num = id(n);
export const str = id(s);
"#;
  let mut host = MemoryHost::new();
  let key = FileKey::new("entry.ts");
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file = program.file_id(&key).expect("entry.ts file id");
  let call_sites: Vec<u32> = source
    .match_indices("id(")
    .map(|(idx, _)| idx as u32)
    .collect();
  assert_eq!(call_sites.len(), 2, "expected 2 id(...) call sites");

  let sig_for = |call_start: u32| -> Signature {
    let call_offset = call_start + 2; // points at `(` in `id(...)`
    let sig_id = program
      .call_signature_at(file, call_offset)
      .expect("call signature recorded");
    program.signature(sig_id).expect("signature in store")
  };

  let sig_num = sig_for(call_sites[0]);
  let sig_str = sig_for(call_sites[1]);

  assert_eq!(sig_num.params.len(), 1);
  assert_eq!(program.display_type(sig_num.params[0].ty).to_string(), "number");
  assert_eq!(program.display_type(sig_num.ret).to_string(), "number");

  assert_eq!(sig_str.params.len(), 1);
  assert_eq!(program.display_type(sig_str.params[0].ty).to_string(), "string");
  assert_eq!(program.display_type(sig_str.ret).to_string(), "string");
}

#[test]
fn new_expr_records_construct_signature() {
  let source = r#"const value = new Foo("hi");"#;
  let store = TypeStore::new();
  let prim = store.primitive_ids();

  let instance_ty = {
    let shape_id = store.intern_shape(Shape::new());
    let obj_id = store.intern_object(ObjectType { shape: shape_id });
    store.intern_type(TypeKind::Object(obj_id))
  };

  let sig_number = signature_1(&store, prim.number, instance_ty);
  let sig_string = signature_1(&store, prim.string, instance_ty);

  let ctor_ty = {
    let mut shape = Shape::new();
    shape.construct_signatures = vec![sig_number, sig_string];
    let shape_id = store.intern_shape(shape);
    let obj_id = store.intern_object(ObjectType { shape: shape_id });
    store.intern_type(TypeKind::Object(obj_id))
  };

  let mut bindings = HashMap::new();
  bindings.insert("Foo".to_string(), ctor_ty);

  let result = check_top_level(source, &store, &bindings);
  assert!(
    result.diagnostics().is_empty(),
    "unexpected diagnostics: {:?}",
    result.diagnostics()
  );

  let new_start = source.find("new Foo(\"hi\")").expect("new exists") as u32;
  let new_offset = new_start + "new Foo".len() as u32; // points at `(` in `new Foo("hi")`
  let (expr, _) = result.expr_at(new_offset).expect("new expr");
  let sig_id = result.call_signature(expr).expect("construct signature recorded");

  let sig = store.signature(sig_id);
  assert_eq!(sig.params.len(), 1);
  assert_eq!(sig.params[0].ty, prim.string);
  assert_eq!(sig.ret, instance_ty);
}

#[test]
fn optional_chain_call_records_signature() {
  let source = r#"const value = obj?.method(123);"#;
  let store = TypeStore::new();
  let prim = store.primitive_ids();

  let method_sig = signature_1(&store, prim.number, prim.string);
  let method_ty = store.intern_type(TypeKind::Callable {
    overloads: vec![method_sig],
  });

  let obj_ty = {
    let mut shape = Shape::new();
    shape.properties.push(Property {
      key: PropKey::String(store.intern_name("method")),
      data: PropData {
        ty: method_ty,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    });
    let shape_id = store.intern_shape(shape);
    let obj_id = store.intern_object(ObjectType { shape: shape_id });
    store.intern_type(TypeKind::Object(obj_id))
  };

  let obj_optional_ty = store.union(vec![obj_ty, prim.undefined]);

  let mut bindings = HashMap::new();
  bindings.insert("obj".to_string(), obj_optional_ty);

  let result = check_top_level(source, &store, &bindings);
  assert!(
    result.diagnostics().is_empty(),
    "unexpected diagnostics: {:?}",
    result.diagnostics()
  );

  let call_start = source
    .find("obj?.method(123)")
    .expect("optional call exists") as u32;
  let call_offset = call_start + "obj?.method".len() as u32; // points at `(` in `obj?.method(123)`
  let (expr, _) = result.expr_at(call_offset).expect("call expr");
  let sig_id = result.call_signature(expr).expect("signature recorded");

  let sig = store.signature(sig_id);
  assert_eq!(sig.params.len(), 1);
  assert_eq!(sig.params[0].ty, prim.number);
  assert_eq!(sig.ret, prim.string);
}
