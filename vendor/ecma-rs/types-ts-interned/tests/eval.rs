use std::collections::HashMap;
use std::sync::Arc;

use ordered_float::OrderedFloat;
use types_ts_interned::{
  Accessibility, DefId, EvaluatorLimits, ExpandedType, Indexer, MappedModifier, MappedType,
  ObjectType, Param, PredicateParam, PropData, PropKey, Property, Shape, Signature, TemplateChunk,
  TemplateLiteralType, TupleElem, TypeEvaluator, TypeExpander, TypeId, TypeKind, TypeOptions,
  TypeParamDecl, TypeParamId, TypeStore,
};

#[derive(Default)]
struct MockExpander {
  defs: HashMap<DefId, ExpandedType>,
}

impl MockExpander {
  fn insert(&mut self, def: DefId, expanded: ExpandedType) {
    self.defs.insert(def, expanded);
  }
}

impl TypeExpander for MockExpander {
  fn expand(&self, _store: &TypeStore, def: DefId, _args: &[TypeId]) -> Option<ExpandedType> {
    self.defs.get(&def).cloned()
  }
}

fn evaluator(store: Arc<TypeStore>, expander: &MockExpander) -> TypeEvaluator<'_, MockExpander> {
  TypeEvaluator::new(store, expander)
}

#[test]
fn predicate_asserted_type_param_is_substituted() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t = store.intern_type(TypeKind::TypeParam(TypeParamId(0)));
  let predicate = store.intern_type(TypeKind::Predicate {
    parameter: Some(PredicateParam::Param(0)),
    asserted: Some(t),
    asserts: false,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(predicate, vec![(TypeParamId(0), primitives.number)]);

  assert_eq!(
    store.type_kind(result),
    TypeKind::Predicate {
      parameter: Some(PredicateParam::Param(0)),
      asserted: Some(primitives.number),
      asserts: false,
    }
  );
}

#[test]
fn predicate_without_asserted_type_is_unchanged() {
  let store = TypeStore::new();

  let predicate = store.intern_type(TypeKind::Predicate {
    parameter: Some(PredicateParam::Param(0)),
    asserted: None,
    asserts: false,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(predicate, vec![(TypeParamId(0), store.primitive_ids().number)]);

  assert_eq!(result, predicate);
}

#[test]
fn intersection_with_empty_object_removes_nullish_from_union() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();
  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let union = store.union(vec![primitives.string, primitives.null, primitives.undefined]);
  let intersection = store.intersection(vec![union, empty_object]);

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  assert_eq!(eval.evaluate(intersection), primitives.string);
}

#[test]
fn type_param_intersection_empty_object_is_not_elided() {
  let store = TypeStore::new();
  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let t = store.intern_type(TypeKind::TypeParam(TypeParamId(0)));
  let intersection = store.intersection(vec![t, empty_object]);

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(intersection);

  assert_eq!(result, intersection);
  let TypeKind::Intersection(members) = store.type_kind(result) else {
    panic!("expected intersection, got {:?}", store.type_kind(result));
  };
  assert!(members.contains(&t));
  assert!(members.contains(&empty_object));
}

#[test]
fn signature_type_params_do_not_capture_outer_substitutions() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  // A<string> references B. If type parameter IDs are local to their
  // definitions, A's `T` and B's signature-local `U` can both be `TypeParamId(0)`.
  //
  // The evaluator must treat signature type params as *bound* and therefore
  // ignore any outer substitution for those IDs.
  let a_def = DefId(0);
  let b_def = DefId(1);

  let a_ref_b = store.intern_type(TypeKind::Ref {
    def: b_def,
    args: vec![],
  });

  let u = TypeParamId(0);
  let u_ty = store.intern_type(TypeKind::TypeParam(u));
  let mut b_sig = Signature::new(
    vec![Param {
      name: None,
      ty: u_ty,
      optional: false,
      rest: false,
    }],
    u_ty,
  );
  b_sig.type_params = vec![TypeParamDecl::new(u)];
  let b_sig = store.intern_signature(b_sig);
  let b_callable = store.intern_type(TypeKind::Callable {
    overloads: vec![b_sig],
  });

  let mut expander = MockExpander::default();
  expander.insert(
    a_def,
    ExpandedType {
      params: vec![TypeParamId(0)],
      ty: a_ref_b,
    },
  );
  expander.insert(
    b_def,
    ExpandedType {
      params: vec![],
      ty: b_callable,
    },
  );

  let a_string = store.intern_type(TypeKind::Ref {
    def: a_def,
    args: vec![primitives.string],
  });

  let mut eval = evaluator(store.clone(), &expander);
  let result = eval.evaluate(a_string);

  let TypeKind::Callable { overloads } = store.type_kind(result) else {
    panic!("expected callable, got {:?}", store.type_kind(result));
  };
  assert_eq!(overloads.len(), 1);
  let sig = store.signature(overloads[0]);
  assert_eq!(sig.type_params.len(), 1);
  assert_eq!(sig.type_params[0].id, TypeParamId(0));
  assert_eq!(sig.params.len(), 1);

  assert!(matches!(
    store.type_kind(sig.params[0].ty),
    TypeKind::TypeParam(TypeParamId(0))
  ));
  assert!(matches!(
    store.type_kind(sig.ret),
    TypeKind::TypeParam(TypeParamId(0))
  ));
}

#[test]
fn signature_type_params_are_not_substituted_by_outer_bindings() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let local_param = TypeParamId(0);
  let local_ty = store.intern_type(TypeKind::TypeParam(local_param));

  let mut sig = Signature::new(
    vec![Param {
      name: None,
      ty: local_ty,
      optional: false,
      rest: false,
    }],
    local_ty,
  );
  sig.type_params.push(TypeParamDecl::new(local_param));
  let sig_id = store.intern_signature(sig);

  let callable = store.intern_type(TypeKind::Callable {
    overloads: vec![sig_id],
  });

  // Bindings for `TypeParamId(0)` exist in the outer context, but the signature's
  // local type parameter must shadow them.
  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let evaluated = eval.evaluate_with_bindings(callable, vec![(local_param, primitives.number)]);

  let TypeKind::Callable { overloads } = store.type_kind(evaluated) else {
    panic!("expected callable, got {:?}", store.type_kind(evaluated));
  };
  assert_eq!(overloads.len(), 1);

  let evaluated_sig = store.signature(overloads[0]);
  assert!(matches!(
    store.type_kind(evaluated_sig.params[0].ty),
    TypeKind::TypeParam(TypeParamId(0))
  ));
  assert!(matches!(
    store.type_kind(evaluated_sig.ret),
    TypeKind::TypeParam(TypeParamId(0))
  ));
}

#[test]
fn signature_constraints_and_defaults_are_substituted_but_locals_are_masked() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let local_param = TypeParamId(0);
  let outer_param = TypeParamId(10);

  let local_ty = store.intern_type(TypeKind::TypeParam(local_param));
  let outer_ty = store.intern_type(TypeKind::TypeParam(outer_param));

  let mut sig = Signature::new(
    vec![Param {
      name: None,
      ty: local_ty,
      optional: false,
      rest: false,
    }],
    local_ty,
  );
  sig.type_params.push(TypeParamDecl {
    id: local_param,
    constraint: Some(outer_ty),
    default: Some(outer_ty),
    variance: None,
    const_: false,
  });
  let sig_id = store.intern_signature(sig);

  let callable = store.intern_type(TypeKind::Callable {
    overloads: vec![sig_id],
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let evaluated = eval.evaluate_with_bindings(
    callable,
    vec![
      // Should substitute into constraints/defaults.
      (outer_param, primitives.string),
      // Must be ignored for signature-local occurrences.
      (local_param, primitives.number),
    ],
  );

  let TypeKind::Callable { overloads } = store.type_kind(evaluated) else {
    panic!("expected callable, got {:?}", store.type_kind(evaluated));
  };
  assert_eq!(overloads.len(), 1);
  let evaluated_sig = store.signature(overloads[0]);

  // Constraint/default should see `outer_param` substituted.
  assert_eq!(evaluated_sig.type_params.len(), 1);
  assert_eq!(evaluated_sig.type_params[0].constraint, Some(primitives.string));
  assert_eq!(evaluated_sig.type_params[0].default, Some(primitives.string));

  // Signature-local occurrences must be masked.
  assert!(matches!(
    store.type_kind(evaluated_sig.params[0].ty),
    TypeKind::TypeParam(TypeParamId(0))
  ));
  assert!(matches!(
    store.type_kind(evaluated_sig.ret),
    TypeKind::TypeParam(TypeParamId(0))
  ));
}

#[test]
fn recursive_promise_like_evaluation_terminates() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  // A minimal PromiseLike-ish definition that is *recursive with changing type
  // arguments* (`PromiseLike<T>` contains `PromiseLike<PromiseLike<T>>`).
  //
  // This exercises the evaluator's recursion guards (depth limit + in-progress
  // tracking) without requiring the full lib surface.
  let promise_like_def = DefId(0);
  let t_param = TypeParamId(0);
  let t_ty = store.intern_type(TypeKind::TypeParam(t_param));
  let promise_like_t = store.intern_type(TypeKind::Ref {
    def: promise_like_def,
    args: vec![t_ty],
  });
  let promise_like_promise_like_t = store.intern_type(TypeKind::Ref {
    def: promise_like_def,
    args: vec![promise_like_t],
  });

  let then_sig = store.intern_signature(Signature::new(Vec::new(), promise_like_promise_like_t));
  let then_callable = store.intern_type(TypeKind::Callable {
    overloads: vec![then_sig],
  });

  let shape_id = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(store.intern_name_ref("then")),
      data: PropData {
        ty: then_callable,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: true,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let obj = store.intern_object(ObjectType { shape: shape_id });
  let promise_like_body = store.intern_type(TypeKind::Object(obj));

  let mut expander = MockExpander::default();
  expander.insert(
    promise_like_def,
    ExpandedType {
      params: vec![t_param],
      ty: promise_like_body,
    },
  );

  let promise_like_string = store.intern_type(TypeKind::Ref {
    def: promise_like_def,
    args: vec![primitives.string],
  });

  let mut eval = evaluator(store.clone(), &expander).with_depth_limit(32);
  let result = eval.evaluate(promise_like_string);
  assert!(
    matches!(store.type_kind(result), TypeKind::Object(_)),
    "expected expansion to yield an object type, got {:?}",
    store.type_kind(result)
  );
}

#[test]
fn conditional_distributes_over_union_with_substitution() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: true,
  });

  let mut expander = MockExpander::default();
  expander.insert(
    DefId(0),
    ExpandedType {
      params: vec![TypeParamId(0)],
      ty: cond,
    },
  );

  let arg_union = store.union(vec![
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("ok"))),
    store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1.0))),
  ]);
  let ref_ty = store.intern_type(TypeKind::Ref {
    def: DefId(0),
    args: vec![arg_union],
  });

  let mut eval = evaluator(store.clone(), &expander);
  let result = eval.evaluate(ref_ty);
  let TypeKind::Union(members) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };
  assert!(members.contains(&primitives.number));
  assert!(members.contains(&primitives.boolean));
  assert_eq!(members.len(), 2);
}

