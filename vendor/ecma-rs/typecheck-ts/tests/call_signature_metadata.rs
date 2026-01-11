use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::FileId;
use hir_js::{lower_from_source, BodyKind};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use typecheck_ts::check::caches::CheckerCaches;
use typecheck_ts::check::hir_body::{check_body, AstIndex};
use typecheck_ts::lib_support::ScriptTarget;
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