#[test]
fn signature_type_param_constraint_and_default_are_substituted() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t_param = TypeParamId(0);
  let t_ty = store.intern_type(TypeKind::TypeParam(t_param));

  let u_param = TypeParamId(1);
  let u_ty = store.intern_type(TypeKind::TypeParam(u_param));

  let mut u_decl = TypeParamDecl::new(u_param);
  u_decl.constraint = Some(t_ty);
  u_decl.default = Some(t_ty);

  let mut sig = Signature::new(
    vec![Param {
      name: None,
      ty: u_ty,
      optional: false,
      rest: false,
    }],
    u_ty,
  );
  sig.type_params = vec![u_decl];

  let callable = store.intern_type(TypeKind::Callable {
    overloads: vec![store.intern_signature(sig)],
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(callable, vec![(t_param, primitives.number)]);

  let TypeKind::Callable { overloads } = store.type_kind(result) else {
    panic!("expected callable, got {:?}", store.type_kind(result));
  };
  assert_eq!(overloads.len(), 1);
  let sig = store.signature(overloads[0]);
  assert_eq!(sig.type_params.len(), 1);
  assert_eq!(sig.type_params[0].constraint, Some(primitives.number));
  assert_eq!(sig.type_params[0].default, Some(primitives.number));
}

#[test]
fn distributive_conditional_preserves_distributivity_for_type_param_members() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: true,
  });

  let mut expander = MockExpander::default();
  expander.insert(
    DefId(0),
    ExpandedType {
      params: vec![TypeParamId(0)],
      ty: cond,
    },
  );

  // Instantiate with `string | U` where `U` is a different (unresolved) type
  // parameter. The conditional should distribute over the concrete member and
  // keep a distributive conditional for the remaining type parameter.
  let other_param = store.intern_type(TypeKind::TypeParam(TypeParamId(1)));
  let arg_union = store.union(vec![primitives.string, other_param]);
  let ref_ty = store.intern_type(TypeKind::Ref {
    def: DefId(0),
    args: vec![arg_union],
  });

  let mut eval = evaluator(store.clone(), &expander);
  let result = eval.evaluate(ref_ty);
  let TypeKind::Union(members) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };
  assert!(members.contains(&primitives.number));

  let mut saw_conditional = false;
  for member in members {
    if let TypeKind::Conditional {
      distributive,
      check,
      extends,
      ..
    } = store.type_kind(member)
    {
      saw_conditional = true;
      assert!(distributive);
      assert!(matches!(store.type_kind(check), TypeKind::TypeParam(_)));
      assert!(matches!(store.type_kind(extends), TypeKind::String));
    }
  }
  assert!(saw_conditional);
}

#[test]
fn distributive_conditional_preserves_self_type_param_member() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: true,
  });

  let mut expander = MockExpander::default();
  expander.insert(
    DefId(0),
    ExpandedType {
      params: vec![TypeParamId(0)],
      ty: cond,
    },
  );

  // Instantiate with `string | T` (recursive). The `T` branch must remain a
  // conditional rather than collapsing to the false branch.
  let self_param = store.intern_type(TypeKind::TypeParam(TypeParamId(0)));
  let arg_union = store.union(vec![primitives.string, self_param]);
  let ref_ty = store.intern_type(TypeKind::Ref {
    def: DefId(0),
    args: vec![arg_union],
  });

  let mut eval = evaluator(store.clone(), &expander);
  let result = eval.evaluate(ref_ty);
  let TypeKind::Union(members) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };
  assert!(members.contains(&primitives.number));

  let mut saw_conditional = false;
  for member in members {
    if let TypeKind::Conditional { distributive, .. } = store.type_kind(member) {
      saw_conditional = true;
      assert!(distributive);
    }
  }
  assert!(saw_conditional);
}

#[test]
fn conditional_with_unsubstituted_type_param_is_deferred() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: false,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(cond);
  assert!(matches!(
    store.type_kind(result),
    TypeKind::Conditional { .. }
  ));
}

#[test]
fn conditional_with_unresolved_extends_type_param_is_deferred() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: primitives.string,
    extends: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: false,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(cond);
  assert!(matches!(
    store.type_kind(result),
    TypeKind::Conditional { .. }
  ));
}

#[test]
fn conditional_with_wrapped_unresolved_type_param_is_deferred() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::Array {
      ty: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
      readonly: false,
    }),
    extends: store.intern_type(TypeKind::Array {
      ty: primitives.string,
      readonly: false,
    }),
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: false,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(cond);
  assert!(matches!(
    store.type_kind(result),
    TypeKind::Conditional { .. }
  ));
}

#[test]
fn conditional_with_infer_in_extends_infers_identity() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t = store.intern_type(TypeKind::TypeParam(TypeParamId(0)));
  let cond = store.intern_type(TypeKind::Conditional {
    check: t,
    extends: store.intern_type(TypeKind::Infer {
      param: TypeParamId(1),
      constraint: None,
    }),
    true_ty: store.intern_type(TypeKind::TypeParam(TypeParamId(1))),
    false_ty: primitives.never,
    distributive: false,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(cond);

  assert_eq!(result, t);
}

#[test]
fn infer_placeholders_are_not_substituted_by_outer_bindings() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t_param = TypeParamId(1);
  let u_param = TypeParamId(0);

  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::TypeParam(t_param)),
    extends: store.intern_type(TypeKind::Infer {
      param: u_param,
      constraint: None,
    }),
    true_ty: store.intern_type(TypeKind::TypeParam(u_param)),
    false_ty: primitives.never,
    distributive: false,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(
    cond,
    vec![(u_param, primitives.string), (t_param, primitives.number)],
  );

  assert_eq!(result, primitives.number);
}

#[test]
fn conditional_infers_return_type() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t_param = TypeParamId(0);
  let r_param = TypeParamId(1);

  // `T extends (...args: any) => infer R ? R : any`
  let t_ty = store.intern_type(TypeKind::TypeParam(t_param));
  let r_ty = store.intern_type(TypeKind::TypeParam(r_param));
  let infer_r = store.intern_type(TypeKind::Infer {
    param: r_param,
    constraint: None,
  });

  let any_rest_param = Param {
    name: None,
    ty: primitives.any,
    optional: false,
    rest: true,
  };
  let extends_sig = store.intern_signature(Signature::new(vec![any_rest_param], infer_r));
  let extends = store.intern_type(TypeKind::Callable {
    overloads: vec![extends_sig],
  });

  let cond = store.intern_type(TypeKind::Conditional {
    check: t_ty,
    extends,
    true_ty: r_ty,
    false_ty: primitives.any,
    distributive: true,
  });

  let check_param = Param {
    name: None,
    ty: primitives.number,
    optional: false,
    rest: false,
  };
  let check_sig = store.intern_signature(Signature::new(vec![check_param], primitives.string));
  let check_callable = store.intern_type(TypeKind::Callable {
    overloads: vec![check_sig],
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(cond, vec![(t_param, check_callable)]);
  assert_eq!(result, primitives.string);
}

#[test]
fn conditional_infers_parameters_tuple() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t_param = TypeParamId(0);
  let p_param = TypeParamId(1);

  // `T extends (...args: infer P) => any ? P : never`
  let t_ty = store.intern_type(TypeKind::TypeParam(t_param));
  let p_ty = store.intern_type(TypeKind::TypeParam(p_param));
  let infer_p = store.intern_type(TypeKind::Infer {
    param: p_param,
    constraint: None,
  });

  let rest_infer_param = Param {
    name: None,
    ty: infer_p,
    optional: false,
    rest: true,
  };
  let extends_sig = store.intern_signature(Signature::new(vec![rest_infer_param], primitives.any));
  let extends = store.intern_type(TypeKind::Callable {
    overloads: vec![extends_sig],
  });

  let cond = store.intern_type(TypeKind::Conditional {
    check: t_ty,
    extends,
    true_ty: p_ty,
    false_ty: primitives.never,
    distributive: true,
  });

  let check_sig = store.intern_signature(Signature::new(
    vec![
      Param {
        name: None,
        ty: primitives.number,
        optional: false,
        rest: false,
      },
      Param {
        name: None,
        ty: primitives.string,
        optional: true,
        rest: false,
      },
    ],
    primitives.void,
  ));
  let check_callable = store.intern_type(TypeKind::Callable {
    overloads: vec![check_sig],
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(cond, vec![(t_param, check_callable)]);

  let TypeKind::Tuple(elems) = store.type_kind(result) else {
    panic!("expected tuple, got {:?}", store.type_kind(result));
  };
  assert_eq!(elems.len(), 2);

  let elem_0 = store.evaluate(store.intern_type(TypeKind::IndexedAccess {
    obj: result,
    index: store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(0.0))),
  }));
  assert_eq!(elem_0, primitives.number);

  let elem_1 = store.evaluate(store.intern_type(TypeKind::IndexedAccess {
    obj: result,
    index: store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1.0))),
  }));
  assert_eq!(elem_1, store.union(vec![primitives.string, primitives.undefined]));
}

#[test]
fn infer_constraint_failure_defers_conditional() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t_param = TypeParamId(0);
  let r_param = TypeParamId(1);

  // `T extends (...args: any) => infer R extends string ? R : never`
  let t_ty = store.intern_type(TypeKind::TypeParam(t_param));
  let r_ty = store.intern_type(TypeKind::TypeParam(r_param));
  let infer_r = store.intern_type(TypeKind::Infer {
    param: r_param,
    constraint: Some(primitives.string),
  });

  let any_rest_param = Param {
    name: None,
    ty: primitives.any,
    optional: false,
    rest: true,
  };
  let extends_sig = store.intern_signature(Signature::new(vec![any_rest_param], infer_r));
  let extends = store.intern_type(TypeKind::Callable {
    overloads: vec![extends_sig],
  });

  let cond = store.intern_type(TypeKind::Conditional {
    check: t_ty,
    extends,
    true_ty: r_ty,
    false_ty: primitives.never,
    distributive: true,
  });

  let check_sig = store.intern_signature(Signature::new(Vec::new(), primitives.number));
  let check_callable = store.intern_type(TypeKind::Callable {
    overloads: vec![check_sig],
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(cond, vec![(t_param, check_callable)]);
  assert!(matches!(store.type_kind(result), TypeKind::Conditional { .. }));
}

#[test]
fn conditional_infer_does_not_capture_outer_substitution_across_defs() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let def_a = DefId(0);
  let def_b = DefId(1);

  // Def A is generic over `T` (TypeParamId(0)) and just aliases Def B.
  //
  // The evaluator instantiates A with a substitution {0 -> string}. When it
  // expands B under that substitution, we must *not* accidentally rewrite B's
  // `infer` placeholder (also TypeParamId(0)) or its corresponding
  // `TypeParam(0)` references in branches.
  let a_param = TypeParamId(0);
  let a_ty = store.intern_type(TypeKind::Ref {
    def: def_b,
    args: Vec::new(),
  });

  // Def B is non-generic but contains a conditional type with `infer R` where
  // `R` was allocated as TypeParamId(0) (colliding with A's type param id).
  //
  // `string extends (() => infer R) ? R : never`
  let infer_r = store.intern_type(TypeKind::Infer {
    param: TypeParamId(0),
    constraint: None,
  });
  let extends_sig = store.intern_signature(Signature::new(Vec::new(), infer_r));
  let extends = store.intern_type(TypeKind::Callable {
    overloads: vec![extends_sig],
  });
  let true_ty = store.intern_type(TypeKind::TypeParam(TypeParamId(0)));
  let cond = store.intern_type(TypeKind::Conditional {
    check: primitives.string,
    extends,
    true_ty,
    false_ty: primitives.never,
    distributive: false,
  });

  let mut expander = MockExpander::default();
  expander.insert(
    def_a,
    ExpandedType {
      params: vec![a_param],
      ty: a_ty,
    },
  );
  expander.insert(
    def_b,
    ExpandedType {
      params: Vec::new(),
      ty: cond,
    },
  );

  let ref_a = store.intern_type(TypeKind::Ref {
    def: def_a,
    args: vec![primitives.string],
  });

  let mut eval = evaluator(store.clone(), &expander);
  let result = eval.evaluate(ref_a);

  let TypeKind::Conditional {
    check,
    extends,
    true_ty,
    false_ty,
    distributive,
  } = store.type_kind(result)
  else {
    panic!("expected conditional type, got {:?}", store.type_kind(result));
  };

  assert_eq!(check, primitives.string);
  assert!(!distributive);
  assert_eq!(false_ty, primitives.never);
  assert_eq!(store.type_kind(true_ty), TypeKind::TypeParam(TypeParamId(0)));

  let TypeKind::Callable { overloads } = store.type_kind(extends) else {
    panic!("expected callable extends operand, got {:?}", store.type_kind(extends));
  };
  assert_eq!(overloads.len(), 1);
  let sig = store.signature(overloads[0]);
  assert_eq!(sig.params.len(), 0);
  assert_eq!(
    store.type_kind(sig.ret),
    TypeKind::Infer {
      param: TypeParamId(0),
      constraint: None
    }
  );
}

#[test]
fn distributive_conditional_instantiated_with_never_yields_never() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: true,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(cond, vec![(TypeParamId(0), primitives.never)]);
  assert_eq!(result, primitives.never);
}

#[test]
fn conditional_checked_type_any_yields_union_of_branches() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: primitives.any,
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: false,
  });

  let result = store.evaluate(cond);
  assert_eq!(
    result,
    store.union(vec![primitives.number, primitives.boolean])
  );
}

#[test]
fn distributive_conditional_any_is_union_of_branches() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: true,
  });

  let mut expander = MockExpander::default();
  expander.insert(
    DefId(0),
    ExpandedType {
      params: vec![TypeParamId(0)],
      ty: cond,
    },
  );

  let ref_ty = store.intern_type(TypeKind::Ref {
    def: DefId(0),
    args: vec![primitives.any],
  });

  let mut eval = evaluator(store.clone(), &expander);
  let result = eval.evaluate(ref_ty);
  assert_eq!(
    result,
    store.union(vec![primitives.number, primitives.boolean])
  );
}

#[test]
fn conditional_with_unresolved_type_param_is_preserved() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tp = store.intern_type(TypeKind::TypeParam(TypeParamId(0)));
  let cond = store.intern_type(TypeKind::Conditional {
    check: tp,
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: true,
  });

  let result = store.evaluate(cond);
  assert!(matches!(
    store.type_kind(result),
    TypeKind::Conditional { .. }
  ));
}

#[test]
fn conditional_with_unresolved_nested_type_param_is_preserved() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tp = store.intern_type(TypeKind::TypeParam(TypeParamId(0)));
  let key = store.intern_name_ref("a");
  let shape = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(key),
      data: PropData {
        ty: tp,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let check_obj = store.intern_type(TypeKind::Object(store.intern_object(ObjectType { shape })));

  let shape = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(key),
      data: PropData {
        ty: primitives.string,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let extends_obj = store.intern_type(TypeKind::Object(store.intern_object(ObjectType { shape })));

  let cond = store.intern_type(TypeKind::Conditional {
    check: check_obj,
    extends: extends_obj,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: false,
  });

  let result = store.evaluate(cond);
  assert!(matches!(
    store.type_kind(result),
    TypeKind::Conditional { .. }
  ));
}

#[test]
fn conditional_with_unresolved_ref_is_preserved() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let unresolved = store.intern_type(TypeKind::Ref {
    def: DefId(0),
    args: Vec::new(),
  });
  let cond = store.intern_type(TypeKind::Conditional {
    check: unresolved,
    extends: primitives.string,
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: false,
  });

  let result = store.evaluate(cond);
  assert!(matches!(
    store.type_kind(result),
    TypeKind::Conditional { .. }
  ));
}

#[test]
fn distributive_conditional_substitutes_extends_per_member() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tp = store.intern_type(TypeKind::TypeParam(TypeParamId(0)));

  // Inner conditional is intentionally non-distributive (check is wrapped in an array)
  // but still references the same type parameter.
  let inner_cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::Array {
      ty: tp,
      readonly: false,
    }),
    extends: store.intern_type(TypeKind::Array {
      ty: primitives.string,
      readonly: false,
    }),
    true_ty: primitives.number,
    false_ty: primitives.string,
    distributive: false,
  });

  let outer_cond = store.intern_type(TypeKind::Conditional {
    check: tp,
    extends: inner_cond,
    true_ty: primitives.number,
    false_ty: primitives.string,
    distributive: true,
  });

  let mut expander = MockExpander::default();
  expander.insert(
    DefId(0),
    ExpandedType {
      params: vec![TypeParamId(0)],
      ty: outer_cond,
    },
  );

  let arg_union = store.union(vec![primitives.string, primitives.number]);
  let ref_ty = store.intern_type(TypeKind::Ref {
    def: DefId(0),
    args: vec![arg_union],
  });

  // Correct distributive semantics must re-evaluate the `extends` clause with the
  // per-member substitution. If it incorrectly reuses the union-substituted
  // `extends` type, the string branch would spuriously become assignable and
  // produce `number | string` here.
  let mut eval = evaluator(store.clone(), &expander);
  let result = eval.evaluate(ref_ty);
  assert_eq!(result, primitives.string);
}

#[test]
fn distributive_conditional_substitutes_in_extends_per_member() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();
  let name_a = store.intern_name_ref("a");

  let m1_shape = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(name_a),
      data: PropData {
        ty: primitives.number,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let m1 = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: m1_shape }),
  ));

  let m2_shape = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(name_a),
      data: PropData {
        ty: m1,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let m2 = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: m2_shape }),
  ));

  let param = TypeParamId(0);
  let param_ty = store.intern_type(TypeKind::TypeParam(param));
  let extends_shape = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(name_a),
      data: PropData {
        ty: param_ty,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let extends_ty = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: extends_shape,
  })));

  let cond = store.intern_type(TypeKind::Conditional {
    check: param_ty,
    extends: extends_ty,
    true_ty: primitives.number,
    false_ty: primitives.string,
    distributive: true,
  });

  let union = store.union(vec![m1, m2]);
  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(cond, [(param, union)]);

  assert_eq!(result, primitives.string);
}

#[test]
fn conditional_uses_structural_object_assignability() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let foo = store.intern_name_ref("foo");
  let bar = store.intern_name_ref("bar");

  let true_ty = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1.0)));
  let false_ty = store.intern_type(TypeKind::BooleanLiteral(false));

  let src_shape_id = store.intern_shape(Shape {
    properties: vec![
      Property {
        key: PropKey::String(foo),
        data: PropData {
          ty: primitives.number,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
      Property {
        key: PropKey::String(bar),
        data: PropData {
          ty: primitives.string,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
    ],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let src_ty = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: src_shape_id,
  })));

  let dst_shape_id = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(foo),
      data: PropData {
        ty: primitives.number,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let dst_ty = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: dst_shape_id,
  })));
  assert_ne!(src_ty, dst_ty);

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);

  let result = eval.evaluate(store.intern_type(TypeKind::Conditional {
    check: src_ty,
    extends: dst_ty,
    true_ty,
    false_ty,
    distributive: false,
  }));
  assert_eq!(result, true_ty);

  // Negative: property types differ (`foo: number` is not assignable to `foo: string`).
  let dst_mismatch = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: store.intern_shape(Shape {
      properties: vec![Property {
        key: PropKey::String(foo),
        data: PropData {
          ty: primitives.string,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      }],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));
  let result = eval.evaluate(store.intern_type(TypeKind::Conditional {
    check: src_ty,
    extends: dst_mismatch,
    true_ty,
    false_ty,
    distributive: false,
  }));
  assert_eq!(result, false_ty);

  // Negative: optional vs required (`foo?: number` is not assignable to `foo: number`).
  let src_optional = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: store.intern_shape(Shape {
      properties: vec![Property {
        key: PropKey::String(foo),
        data: PropData {
          ty: primitives.number,
          optional: true,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      }],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));
  let result = eval.evaluate(store.intern_type(TypeKind::Conditional {
    check: src_optional,
    extends: dst_ty,
    true_ty,
    false_ty,
    distributive: false,
  }));
  assert_eq!(result, false_ty);
}

#[test]
fn conditional_uses_structural_object_assignability_for_index_signatures() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let foo = store.intern_name_ref("foo");
  let true_ty = primitives.number;
  let false_ty = primitives.boolean;

  // `check`: { [key: string]: number }
  // This is assignable to `{ foo: number }` even though `foo` is not an explicit
  // property, thanks to the index signature.
  let src_shape_id = store.intern_shape(Shape {
    properties: Vec::new(),
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: vec![Indexer {
      key_type: primitives.string,
      value_type: primitives.number,
      readonly: false,
    }],
  });
  let src_ty = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: src_shape_id,
  })));

  let dst_shape_id = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(foo),
      data: PropData {
        ty: primitives.number,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let dst_ty = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: dst_shape_id,
  })));

  let cond = store.intern_type(TypeKind::Conditional {
    check: src_ty,
    extends: dst_ty,
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), true_ty);

  // Negative: index signature value type (`number`) is not assignable to
  // property type (`string`).
  let dst_mismatch = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: store.intern_shape(Shape {
      properties: vec![Property {
        key: PropKey::String(foo),
        data: PropData {
          ty: primitives.string,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      }],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));
  let cond = store.intern_type(TypeKind::Conditional {
    check: src_ty,
    extends: dst_mismatch,
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), false_ty);
}

#[test]
fn conditional_respects_strict_null_checks_option() {
  let strict_store = TypeStore::new();
  let strict_primitives = strict_store.primitive_ids();
  let empty_object = strict_store.intern_type(TypeKind::EmptyObject);
  let true_ty = strict_primitives.number;
  let false_ty = strict_primitives.boolean;

  // Under `strictNullChecks`, `null` is not assignable to `{}`.
  let strict_conditional = strict_store.intern_type(TypeKind::Conditional {
    check: strict_primitives.null,
    extends: empty_object,
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(strict_store.evaluate(strict_conditional), false_ty);

  let loose_store = TypeStore::with_options(TypeOptions {
    strict_null_checks: false,
    ..TypeOptions::default()
  });
  let loose_primitives = loose_store.primitive_ids();
  let empty_object = loose_store.intern_type(TypeKind::EmptyObject);

  // Without `strictNullChecks`, `null` is assignable to `{}`.
  let loose_conditional = loose_store.intern_type(TypeKind::Conditional {
    check: loose_primitives.null,
    extends: empty_object,
    true_ty: loose_primitives.number,
    false_ty: loose_primitives.boolean,
    distributive: false,
  });
  assert_eq!(
    loose_store.evaluate(loose_conditional),
    loose_primitives.number
  );
}

#[test]
fn conditional_respects_strict_function_types_option() {
  let strict_store = TypeStore::new();
  let strict_primitives = strict_store.primitive_ids();

  let strict_wide_param =
    strict_store.union(vec![strict_primitives.number, strict_primitives.string]);
  let strict_narrow_param = strict_primitives.number;

  let strict_narrow_fn = strict_store.intern_type(TypeKind::Callable {
    overloads: vec![strict_store.intern_signature(Signature::new(
      vec![Param {
        name: None,
        ty: strict_narrow_param,
        optional: false,
        rest: false,
      }],
      strict_primitives.number,
    ))],
  });
  let strict_wide_fn = strict_store.intern_type(TypeKind::Callable {
    overloads: vec![strict_store.intern_signature(Signature::new(
      vec![Param {
        name: None,
        ty: strict_wide_param,
        optional: false,
        rest: false,
      }],
      strict_primitives.number,
    ))],
  });

  // With `strict_function_types` enabled by default, a function requiring a
  // narrower parameter is not assignable to one requiring a wider parameter.
  let strict_conditional = strict_store.intern_type(TypeKind::Conditional {
    check: strict_narrow_fn,
    extends: strict_wide_fn,
    true_ty: strict_primitives.number,
    false_ty: strict_primitives.boolean,
    distributive: false,
  });
  assert_eq!(
    strict_store.evaluate(strict_conditional),
    strict_primitives.boolean
  );

  let loose_store = TypeStore::with_options(TypeOptions {
    strict_function_types: false,
    ..TypeOptions::default()
  });
  let loose_primitives = loose_store.primitive_ids();

  let wide_param = loose_store.union(vec![loose_primitives.number, loose_primitives.string]);
  let narrow_param = loose_primitives.number;

  let narrow_fn = loose_store.intern_type(TypeKind::Callable {
    overloads: vec![loose_store.intern_signature(Signature::new(
      vec![Param {
        name: None,
        ty: narrow_param,
        optional: false,
        rest: false,
      }],
      loose_primitives.number,
    ))],
  });
  let wide_fn = loose_store.intern_type(TypeKind::Callable {
    overloads: vec![loose_store.intern_signature(Signature::new(
      vec![Param {
        name: None,
        ty: wide_param,
        optional: false,
        rest: false,
      }],
      loose_primitives.number,
    ))],
  });

  // When `strict_function_types` is disabled, parameter types become bivariant
  // and the conditional reduces to the true branch.
  let loose_conditional = loose_store.intern_type(TypeKind::Conditional {
    check: narrow_fn,
    extends: wide_fn,
    true_ty: loose_primitives.number,
    false_ty: loose_primitives.boolean,
    distributive: false,
  });
  assert_eq!(
    loose_store.evaluate(loose_conditional),
    loose_primitives.number
  );
}

#[test]
fn conditional_respects_exact_optional_property_types_option() {
  let loose_store = TypeStore::new();
  let loose_primitives = loose_store.primitive_ids();
  let foo = loose_store.intern_name_ref("foo");
  let num_or_undef = loose_store.union(vec![loose_primitives.number, loose_primitives.undefined]);

  let src_obj = loose_store.intern_type(TypeKind::Object(loose_store.intern_object(ObjectType {
    shape: loose_store.intern_shape(Shape {
      properties: vec![Property {
        key: PropKey::String(foo),
        data: PropData {
          ty: num_or_undef,
          optional: true,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      }],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));

  let dst_obj = loose_store.intern_type(TypeKind::Object(loose_store.intern_object(ObjectType {
    shape: loose_store.intern_shape(Shape {
      properties: vec![Property {
        key: PropKey::String(foo),
        data: PropData {
          ty: loose_primitives.number,
          optional: true,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      }],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));

  // Without `exactOptionalPropertyTypes`, optional properties implicitly include
  // `undefined` in assignability checks.
  let cond = loose_store.intern_type(TypeKind::Conditional {
    check: src_obj,
    extends: dst_obj,
    true_ty: loose_primitives.number,
    false_ty: loose_primitives.boolean,
    distributive: false,
  });
  assert_eq!(loose_store.evaluate(cond), loose_primitives.number);

  let exact_store = TypeStore::with_options(TypeOptions {
    exact_optional_property_types: true,
    ..TypeOptions::default()
  });
  let exact_primitives = exact_store.primitive_ids();
  let foo = exact_store.intern_name_ref("foo");
  let num_or_undef = exact_store.union(vec![exact_primitives.number, exact_primitives.undefined]);

  let src_obj = exact_store.intern_type(TypeKind::Object(exact_store.intern_object(ObjectType {
    shape: exact_store.intern_shape(Shape {
      properties: vec![Property {
        key: PropKey::String(foo),
        data: PropData {
          ty: num_or_undef,
          optional: true,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      }],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));

  let dst_obj = exact_store.intern_type(TypeKind::Object(exact_store.intern_object(ObjectType {
    shape: exact_store.intern_shape(Shape {
      properties: vec![Property {
        key: PropKey::String(foo),
        data: PropData {
          ty: exact_primitives.number,
          optional: true,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      }],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));

  // With `exactOptionalPropertyTypes`, `undefined` is not implicitly included in
  // optional property assignability.
  let cond = exact_store.intern_type(TypeKind::Conditional {
    check: src_obj,
    extends: dst_obj,
    true_ty: exact_primitives.number,
    false_ty: exact_primitives.boolean,
    distributive: false,
  });
  assert_eq!(exact_store.evaluate(cond), exact_primitives.boolean);
}

#[test]
fn conditional_respects_no_unchecked_indexed_access_option() {
  let strict_store = TypeStore::new();
  let strict_primitives = strict_store.primitive_ids();
  let num_or_undef =
    strict_store.union(vec![strict_primitives.number, strict_primitives.undefined]);

  // src: { [key: string]: number | undefined }
  let src = strict_store.intern_type(TypeKind::Object(strict_store.intern_object(ObjectType {
    shape: strict_store.intern_shape(Shape {
      properties: Vec::new(),
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: vec![Indexer {
        key_type: strict_primitives.string,
        value_type: num_or_undef,
        readonly: false,
      }],
    }),
  })));

  // dst: { [key: string]: number }
  let dst = strict_store.intern_type(TypeKind::Object(strict_store.intern_object(ObjectType {
    shape: strict_store.intern_shape(Shape {
      properties: Vec::new(),
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: vec![Indexer {
        key_type: strict_primitives.string,
        value_type: strict_primitives.number,
        readonly: false,
      }],
    }),
  })));

  let cond = strict_store.intern_type(TypeKind::Conditional {
    check: src,
    extends: dst,
    true_ty: strict_primitives.number,
    false_ty: strict_primitives.boolean,
    distributive: false,
  });
  assert_eq!(strict_store.evaluate(cond), strict_primitives.boolean);

  let unchecked_store = TypeStore::with_options(TypeOptions {
    no_unchecked_indexed_access: true,
    ..TypeOptions::default()
  });
  let prim = unchecked_store.primitive_ids();
  let num_or_undef = unchecked_store.union(vec![prim.number, prim.undefined]);

  let src = unchecked_store.intern_type(TypeKind::Object(unchecked_store.intern_object(
    ObjectType {
      shape: unchecked_store.intern_shape(Shape {
        properties: Vec::new(),
        call_signatures: Vec::new(),
        construct_signatures: Vec::new(),
        indexers: vec![Indexer {
          key_type: prim.string,
          value_type: num_or_undef,
          readonly: false,
        }],
      }),
    },
  )));

  let dst = unchecked_store.intern_type(TypeKind::Object(unchecked_store.intern_object(
    ObjectType {
      shape: unchecked_store.intern_shape(Shape {
        properties: Vec::new(),
        call_signatures: Vec::new(),
        construct_signatures: Vec::new(),
        indexers: vec![Indexer {
          key_type: prim.string,
          value_type: prim.number,
          readonly: false,
        }],
      }),
    },
  )));

  let cond = unchecked_store.intern_type(TypeKind::Conditional {
    check: src,
    extends: dst,
    true_ty: prim.number,
    false_ty: prim.boolean,
    distributive: false,
  });
  assert_eq!(unchecked_store.evaluate(cond), prim.number);
}

#[test]
fn conditional_respects_method_bivariance_under_strict_function_types() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let num_or_str = store.union(vec![primitives.number, primitives.string]);

  let fn_narrow = store.intern_type(TypeKind::Callable {
    overloads: vec![store.intern_signature(Signature::new(
      vec![Param {
        name: None,
        ty: primitives.number,
        optional: false,
        rest: false,
      }],
      primitives.void,
    ))],
  });
  let fn_wide = store.intern_type(TypeKind::Callable {
    overloads: vec![store.intern_signature(Signature::new(
      vec![Param {
        name: None,
        ty: num_or_str,
        optional: false,
        rest: false,
      }],
      primitives.void,
    ))],
  });

  let method_name = store.intern_name_ref("method");
  let make_object = |is_method: bool, ty: TypeId| {
    store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
      shape: store.intern_shape(Shape {
        properties: vec![Property {
          key: PropKey::String(method_name),
          data: PropData {
            ty,
            optional: false,
            readonly: false,
            accessibility: None,
            is_method,
            origin: None,
            declared_on: None,
          },
        }],
        call_signatures: Vec::new(),
        construct_signatures: Vec::new(),
        indexers: Vec::new(),
      }),
    })))
  };

  // Methods are checked bivariantly even under `strictFunctionTypes`.
  let cond = store.intern_type(TypeKind::Conditional {
    check: make_object(true, fn_narrow),
    extends: make_object(true, fn_wide),
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), primitives.number);

  // Non-method properties should obey strict function parameter variance.
  let cond = store.intern_type(TypeKind::Conditional {
    check: make_object(false, fn_narrow),
    extends: make_object(false, fn_wide),
    true_ty: primitives.number,
    false_ty: primitives.boolean,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), primitives.boolean);
}

#[test]
fn conditional_respects_private_member_origin_by_default() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let private_key = store.intern_name_ref("x");
  let extra_key = store.intern_name_ref("y");

  let make_src = |declared_on: DefId| {
    store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
      shape: store.intern_shape(Shape {
        properties: vec![
          Property {
            key: PropKey::String(private_key),
            data: PropData {
              ty: primitives.number,
              optional: false,
              readonly: false,
              accessibility: Some(Accessibility::Private),
              is_method: false,
              origin: None,
              declared_on: Some(declared_on),
            },
          },
          Property {
            key: PropKey::String(extra_key),
            data: PropData {
              ty: primitives.string,
              optional: false,
              readonly: false,
              accessibility: None,
              is_method: false,
              origin: None,
              declared_on: None,
            },
          },
        ],
        call_signatures: Vec::new(),
        construct_signatures: Vec::new(),
        indexers: Vec::new(),
      }),
    })))
  };

  let make_dst = |declared_on: DefId| {
    store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
      shape: store.intern_shape(Shape {
        properties: vec![Property {
          key: PropKey::String(private_key),
          data: PropData {
            ty: primitives.number,
            optional: false,
            readonly: false,
            accessibility: Some(Accessibility::Private),
            is_method: false,
            origin: None,
            declared_on: Some(declared_on),
          },
        }],
        call_signatures: Vec::new(),
        construct_signatures: Vec::new(),
        indexers: Vec::new(),
      }),
    })))
  };

  let true_ty = primitives.number;
  let false_ty = primitives.boolean;

  // Same-origin private members are compatible by default.
  let cond = store.intern_type(TypeKind::Conditional {
    check: make_src(DefId(1)),
    extends: make_dst(DefId(1)),
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), true_ty);

  // Different-origin private members are incompatible without a hook.
  let cond = store.intern_type(TypeKind::Conditional {
    check: make_src(DefId(1)),
    extends: make_dst(DefId(2)),
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), false_ty);
}

#[test]
fn conditional_respects_protected_member_origin_by_default() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let key = store.intern_name_ref("p");
  let make_obj = |declared_on: DefId| {
    store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
      shape: store.intern_shape(Shape {
        properties: vec![Property {
          key: PropKey::String(key),
          data: PropData {
            ty: primitives.number,
            optional: false,
            readonly: false,
            accessibility: Some(Accessibility::Protected),
            is_method: false,
            origin: None,
            declared_on: Some(declared_on),
          },
        }],
        call_signatures: Vec::new(),
        construct_signatures: Vec::new(),
        indexers: Vec::new(),
      }),
    })))
  };

  let true_ty = primitives.number;
  let false_ty = primitives.boolean;

  let cond = store.intern_type(TypeKind::Conditional {
    check: make_obj(DefId(1)),
    extends: make_obj(DefId(1)),
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), true_ty);

  let cond = store.intern_type(TypeKind::Conditional {
    check: make_obj(DefId(1)),
    extends: make_obj(DefId(2)),
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), false_ty);
}

#[test]
fn conditional_treats_private_and_protected_members_as_incompatible() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let key = store.intern_name_ref("x");
  let make_obj = |accessibility| {
    store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
      shape: store.intern_shape(Shape {
        properties: vec![Property {
          key: PropKey::String(key),
          data: PropData {
            ty: primitives.number,
            optional: false,
            readonly: false,
            accessibility: Some(accessibility),
            is_method: false,
            origin: None,
            declared_on: Some(DefId(1)),
          },
        }],
        call_signatures: Vec::new(),
        construct_signatures: Vec::new(),
        indexers: Vec::new(),
      }),
    })))
  };

  let private_obj = make_obj(Accessibility::Private);
  let protected_obj = make_obj(Accessibility::Protected);

  let true_ty = primitives.number;
  let false_ty = primitives.boolean;

  let cond = store.intern_type(TypeKind::Conditional {
    check: private_obj,
    extends: protected_obj,
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), false_ty);

  let cond = store.intern_type(TypeKind::Conditional {
    check: protected_obj,
    extends: private_obj,
    true_ty,
    false_ty,
    distributive: false,
  });
  assert_eq!(store.evaluate(cond), false_ty);
}

#[test]
fn conditional_uses_tuple_vs_array_assignability() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let number_array = store.intern_type(TypeKind::Array {
    ty: primitives.number,
    readonly: false,
  });
  let fixed_tuple = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
  ]));
  let rest_tuple = store.intern_type(TypeKind::Tuple(vec![TupleElem {
    ty: number_array,
    optional: false,
    rest: true,
    readonly: false,
  }]));

  let true_ty = primitives.number;
  let false_ty = primitives.boolean;

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);

  let result = eval.evaluate(store.intern_type(TypeKind::Conditional {
    check: number_array,
    extends: fixed_tuple,
    true_ty,
    false_ty,
    distributive: false,
  }));
  assert_eq!(result, false_ty);

  let result = eval.evaluate(store.intern_type(TypeKind::Conditional {
    check: number_array,
    extends: rest_tuple,
    true_ty,
    false_ty,
    distributive: false,
  }));
  assert_eq!(result, true_ty);
}

#[test]
fn conditional_uses_callable_assignability() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();
  let num_or_str = store.union(vec![primitives.number, primitives.string]);

  let param_num = Param {
    name: None,
    ty: primitives.number,
    optional: false,
    rest: false,
  };
  let param_num_or_str = Param {
    name: None,
    ty: num_or_str,
    optional: false,
    rest: false,
  };

  let f_num = store.intern_type(TypeKind::Callable {
    overloads: vec![store.intern_signature(Signature::new(vec![param_num], primitives.number))],
  });
  let f_num_or_str = store.intern_type(TypeKind::Callable {
    overloads: vec![
      store.intern_signature(Signature::new(vec![param_num_or_str], primitives.number))
    ],
  });

  let true_ty = primitives.number;
  let false_ty = primitives.boolean;

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);

  // With `strict_function_types` enabled by default, a function accepting
  // `number | string` is assignable to one requiring `number`.
  let result = eval.evaluate(store.intern_type(TypeKind::Conditional {
    check: f_num_or_str,
    extends: f_num,
    true_ty,
    false_ty,
    distributive: false,
  }));
  assert_eq!(result, true_ty);

  let result = eval.evaluate(store.intern_type(TypeKind::Conditional {
    check: f_num,
    extends: f_num_or_str,
    true_ty,
    false_ty,
    distributive: false,
  }));
  assert_eq!(result, false_ty);
}

#[test]
fn conditional_uses_structural_assignability_for_arrays() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let check = store.intern_type(TypeKind::Array {
    ty: primitives.number,
    readonly: false,
  });
  let elem_union = store.union(vec![primitives.number, primitives.string]);
  let extends = store.intern_type(TypeKind::Array {
    ty: elem_union,
    readonly: false,
  });

  let cond = store.intern_type(TypeKind::Conditional {
    check,
    extends,
    true_ty: primitives.boolean,
    false_ty: primitives.never,
    distributive: false,
  });

  assert_eq!(store.evaluate(cond), primitives.boolean);
}

#[test]
fn conditional_uses_structural_assignability_for_tuples() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let check = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.string,
      optional: false,
      rest: false,
      readonly: false,
    },
  ]));
  let elem_union = store.union(vec![primitives.number, primitives.string]);
  let extends = store.intern_type(TypeKind::Array {
    ty: elem_union,
    readonly: false,
  });

  let cond = store.intern_type(TypeKind::Conditional {
    check,
    extends,
    true_ty: primitives.boolean,
    false_ty: primitives.never,
    distributive: false,
  });

  assert_eq!(store.evaluate(cond), primitives.boolean);
}

#[test]
fn conditional_uses_structural_assignability_for_callables() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let union = store.union(vec![primitives.number, primitives.string]);

  let wide_sig = store.intern_signature(Signature::new(
    vec![Param {
      name: None,
      ty: union,
      optional: false,
      rest: false,
    }],
    primitives.number,
  ));
  let check = store.intern_type(TypeKind::Callable {
    overloads: vec![wide_sig],
  });

  let narrow_sig = store.intern_signature(Signature::new(
    vec![Param {
      name: None,
      ty: primitives.number,
      optional: false,
      rest: false,
    }],
    primitives.number,
  ));
  let extends = store.intern_type(TypeKind::Callable {
    overloads: vec![narrow_sig],
  });

  let cond = store.intern_type(TypeKind::Conditional {
    check,
    extends,
    true_ty: primitives.boolean,
    false_ty: primitives.never,
    distributive: false,
  });

  // With strict function types enabled by default, (x: number | string) => number is
  // assignable to (x: number) => number.
  assert_eq!(store.evaluate(cond), primitives.boolean);
}

#[test]
fn homomorphic_mapped_type_readonly_over_array_produces_array() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t_param = TypeParamId(0);
  let p_param = TypeParamId(1);
  let t_ty = store.intern_type(TypeKind::TypeParam(t_param));
  let p_ty = store.intern_type(TypeKind::TypeParam(p_param));

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: p_param,
    source: store.intern_type(TypeKind::KeyOf(t_ty)),
    value: store.intern_type(TypeKind::IndexedAccess { obj: t_ty, index: p_ty }),
    readonly: MappedModifier::Add,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: None,
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let array = store.intern_type(TypeKind::Array {
    ty: primitives.number,
    readonly: false,
  });
  let result = eval.evaluate_with_bindings(mapped, vec![(t_param, array)]);
  match store.type_kind(result) {
    TypeKind::Array { ty, readonly } => {
      assert_eq!(ty, primitives.number);
      assert!(readonly);
    }
    other => panic!("expected array, got {other:?}"),
  }
}

#[test]
fn homomorphic_mapped_type_readonly_over_tuple_produces_tuple() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t_param = TypeParamId(0);
  let p_param = TypeParamId(1);
  let t_ty = store.intern_type(TypeKind::TypeParam(t_param));
  let p_ty = store.intern_type(TypeKind::TypeParam(p_param));

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: p_param,
    source: store.intern_type(TypeKind::KeyOf(t_ty)),
    value: store.intern_type(TypeKind::IndexedAccess { obj: t_ty, index: p_ty }),
    readonly: MappedModifier::Add,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: None,
  }));

  let tuple = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.string,
      optional: false,
      rest: false,
      readonly: false,
    },
  ]));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(mapped, vec![(t_param, tuple)]);
  let TypeKind::Tuple(elems) = store.type_kind(result) else {
    panic!("expected tuple, got {:?}", store.type_kind(result));
  };
  assert_eq!(elems.len(), 2);
  assert_eq!(elems[0].ty, primitives.number);
  assert_eq!(elems[1].ty, primitives.string);
  assert!(elems.iter().all(|elem| elem.readonly));
}

#[test]
fn homomorphic_mapped_type_optional_modifier_over_tuple_updates_elements() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let t_param = TypeParamId(0);
  let p_param = TypeParamId(1);
  let t_ty = store.intern_type(TypeKind::TypeParam(t_param));
  let p_ty = store.intern_type(TypeKind::TypeParam(p_param));

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: p_param,
    source: store.intern_type(TypeKind::KeyOf(t_ty)),
    value: store.intern_type(TypeKind::IndexedAccess { obj: t_ty, index: p_ty }),
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Add,
    name_type: None,
    as_type: None,
  }));

  let tuple = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.string,
      optional: false,
      rest: false,
      readonly: false,
    },
  ]));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate_with_bindings(mapped, vec![(t_param, tuple)]);
  let TypeKind::Tuple(elems) = store.type_kind(result) else {
    panic!("expected tuple, got {:?}", store.type_kind(result));
  };
  assert!(elems.iter().all(|elem| elem.optional));
}

#[test]
fn mapped_type_applies_modifiers_and_remaps() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let name_a = store.intern_name_ref("a");
  let name_b = store.intern_name_ref("b");
  let shape_id = store.intern_shape(Shape {
    properties: vec![
      Property {
        key: PropKey::String(name_a),
        data: PropData {
          ty: primitives.string,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
      Property {
        key: PropKey::String(name_b),
        data: PropData {
          ty: primitives.number,
          optional: true,
          readonly: true,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
    ],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: store.intern_type(TypeKind::KeyOf(obj_ty)),
    value: primitives.boolean,
    readonly: MappedModifier::Add,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: Some(
      store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
        head: "".into(),
        spans: vec![TemplateChunk {
          literal: "_done".into(),
          ty: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
        }],
      })),
    ),
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  let TypeKind::Object(obj) = store.type_kind(result) else {
    panic!("expected object, got {:?}", store.type_kind(result));
  };
  let shape = store.shape(store.object(obj).shape);
  assert_eq!(shape.properties.len(), 2);

  let mut names: Vec<_> = shape
    .properties
    .iter()
    .map(|p| match p.key {
      PropKey::String(id) => store.name(id),
      _ => panic!("unexpected key {:?}", p.key),
    })
    .collect();
  names.sort();
  assert_eq!(names, vec!["a_done".to_string(), "b_done".to_string()]);

  for prop in shape.properties.iter() {
    assert!(prop.data.readonly);
    assert_eq!(prop.data.ty, primitives.boolean);
    if let PropKey::String(id) = prop.key {
      if store.name(id) == "b_done" {
        assert!(prop.data.optional);
      } else {
        assert!(!prop.data.optional);
      }
    }
  }
}

#[test]
fn mapped_type_remap_as_never_filters_keys() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let name_a = store.intern_name_ref("a");
  let name_b = store.intern_name_ref("b");
  let shape_id = store.intern_shape(Shape {
    properties: vec![
      Property {
        key: PropKey::String(name_a),
        data: PropData {
          ty: primitives.string,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
      Property {
        key: PropKey::String(name_b),
        data: PropData {
          ty: primitives.number,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
    ],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: store.intern_type(TypeKind::KeyOf(obj_ty)),
    value: primitives.boolean,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: Some(primitives.never),
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  assert!(
    matches!(store.type_kind(result), TypeKind::EmptyObject),
    "expected {{}}, got {:?}",
    store.type_kind(result)
  );
}

#[test]
fn mapped_over_keyof_any_produces_broad_indexers() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: store.intern_type(TypeKind::KeyOf(primitives.any)),
    value: primitives.number,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: None,
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  let TypeKind::Object(obj) = store.type_kind(result) else {
    panic!("expected object, got {:?}", store.type_kind(result));
  };
  let shape = store.shape(store.object(obj).shape);

  assert!(shape.properties.is_empty());
  assert_eq!(shape.indexers.len(), 3);
  assert_eq!(
    shape
      .indexers
      .iter()
      .map(|idx| idx.key_type)
      .collect::<Vec<_>>(),
    vec![primitives.number, primitives.string, primitives.symbol]
  );
  assert!(shape
    .indexers
    .iter()
    .all(|idx| idx.value_type == primitives.number));
}

#[test]
fn mapped_remap_as_any_widens_to_broad_indexers() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let key_a = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("a")));
  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: key_a,
    value: primitives.number,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: Some(primitives.any),
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  let TypeKind::Object(obj) = store.type_kind(result) else {
    panic!("expected object, got {:?}", store.type_kind(result));
  };
  let shape = store.shape(store.object(obj).shape);

  assert!(shape.properties.is_empty());
  assert_eq!(shape.indexers.len(), 3);
  assert_eq!(
    shape
      .indexers
      .iter()
      .map(|idx| idx.key_type)
      .collect::<Vec<_>>(),
    vec![primitives.number, primitives.string, primitives.symbol]
  );
  assert!(shape
    .indexers
    .iter()
    .all(|idx| idx.value_type == primitives.number));
}

#[test]
fn mapped_as_string_produces_string_indexer() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let key_a = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("a")));
  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: key_a,
    value: primitives.number,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: Some(primitives.string),
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  let TypeKind::Object(obj) = store.type_kind(result) else {
    panic!("expected object, got {:?}", store.type_kind(result));
  };
  let shape = store.shape(store.object(obj).shape);

  assert!(shape.properties.is_empty());
  assert_eq!(shape.indexers.len(), 1);
  let indexer = &shape.indexers[0];
  assert_eq!(indexer.key_type, primitives.string);
  assert_eq!(indexer.value_type, primitives.number);
}

#[test]
fn mapped_as_number_produces_number_indexer() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let key_a = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("a")));
  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: key_a,
    value: primitives.boolean,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: Some(primitives.number),
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  let TypeKind::Object(obj) = store.type_kind(result) else {
    panic!("expected object, got {:?}", store.type_kind(result));
  };
  let shape = store.shape(store.object(obj).shape);

  assert!(shape.properties.is_empty());
  assert_eq!(shape.indexers.len(), 1);
  let indexer = &shape.indexers[0];
  assert_eq!(indexer.key_type, primitives.number);
  assert_eq!(indexer.value_type, primitives.boolean);
}

#[test]
fn mapped_as_union_emits_indexer_and_literals() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let key_a_name = store.intern_name_ref("a");
  let key_a = store.intern_type(TypeKind::StringLiteral(key_a_name));

  let literal_num = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(123.0)));
  let as_union = store.union(vec![primitives.string, literal_num]);

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: key_a,
    value: primitives.number,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: Some(as_union),
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  let TypeKind::Object(obj) = store.type_kind(result) else {
    panic!("expected object, got {:?}", store.type_kind(result));
  };
  let shape = store.shape(store.object(obj).shape);

  assert_eq!(shape.indexers.len(), 1);
  assert_eq!(shape.indexers[0].key_type, primitives.string);
  assert_eq!(shape.indexers[0].value_type, primitives.number);

  assert_eq!(shape.properties.len(), 1);
  assert_eq!(shape.properties[0].key, PropKey::Number(123));
  assert_eq!(shape.properties[0].data.ty, primitives.number);
  assert!(!shape
    .properties
    .iter()
    .any(|prop| prop.key == PropKey::String(key_a_name)));
}

#[test]
fn template_literal_distributes_over_union_parts() {
  let store = TypeStore::new();

  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "foo".into(),
    spans: vec![TemplateChunk {
      literal: "bar".into(),
      ty: store.union(vec![
        store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("x"))),
        store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("y"))),
      ]),
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander).with_max_template_strings(2);
  let result = eval.evaluate(tpl);
  let TypeKind::Union(members) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };
  let strings: Vec<_> = members
    .iter()
    .map(|m| match store.type_kind(*m) {
      TypeKind::StringLiteral(id) => store.name(id),
      other => panic!("unexpected member {:?}", other),
    })
    .collect();
  assert!(strings.contains(&"fooxbar".to_string()));
  assert!(strings.contains(&"fooybar".to_string()));
  assert_eq!(strings.len(), 2);
}

#[test]
fn template_literal_expands_boolean_to_true_false_union() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: primitives.boolean,
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(tpl);

  let TypeKind::Union(members) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };

  let mut strings: Vec<_> = members
    .iter()
    .map(|m| match store.type_kind(*m) {
      TypeKind::StringLiteral(id) => store.name(id),
      other => panic!("unexpected member {:?}", other),
    })
    .collect();
  strings.sort();
  assert_eq!(strings, vec!["false".to_string(), "true".to_string()]);
}

#[test]
fn template_literal_null_is_finite() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: primitives.null,
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(tpl);

  let expected = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("null")));
  assert_eq!(result, expected);
}

#[test]
fn template_literal_undefined_is_finite() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: primitives.undefined,
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(tpl);

  let expected = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("undefined")));
  assert_eq!(result, expected);
}

#[test]
fn template_literal_number_literal_uses_js_stringification_large_exponent() {
  let store = TypeStore::new();

  let literal = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1e21)));
  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: literal,
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(tpl);

  let TypeKind::StringLiteral(id) = store.type_kind(result) else {
    panic!("expected string literal, got {:?}", store.type_kind(result));
  };
  assert_eq!(store.name(id), "1e+21".to_string());
}

#[test]
fn template_literal_number_literal_uses_js_stringification_small_exponent() {
  let store = TypeStore::new();

  let literal = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1e-7)));
  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: literal,
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(tpl);

  let TypeKind::StringLiteral(id) = store.type_kind(result) else {
    panic!("expected string literal, got {:?}", store.type_kind(result));
  };
  assert_eq!(store.name(id), "1e-7".to_string());
}

#[test]
fn template_literal_number_literal_negative_zero_formats_as_zero() {
  let store = TypeStore::new();

  let literal = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(-0.0)));
  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: literal,
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(tpl);

  let TypeKind::StringLiteral(id) = store.type_kind(result) else {
    panic!("expected string literal, got {:?}", store.type_kind(result));
  };
  assert_eq!(store.name(id), "0".to_string());
}

#[test]
fn template_literal_expands_boolean_in_mixed_template() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "x_".into(),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: primitives.boolean,
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(tpl);

  let TypeKind::Union(members) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };

  let mut strings: Vec<_> = members
    .iter()
    .map(|m| match store.type_kind(*m) {
      TypeKind::StringLiteral(id) => store.name(id),
      other => panic!("unexpected member {:?}", other),
    })
    .collect();
  strings.sort();
  assert_eq!(strings, vec!["x_false".to_string(), "x_true".to_string()]);
}

#[test]
fn template_literal_pattern_is_preserved() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "foo".into(),
    spans: vec![TemplateChunk {
      literal: "bar".into(),
      ty: primitives.string,
    }],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(tpl);

  assert_ne!(result, primitives.string);
  let TypeKind::TemplateLiteral(tpl) = store.type_kind(result) else {
    panic!(
      "expected template literal, got {:?}",
      store.type_kind(result)
    );
  };
  assert_eq!(tpl.head, "foo".to_string());
  assert_eq!(tpl.spans.len(), 1);
  assert_eq!(tpl.spans[0].literal, "bar".to_string());
  assert_eq!(tpl.spans[0].ty, primitives.string);
}

#[test]
fn template_literal_as_mapped_key_produces_string_indexer() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let key_pattern = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "foo".into(),
    spans: vec![TemplateChunk {
      literal: "bar".into(),
      ty: primitives.string,
    }],
  }));

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: key_pattern,
    value: primitives.number,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: None,
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  let TypeKind::Object(obj) = store.type_kind(result) else {
    panic!("expected object, got {:?}", store.type_kind(result));
  };
  let shape = store.shape(store.object(obj).shape);

  assert!(shape.properties.is_empty());
  assert_eq!(shape.indexers.len(), 1);
  assert_eq!(shape.indexers[0].key_type, primitives.string);
  assert_eq!(shape.indexers[0].value_type, primitives.number);
}

#[test]
fn template_literal_expansion_bails_out_on_blowup() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let union1 = store.union(vec![
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("a"))),
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("b"))),
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("c"))),
  ]);
  let union2 = store.union(vec![
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("x"))),
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("y"))),
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("z"))),
  ]);

  // 3×3 = 9 possible strings; with a low limit, we should bail out and widen to
  // `string` rather than enumerating the full cross-product.
  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![
      TemplateChunk {
        literal: "".into(),
        ty: union1,
      },
      TemplateChunk {
        literal: "".into(),
        ty: union2,
      },
    ],
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander).with_limits(EvaluatorLimits {
    max_template_strings: 4,
    ..EvaluatorLimits::default()
  });
  assert_eq!(eval.limits().max_template_strings, 4);
  let result = eval.evaluate(tpl);
  assert_eq!(result, primitives.string);
}

#[test]
fn indexed_access_collects_optional_properties() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let name_a = store.intern_name_ref("a");
  let name_b = store.intern_name_ref("b");
  let shape_id = store.intern_shape(Shape {
    properties: vec![
      Property {
        key: PropKey::String(name_a),
        data: PropData {
          ty: primitives.string,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
      Property {
        key: PropKey::String(name_b),
        data: PropData {
          ty: primitives.number,
          optional: true,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
    ],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let index_ty = store.union(vec![
    store.intern_type(TypeKind::StringLiteral(name_a)),
    store.intern_type(TypeKind::StringLiteral(name_b)),
  ]);
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: obj_ty,
    index: index_ty,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(indexed);
  let TypeKind::Union(members) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };
  assert!(members.contains(&primitives.string));
  assert!(members.contains(&primitives.number));
  assert!(members.contains(&primitives.undefined));
}

#[test]
fn indexed_access_over_union_collects_member_properties() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let name_a = store.intern_name_ref("a");

  let shape1 = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(name_a),
      data: PropData {
        ty: primitives.string,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let shape2 = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(name_a),
      data: PropData {
        ty: primitives.number,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });

  let union_obj = store.union(vec![
    store.intern_type(TypeKind::Object(
      store.intern_object(ObjectType { shape: shape1 }),
    )),
    store.intern_type(TypeKind::Object(
      store.intern_object(ObjectType { shape: shape2 }),
    )),
  ]);
  let index = store.intern_type(TypeKind::KeyOf(union_obj));
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: union_obj,
    index,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(indexed);
  let TypeKind::Union(members) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };
  assert!(members.contains(&primitives.string));
  assert!(members.contains(&primitives.number));
  assert_eq!(members.len(), 2);
}

#[test]
fn array_length_indexed_access_is_number() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let array = store.intern_type(TypeKind::Array {
    ty: primitives.boolean,
    readonly: false,
  });
  let length = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("length")));
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: array,
    index: length,
  });

  let result = store.evaluate(indexed);
  assert_eq!(result, primitives.number);
}

#[test]
fn tuple_length_indexed_access_fixed_is_number_literal() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tuple = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.string,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
  ]));
  let length = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("length")));
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: tuple,
    index: length,
  });

  let result = store.evaluate(indexed);
  assert_eq!(
    result,
    store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(2.0)))
  );
}

#[test]
fn tuple_length_indexed_access_optional_is_union() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tuple = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.string,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.number,
      optional: true,
      rest: false,
      readonly: false,
    },
  ]));
  let length = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("length")));
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: tuple,
    index: length,
  });

  let result = store.evaluate(indexed);
  assert_eq!(
    result,
    store.union(vec![
      store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1.0))),
      store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(2.0))),
    ])
  );
}

#[test]
fn tuple_length_indexed_access_variadic_is_number() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let number_array = store.intern_type(TypeKind::Array {
    ty: primitives.number,
    readonly: false,
  });
  let tuple = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.string,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: number_array,
      optional: false,
      rest: true,
      readonly: false,
    },
  ]));
  let length = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("length")));
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: tuple,
    index: length,
  });

  let result = store.evaluate(indexed);
  assert_eq!(result, primitives.number);
}

#[test]
fn tuple_indexed_access_fractional_number_literal_is_union_of_elements() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tuple = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.string,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
  ]));
  let index = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1.5)));
  let indexed = store.intern_type(TypeKind::IndexedAccess { obj: tuple, index });

  let result = store.evaluate(indexed);
  assert_eq!(
    result,
    store.union(vec![primitives.string, primitives.number])
  );
}

#[test]
fn keyof_respects_union_and_intersection_semantics() {
  let store = TypeStore::new();

  let name_a = store.intern_name_ref("a");
  let name_b = store.intern_name_ref("b");
  let name_c = store.intern_name_ref("c");

  let shape1 = store.intern_shape(Shape {
    properties: vec![
      Property {
        key: PropKey::String(name_a),
        data: PropData {
          ty: store.primitive_ids().string,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
      Property {
        key: PropKey::String(name_b),
        data: PropData {
          ty: store.primitive_ids().number,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
    ],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let shape2 = store.intern_shape(Shape {
    properties: vec![
      Property {
        key: PropKey::String(name_b),
        data: PropData {
          ty: store.primitive_ids().boolean,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
      Property {
        key: PropKey::String(name_c),
        data: PropData {
          ty: store.primitive_ids().boolean,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      },
    ],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });

  let obj1 = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape1 }),
  ));
  let obj2 = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape2 }),
  ));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);

  let union_keys = eval.evaluate(store.intern_type(TypeKind::KeyOf(store.union(vec![obj1, obj2]))));
  let union_kind = store.type_kind(union_keys);
  let mut union_names: Vec<String> = match union_kind {
    TypeKind::Union(members) => members
      .iter()
      .map(|m| match store.type_kind(*m) {
        TypeKind::StringLiteral(id) => store.name(id),
        other => panic!("unexpected {:?}", other),
      })
      .collect(),
    TypeKind::StringLiteral(id) => vec![store.name(id)],
    other => panic!("unexpected {:?}", other),
  };
  union_names.sort();
  assert_eq!(union_names, vec!["b".to_string()]);

  let inter_keys =
    eval.evaluate(store.intern_type(TypeKind::KeyOf(store.intersection(vec![obj1, obj2]))));
  let TypeKind::Union(inter_members) = store.type_kind(inter_keys) else {
    panic!("expected union");
  };
  let mut names: Vec<_> = inter_members
    .iter()
    .map(|m| match store.type_kind(*m) {
      TypeKind::StringLiteral(id) => store.name(id),
      other => panic!("unexpected {:?}", other),
    })
    .collect();
  names.sort();
  assert_eq!(
    names,
    vec!["a".to_string(), "b".to_string(), "c".to_string()]
  );
}

#[test]
fn keyof_unknown_is_never() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let evaluated = store.evaluate(store.intern_type(TypeKind::KeyOf(primitives.unknown)));
  assert_eq!(evaluated, primitives.never);
}

#[test]
fn keyof_never_is_never() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let evaluated = store.evaluate(store.intern_type(TypeKind::KeyOf(primitives.never)));
  assert_eq!(evaluated, primitives.never);
}

#[test]
fn keyof_empty_object_is_never() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let evaluated = store.evaluate(store.intern_type(TypeKind::KeyOf(empty_object)));
  assert_eq!(evaluated, primitives.never);
}

#[test]
fn keyof_fractional_number_literal_in_mapped_source_widens_to_number() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let source = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1.5)));
  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source,
    value: primitives.boolean,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: None,
  }));

  let keys = store.evaluate(store.intern_type(TypeKind::KeyOf(mapped)));
  assert_eq!(keys, primitives.number);
}

#[test]
fn keyof_fractional_number_literal_in_mapped_remap_widens_to_number() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let source = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("a")));
  let remap = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1.5)));
  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source,
    value: primitives.boolean,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: Some(remap),
  }));

  let keys = store.evaluate(store.intern_type(TypeKind::KeyOf(mapped)));
  assert_eq!(keys, primitives.number);
}

#[test]
fn indexed_access_over_empty_object_is_never() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let index = store.intern_type(TypeKind::KeyOf(empty_object));
  let access = store.intern_type(TypeKind::IndexedAccess {
    obj: empty_object,
    index,
  });
  let evaluated = store.evaluate(access);
  assert_eq!(evaluated, primitives.never);
}

#[test]
fn keyof_string_indexer_includes_number() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let shape = store.intern_shape(Shape {
    properties: Vec::new(),
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: vec![Indexer {
      key_type: primitives.string,
      value_type: primitives.boolean,
      readonly: false,
    }],
  });
  let obj = store.intern_type(TypeKind::Object(store.intern_object(ObjectType { shape })));

  let evaluated = store.evaluate(store.intern_type(TypeKind::KeyOf(obj)));
  let TypeKind::Union(members) = store.type_kind(evaluated) else {
    panic!("expected union, got {:?}", store.type_kind(evaluated));
  };
  assert!(members.contains(&primitives.string));
  assert!(members.contains(&primitives.number));
  assert_eq!(members.len(), 2);
}

#[test]
fn keyof_intersection_indexer_key_behaves_like_string() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  // key_type: (string | number) & string
  let key_type = store.intersection(vec![
    store.union(vec![primitives.string, primitives.number]),
    primitives.string,
  ]);

  let shape = store.intern_shape(Shape {
    properties: Vec::new(),
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: vec![Indexer {
      key_type,
      value_type: primitives.boolean,
      readonly: false,
    }],
  });
  let obj = store.intern_type(TypeKind::Object(store.intern_object(ObjectType { shape })));

  let evaluated = store.evaluate(store.intern_type(TypeKind::KeyOf(obj)));
  let TypeKind::Union(members) = store.type_kind(evaluated) else {
    panic!("expected union, got {:?}", store.type_kind(evaluated));
  };

  assert!(members.contains(&primitives.string));
  assert!(members.contains(&primitives.number));
  assert_eq!(members.len(), 2);
}

#[test]
fn keyof_union_with_disjoint_keys_is_never() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let name_a = store.intern_name_ref("a");
  let name_b = store.intern_name_ref("b");

  let shape1 = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(name_a),
      data: PropData {
        ty: primitives.string,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let shape2 = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(name_b),
      data: PropData {
        ty: primitives.number,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });

  let obj1 = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape1 }),
  ));
  let obj2 = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape2 }),
  ));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let keys = eval.evaluate(store.intern_type(TypeKind::KeyOf(store.union(vec![obj1, obj2]))));

  assert_eq!(keys, primitives.never);
  assert!(matches!(store.type_kind(keys), TypeKind::Never));
}

#[test]
fn keyof_union_intersects_literals_against_broad_keys() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let name_a = store.intern_name_ref("a");

  let with_indexer_shape = store.intern_shape(Shape {
    properties: Vec::new(),
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: vec![Indexer {
      key_type: primitives.string,
      value_type: primitives.number,
      readonly: false,
    }],
  });
  let with_indexer = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: with_indexer_shape,
  })));

  let with_property_shape = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(name_a),
      data: PropData {
        ty: primitives.boolean,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let with_property = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: with_property_shape,
  })));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let keys = eval.evaluate(store.intern_type(TypeKind::KeyOf(
    store.union(vec![with_indexer, with_property]),
  )));

  let TypeKind::StringLiteral(id) = store.type_kind(keys) else {
    panic!("expected string literal, got {:?}", store.type_kind(keys));
  };
  assert_eq!(store.name(id), "a".to_string());
}

#[test]
fn recursive_conditional_terminates() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let name_a = store.intern_name_ref("a");
  let name_b = store.intern_name_ref("b");

  let extends_obj = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: store.intern_shape(Shape {
      properties: vec![Property {
        key: PropKey::String(name_a),
        data: PropData {
          ty: primitives.number,
          optional: false,
          readonly: false,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      }],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));

  let self_ref = store.intern_type(TypeKind::Ref {
    def: DefId(0),
    args: vec![store.intern_type(TypeKind::TypeParam(TypeParamId(0)))],
  });
  let cond = store.intern_type(TypeKind::Conditional {
    check: store.intern_type(TypeKind::TypeParam(TypeParamId(0))),
    extends: extends_obj,
    true_ty: self_ref,
    false_ty: primitives.boolean,
    distributive: false,
  });

  let mut expander = MockExpander::default();
  expander.insert(
    DefId(0),
    ExpandedType {
      params: vec![TypeParamId(0)],
      ty: cond,
    },
  );

  let arg = store.intern_type(TypeKind::Object(store.intern_object(ObjectType {
    shape: store.intern_shape(Shape {
      properties: vec![
        Property {
          key: PropKey::String(name_a),
          data: PropData {
            ty: primitives.number,
            optional: false,
            readonly: false,
            accessibility: None,
            is_method: false,
            origin: None,
            declared_on: None,
          },
        },
        Property {
          key: PropKey::String(name_b),
          data: PropData {
            ty: primitives.string,
            optional: false,
            readonly: false,
            accessibility: None,
            is_method: false,
            origin: None,
            declared_on: None,
          },
        },
      ],
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: Vec::new(),
    }),
  })));
  let ref_ty = store.intern_type(TypeKind::Ref {
    def: DefId(0),
    args: vec![arg],
  });

  let mut eval = evaluator(store.clone(), &expander).with_depth_limit(32);
  let result = eval.evaluate(ref_ty);

  // The evaluator should break the cycle and return a stable type without
  // overflowing the stack.
  assert!(matches!(
    store.type_kind(result),
    TypeKind::Ref { .. } | TypeKind::Boolean | TypeKind::Union(_)
  ));
}

#[test]
fn keyof_includes_symbol_index_signature() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let shape_id = store.intern_shape(Shape {
    properties: Vec::new(),
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: vec![Indexer {
      key_type: primitives.symbol,
      value_type: primitives.string,
      readonly: false,
    }],
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(store.intern_type(TypeKind::KeyOf(obj_ty)));
  assert_eq!(result, primitives.symbol);
}

#[test]
fn indexed_access_uses_symbol_indexer_value_type() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let shape_id = store.intern_shape(Shape {
    properties: Vec::new(),
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: vec![Indexer {
      key_type: primitives.symbol,
      value_type: primitives.string,
      readonly: false,
    }],
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: obj_ty,
    index: primitives.symbol,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(indexed);
  assert_eq!(result, primitives.string);
}

#[test]
fn indexed_access_intersection_indexer_key_behaves_like_string() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  // key_type: (string | number) & string
  let key_type = store.intersection(vec![
    store.union(vec![primitives.string, primitives.number]),
    primitives.string,
  ]);

  let shape_id = store.intern_shape(Shape {
    properties: Vec::new(),
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: vec![Indexer {
      key_type,
      value_type: primitives.boolean,
      readonly: false,
    }],
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);

  // A string indexer accepts both string and number property names.
  let indexed_number = store.intern_type(TypeKind::IndexedAccess {
    obj: obj_ty,
    index: primitives.number,
  });
  assert_eq!(eval.evaluate(indexed_number), primitives.boolean);

  // It must not accept symbol keys.
  let indexed_symbol = store.intern_type(TypeKind::IndexedAccess {
    obj: obj_ty,
    index: primitives.symbol,
  });
  assert_eq!(eval.evaluate(indexed_symbol), primitives.never);
}

#[test]
fn indexed_access_intersection_indexer_key_requires_all_members() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let union_key = store.union(vec![primitives.string, primitives.symbol]);
  let intersection_key = store.intersection(vec![union_key, primitives.string]);

  let make_obj = |key_type| {
    let shape_id = store.intern_shape(Shape {
      properties: Vec::new(),
      call_signatures: Vec::new(),
      construct_signatures: Vec::new(),
      indexers: vec![Indexer {
        key_type,
        value_type: primitives.boolean,
        readonly: false,
      }],
    });
    store.intern_type(TypeKind::Object(
      store.intern_object(ObjectType { shape: shape_id }),
    ))
  };

  let obj_union = make_obj(union_key);
  let obj_intersection = make_obj(intersection_key);

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);

  // Union is OR: (string | symbol) should accept symbol keys.
  let indexed_union_symbol = store.intern_type(TypeKind::IndexedAccess {
    obj: obj_union,
    index: primitives.symbol,
  });
  assert_eq!(eval.evaluate(indexed_union_symbol), primitives.boolean);

  // Intersection is AND: (string | symbol) & string behaves like `string`, so it must reject
  // symbol keys.
  let indexed_intersection_symbol = store.intern_type(TypeKind::IndexedAccess {
    obj: obj_intersection,
    index: primitives.symbol,
  });
  assert_eq!(eval.evaluate(indexed_intersection_symbol), primitives.never);
}

#[test]
fn indexed_access_string_numeric_key_matches_numeric_property() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let shape_id = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::Number(0),
      data: PropData {
        ty: primitives.string,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let index_ty = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("0")));
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: obj_ty,
    index: index_ty,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(indexed);
  assert_eq!(result, primitives.string);
}

#[test]
fn indexed_access_number_key_matches_string_numeric_property() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let shape_id = store.intern_shape(Shape {
    properties: vec![Property {
      key: PropKey::String(store.intern_name_ref("0")),
      data: PropData {
        ty: primitives.string,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    }],
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: Vec::new(),
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let index_ty = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(0.0)));
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: obj_ty,
    index: index_ty,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(indexed);
  assert_eq!(result, primitives.string);
}

#[test]
fn array_indexed_access_accepts_string_numeric_literal() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let array_ty = store.intern_type(TypeKind::Array {
    ty: primitives.string,
    readonly: false,
  });
  let index_ty = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("0")));
  let indexed = store.intern_type(TypeKind::IndexedAccess {
    obj: array_ty,
    index: index_ty,
  });

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(indexed);
  assert_eq!(result, primitives.string);
}

#[test]
fn mapped_type_preserves_symbol_indexer() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let shape_id = store.intern_shape(Shape {
    properties: Vec::new(),
    call_signatures: Vec::new(),
    construct_signatures: Vec::new(),
    indexers: vec![Indexer {
      key_type: primitives.symbol,
      value_type: primitives.string,
      readonly: true,
    }],
  });
  let obj_ty = store.intern_type(TypeKind::Object(
    store.intern_object(ObjectType { shape: shape_id }),
  ));

  let mapped = store.intern_type(TypeKind::Mapped(MappedType {
    param: TypeParamId(0),
    source: store.intern_type(TypeKind::KeyOf(obj_ty)),
    value: primitives.boolean,
    readonly: MappedModifier::Preserve,
    optional: MappedModifier::Preserve,
    name_type: None,
    as_type: None,
  }));

  let default_expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &default_expander);
  let result = eval.evaluate(mapped);
  let TypeKind::Object(obj) = store.type_kind(result) else {
    panic!("expected object, got {:?}", store.type_kind(result));
  };
  let shape = store.shape(store.object(obj).shape);
  assert!(shape.properties.is_empty());
  assert_eq!(shape.indexers.len(), 1);
  assert_eq!(shape.indexers[0].key_type, primitives.symbol);
  assert_eq!(shape.indexers[0].value_type, primitives.boolean);
  assert!(shape.indexers[0].readonly);
}

#[test]
fn keyof_array_includes_length_and_number() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let array_ty = store.intern_type(TypeKind::Array {
    ty: primitives.string,
    readonly: false,
  });
  let keyof_array = store.intern_type(TypeKind::KeyOf(array_ty));
  let result = store.evaluate(keyof_array);

  let length_key = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("length")));
  let TypeKind::Union(keys) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };

  assert!(keys.contains(&primitives.number));
  assert!(keys.contains(&length_key));
}

#[test]
fn keyof_tuple_includes_length_numeric_keys_and_canonical_string_indices() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tuple_ty = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.string,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
  ]));
  let keyof_tuple = store.intern_type(TypeKind::KeyOf(tuple_ty));
  let result = store.evaluate(keyof_tuple);

  let length_key = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("length")));
  let idx_0_num = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(0.0)));
  let idx_1_num = store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(1.0)));
  let idx_0_str = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("0")));
  let idx_1_str = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("1")));

  let TypeKind::Union(keys) = store.type_kind(result) else {
    panic!("expected union, got {:?}", store.type_kind(result));
  };

  assert!(keys.contains(&length_key));
  assert!(keys.contains(&idx_0_num));
  assert!(keys.contains(&idx_1_num));
  assert!(keys.contains(&idx_0_str));
  assert!(keys.contains(&idx_1_str));
}

#[test]
fn keyof_tuple_deterministic_across_stores() {
  fn compute(store: Arc<TypeStore>) -> (Arc<TypeStore>, TypeId) {
    let primitives = store.primitive_ids();
    let tuple_ty = store.intern_type(TypeKind::Tuple(vec![
      TupleElem {
        ty: primitives.string,
        optional: false,
        rest: false,
        readonly: false,
      },
      TupleElem {
        ty: primitives.number,
        optional: false,
        rest: false,
        readonly: false,
      },
    ]));
    let keyof_tuple = store.intern_type(TypeKind::KeyOf(tuple_ty));
    let result = store.evaluate(keyof_tuple);
    (store, result)
  }

  let (store_a, result_a) = compute(TypeStore::new());
  let (store_b, result_b) = compute(TypeStore::new());

  assert_eq!(result_a, result_b);
  assert_eq!(
    store_a.display(result_a).to_string(),
    store_b.display(result_b).to_string()
  );
}

#[test]
fn intersection_with_empty_object_removes_nullish() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let nullable = store.union(vec![primitives.string, primitives.null, primitives.undefined]);
  let intersection = store.intern_type(TypeKind::Intersection(vec![nullable, empty_object]));

  assert_eq!(store.evaluate(intersection), primitives.string);
}

#[test]
fn intersection_of_empty_object_union_with_empty_object_is_empty_object() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let empty_object = store.intern_type(TypeKind::EmptyObject);
  // `{}` is a supertype of all non-nullish values, so `{}` in a union should
  // absorb other non-nullish members once intersected with `{}`.
  let union = store.union(vec![empty_object, primitives.string]);
  let intersection = store.intern_type(TypeKind::Intersection(vec![union, empty_object]));

  assert_eq!(store.evaluate(intersection), empty_object);
}

#[test]
fn intersection_distribution_with_empty_object_filters_nullish_branches() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  // The `{}` constraint should remove `undefined` before distribution so we
  // don't keep impossible `undefined & string` style intersection members.
  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let a = store.union(vec![primitives.string, primitives.undefined]);
  let b = store.union(vec![primitives.string, primitives.boolean]);
  let intersection = store.intern_type(TypeKind::Intersection(vec![a, b, empty_object]));

  assert_eq!(store.evaluate(intersection), primitives.string);
}

#[test]
fn void_intersection_empty_object_is_never() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let intersection = store.intern_type(TypeKind::Intersection(vec![primitives.void, empty_object]));

  assert_eq!(store.evaluate(intersection), primitives.never);
}

#[test]
fn intersection_with_empty_object_removes_void_from_union() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let union = store.union(vec![primitives.string, primitives.void]);
  let intersection = store.intern_type(TypeKind::Intersection(vec![union, empty_object]));

  assert_eq!(store.evaluate(intersection), primitives.string);
}

#[test]
fn intersection_nullish_only_with_empty_object_is_never() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let nullish = store.union(vec![primitives.null, primitives.undefined]);
  let intersection = store.intern_type(TypeKind::Intersection(vec![nullish, empty_object]));

  assert_eq!(store.evaluate(intersection), primitives.never);
}

#[test]
fn unknown_intersection_empty_object_is_empty_object() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let empty_object = store.intern_type(TypeKind::EmptyObject);
  let intersection =
    store.intern_type(TypeKind::Intersection(vec![primitives.unknown, empty_object]));

  assert_eq!(store.evaluate(intersection), empty_object);
}

#[test]
fn intersection_distributes_over_unions_to_narrow() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let a = store.union(vec![primitives.string, primitives.number]);
  let b = store.union(vec![primitives.string, primitives.boolean]);
  let intersection = store.intern_type(TypeKind::Intersection(vec![a, b]));

  assert_eq!(store.evaluate(intersection), primitives.string);
}

#[test]
fn intersection_distribution_respects_limit() {
  let store = TypeStore::new();

  let mk = |value: &str| store.intern_type(TypeKind::StringLiteral(store.intern_name(value)));
  let u1 = store.union(vec![mk("a"), mk("b"), mk("c")]);
  let u2 = store.union(vec![mk("d"), mk("e"), mk("f")]);
  let u3 = store.union(vec![mk("g"), mk("h"), mk("i")]);

  // 3×3×3 = 27 combinations, which exceeds the low cap below.
  let inter = store.intersection(vec![u1, u2, u3]);

  let expander = MockExpander::default();
  let mut eval = evaluator(store.clone(), &expander).with_max_intersection_distribution(4);

  let result_a = eval.evaluate(inter);
  let result_b = eval.evaluate(inter);
  assert_eq!(result_a, result_b);
  assert!(
    matches!(store.type_kind(result_a), TypeKind::Intersection(_) | TypeKind::Union(_)),
    "expected a non-distributed intersection result, got {:?}",
    store.type_kind(result_a)
  );
}
