use std::collections::BTreeMap;
use std::path::Path;

use webidl_ir::{
  parse_default_value, parse_idl_type_complete, DefaultValue, IdlType, NamedType, NamedTypeKind, NumericType,
  StringType,
};

use xtask::webidl::overload_ir::{
  are_distinguishable, compute_dispatch_plan, compute_effective_overload_set, distinguishing_argument_index,
  validate_overload_set, Optionality, Overload, OverloadArgument, Origin, WorldContext,
  EffectiveOverloadEntry, EffectiveOverloadSet,
};
use xtask::webidl::resolve::{resolve_webidl_world, ExposureTarget};
use xtask::webidl::semantic::SemanticInterfaceMemberKind;
use xtask::webidl::type_resolution::expand_typedefs_in_type;
use xtask::webidl::{ast::IdlLiteral, load::{load_combined_webidl, WebIdlSource}, parse_webidl, SemanticWorld};

#[derive(Default)]
struct TestWorld {
  inherits: BTreeMap<String, String>,
}

impl WorldContext for TestWorld {
  fn interface_inherits(&self, interface: &str) -> Option<&str> {
    self.inherits.get(interface).map(|s| s.as_str())
  }
}

fn iface(name: &str) -> IdlType {
  IdlType::Named(NamedType {
    name: name.to_string(),
    kind: NamedTypeKind::Interface,
  })
}

fn dict(name: &str) -> IdlType {
  IdlType::Named(NamedType {
    name: name.to_string(),
    kind: NamedTypeKind::Dictionary,
  })
}

#[test]
fn compute_effective_overload_set_spec_example_a_f() {
  let overloads = vec![
    Overload {
      name: "f".into(),
      arguments: vec![OverloadArgument::required(IdlType::String(StringType::DomString))],
      origin: None,
    },
    Overload {
      name: "f".into(),
      arguments: vec![
        OverloadArgument::required(iface("Node")),
        OverloadArgument::required(IdlType::String(StringType::DomString)),
        OverloadArgument::variadic(IdlType::Numeric(NumericType::Double)),
      ],
      origin: None,
    },
    Overload {
      name: "f".into(),
      arguments: vec![],
      origin: None,
    },
    Overload {
      name: "f".into(),
      arguments: vec![
        OverloadArgument::required(iface("Event")),
        OverloadArgument::required(IdlType::String(StringType::DomString)),
        OverloadArgument::optional(IdlType::String(StringType::DomString)),
        OverloadArgument::variadic(IdlType::Numeric(NumericType::Double)),
      ],
      origin: None,
    },
  ];

  let set = compute_effective_overload_set(&overloads, 4);
  let expected = EffectiveOverloadSet {
    items: vec![
      EffectiveOverloadEntry {
        callable_id: 0,
        type_list: vec![IdlType::String(StringType::DomString)],
        optionality_list: vec![Optionality::Required],
      },
      EffectiveOverloadEntry {
        callable_id: 1,
        type_list: vec![iface("Node"), IdlType::String(StringType::DomString)],
        optionality_list: vec![Optionality::Required, Optionality::Required],
      },
      EffectiveOverloadEntry {
        callable_id: 1,
        type_list: vec![
          iface("Node"),
          IdlType::String(StringType::DomString),
          IdlType::Numeric(NumericType::Double),
        ],
        optionality_list: vec![Optionality::Required, Optionality::Required, Optionality::Variadic],
      },
      EffectiveOverloadEntry {
        callable_id: 1,
        type_list: vec![
          iface("Node"),
          IdlType::String(StringType::DomString),
          IdlType::Numeric(NumericType::Double),
          IdlType::Numeric(NumericType::Double),
        ],
        optionality_list: vec![
          Optionality::Required,
          Optionality::Required,
          Optionality::Variadic,
          Optionality::Variadic,
        ],
      },
      EffectiveOverloadEntry {
        callable_id: 2,
        type_list: vec![],
        optionality_list: vec![],
      },
      EffectiveOverloadEntry {
        callable_id: 3,
        type_list: vec![iface("Event"), IdlType::String(StringType::DomString)],
        optionality_list: vec![Optionality::Required, Optionality::Required],
      },
      EffectiveOverloadEntry {
        callable_id: 3,
        type_list: vec![
          iface("Event"),
          IdlType::String(StringType::DomString),
          IdlType::String(StringType::DomString),
        ],
        optionality_list: vec![Optionality::Required, Optionality::Required, Optionality::Optional],
      },
      EffectiveOverloadEntry {
        callable_id: 3,
        type_list: vec![
          iface("Event"),
          IdlType::String(StringType::DomString),
          IdlType::String(StringType::DomString),
          IdlType::Numeric(NumericType::Double),
        ],
        optionality_list: vec![
          Optionality::Required,
          Optionality::Required,
          Optionality::Optional,
          Optionality::Variadic,
        ],
      },
    ],
  };
  assert_eq!(set, expected);

  // The distinguishing argument index for the groups of size 2/3/4 should be 0 (Node vs Event).
  let world = TestWorld::default();
  for len in [2usize, 3, 4] {
    let entries = set
      .items
      .iter()
      .filter(|e| e.type_list.len() == len)
      .cloned()
      .collect::<Vec<_>>();
    assert_eq!(distinguishing_argument_index(&entries, &world), Some(0));
  }
}

#[test]
fn distinguishability_nullable_dictionary_special_case() {
  let world = TestWorld::default();
  let a = IdlType::Nullable(Box::new(IdlType::Numeric(NumericType::Double)));
  let b = dict("Dictionary1");
  assert!(!are_distinguishable(&a, &b, &world));
}

#[test]
fn distinguishability_interface_inheritance_is_not_distinguishable() {
  let mut world = TestWorld::default();
  world.inherits.insert("Event".into(), "Node".into());
  let node = iface("Node");
  let event = iface("Event");
  assert!(!are_distinguishable(&node, &event, &world));
}

#[test]
fn validate_overload_set_rejects_domstring_vs_usvstring() {
  let world = TestWorld::default();
  let overloads = vec![
    Overload {
      name: "f".into(),
      arguments: vec![OverloadArgument::required(IdlType::String(StringType::DomString))],
      origin: None,
    },
    Overload {
      name: "f".into(),
      arguments: vec![OverloadArgument::required(IdlType::String(StringType::UsvString))],
      origin: None,
    },
  ];

  let err = validate_overload_set(&overloads, &world).unwrap_err();
  let msg = err.iter().map(|d| d.message.as_str()).collect::<String>();
  assert!(msg.contains("DOMString"));
  assert!(msg.contains("USVString"));
}

#[test]
fn validate_overload_set_rejects_mismatch_before_distinguishing_index() {
  let world = TestWorld::default();
  let overloads = vec![
    Overload {
      name: "f".into(),
      arguments: vec![OverloadArgument::required(IdlType::String(StringType::DomString))],
      origin: None,
    },
    Overload {
      name: "f".into(),
      arguments: vec![
        OverloadArgument::required(IdlType::Numeric(NumericType::Long)),
        OverloadArgument::required(IdlType::Numeric(NumericType::Double)),
        OverloadArgument::required(iface("Node")),
        OverloadArgument::required(iface("Node")),
      ],
      origin: None,
    },
    Overload {
      name: "f".into(),
      arguments: vec![
        OverloadArgument::required(IdlType::Numeric(NumericType::Double)),
        OverloadArgument::required(IdlType::Numeric(NumericType::Double)),
        OverloadArgument::required(IdlType::String(StringType::DomString)),
        OverloadArgument::required(iface("Node")),
      ],
      origin: None,
    },
  ];

  let err = validate_overload_set(&overloads, &world).unwrap_err();
  let msg = err.iter().map(|d| d.message.as_str()).collect::<String>();
  assert!(msg.contains("differ before that index"));
  assert!(msg.contains("argument 0"));
  assert!(msg.contains("long"));
  assert!(msg.contains("double"));
}

#[test]
fn validate_overload_set_rejects_bigint_vs_numeric_at_distinguishing_index() {
  let world = TestWorld::default();
  let overloads = vec![
    Overload {
      name: "f".into(),
      arguments: vec![OverloadArgument::required(IdlType::BigInt)],
      origin: None,
    },
    Overload {
      name: "f".into(),
      arguments: vec![OverloadArgument::required(IdlType::Numeric(NumericType::Double))],
      origin: None,
    },
  ];

  let err = validate_overload_set(&overloads, &world).unwrap_err();
  let msg = err.iter().map(|d| d.message.as_str()).collect::<String>();
  assert!(msg.contains("BigInt/numeric restriction"));
  assert!(msg.contains("bigint"));
  assert!(msg.contains("numeric"));
}

fn resolve_named_kinds(ty: &mut IdlType, kinds: &BTreeMap<&str, NamedTypeKind>) {
  match ty {
    IdlType::Named(named) => {
      if let Some(kind) = kinds.get(named.name.as_str()) {
        named.kind = kind.clone();
      }
    }
    IdlType::Nullable(inner)
    | IdlType::Sequence(inner)
    | IdlType::FrozenArray(inner)
    | IdlType::AsyncSequence(inner)
    | IdlType::Promise(inner) => resolve_named_kinds(inner, kinds),
    IdlType::Union(members) => {
      for m in members {
        resolve_named_kinds(m, kinds);
      }
    }
    IdlType::Record(key, value) => {
      resolve_named_kinds(key, kinds);
      resolve_named_kinds(value, kinds);
    }
    IdlType::Annotated { inner, .. } => resolve_named_kinds(inner, kinds),
    IdlType::Any
    | IdlType::Undefined
    | IdlType::Boolean
    | IdlType::Numeric(_)
    | IdlType::BigInt
    | IdlType::String(_)
    | IdlType::Object
    | IdlType::Symbol => {}
  }
}

#[test]
fn event_target_add_event_listener_effective_overload_set() {
  let world = TestWorld::default();

  let mut event_listener = parse_idl_type_complete("EventListener?").unwrap();
  let mut options_union = parse_idl_type_complete("(AddEventListenerOptions or boolean)").unwrap();

  // Simulate the semantic layer resolving named type kinds.
  let mut kinds = BTreeMap::<&str, NamedTypeKind>::new();
  kinds.insert("EventListener", NamedTypeKind::CallbackInterface);
  kinds.insert("AddEventListenerOptions", NamedTypeKind::Dictionary);
  resolve_named_kinds(&mut event_listener, &kinds);
  resolve_named_kinds(&mut options_union, &kinds);

  let mut opt3 = OverloadArgument::optional(options_union);
  opt3.default = Some(parse_default_value("{}").unwrap());

  let overloads = vec![Overload {
    name: "addEventListener".into(),
    arguments: vec![
      OverloadArgument::required(IdlType::String(StringType::DomString)),
      OverloadArgument::required(event_listener),
      opt3,
    ],
    origin: Some(Origin {
      interface: "EventTarget".into(),
      raw_member: "undefined addEventListener(DOMString type, EventListener? callback, optional (AddEventListenerOptions or boolean) options = {});".into(),
    }),
  }];

  let plan = compute_dispatch_plan(&overloads, &world).expect("dispatch plan should be computable");
  let group_counts = plan.groups.iter().map(|g| g.argument_count).collect::<Vec<_>>();
  assert_eq!(group_counts, vec![2, 3]);
  for g in &plan.groups {
    assert_eq!(g.entries.len(), 1);
    assert_eq!(g.distinguishing_argument_index, None);
  }
}

fn default_value_from_idl_literal(lit: &IdlLiteral) -> Option<DefaultValue> {
  match lit {
    IdlLiteral::Null => Some(DefaultValue::Null),
    IdlLiteral::Undefined => Some(DefaultValue::Undefined),
    IdlLiteral::Boolean(b) => Some(DefaultValue::Boolean(*b)),
    // `parse_default_value` already knows how to parse WebIDL numeric literals and special values.
    IdlLiteral::Number(n) => parse_default_value(n).ok(),
    IdlLiteral::String(s) => Some(DefaultValue::String(s.clone())),
    IdlLiteral::EmptyObject => Some(DefaultValue::EmptyDictionary),
    IdlLiteral::EmptyArray => Some(DefaultValue::EmptySequence),
    // `Infinity` / `NaN` appear as identifiers in the lightweight xtask parser; accept them.
    IdlLiteral::Identifier(id) => parse_default_value(id).ok(),
  }
}

#[test]
fn overload_dispatch_plans_can_be_computed_for_window_exposed_dom_operations_in_semantic_world() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  let sources = [WebIdlSource {
    rel_path: "specs/whatwg-dom/dom.bs",
    label: "DOM",
  }];

  let loaded = load_combined_webidl(repo_root, &sources).unwrap();
  if !loaded.missing_sources.is_empty() {
    for (label, path) in &loaded.missing_sources {
      eprintln!("skipping semantic overload test: missing {label} source at {}", path.display());
    }
    return;
  }

  // Limit this smoke test to Window-exposed definitions so we validate a realistic per-global
  // overload set.
  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed).filter_by_exposure(ExposureTarget::Window);
  let semantic = SemanticWorld::from_resolved(&resolved);
  let type_ctx = semantic.build_type_context();

  let mut failures = Vec::<String>::new();
  let mut checked_sets = 0usize;

  // Validate overload planning across all Window-exposed interfaces in the semantic world.
  for (iface_name, iface) in &semantic.interfaces {
    // Group operation overloads by (operation name, static flag). This is the minimal key for
    // WebIDL overload sets for named operations.
    let mut ops: BTreeMap<(String, bool), Vec<(String, Vec<xtask::webidl::semantic::SemanticArgument>)>> =
      BTreeMap::new();
    let mut ctors: Vec<(String, Vec<xtask::webidl::semantic::SemanticArgument>)> = Vec::new();

    for member in &iface.members {
      let Some(parsed) = member.parsed.as_ref() else {
        continue;
      };
      match parsed {
        SemanticInterfaceMemberKind::Constructor { arguments } => {
          ctors.push((member.raw.clone(), arguments.clone()));
        }
        SemanticInterfaceMemberKind::Operation {
          name: Some(name),
          arguments,
          static_,
          special: None,
          ..
        } => {
          ops
            .entry((name.clone(), *static_))
            .or_default()
            .push((member.raw.clone(), arguments.clone()));
        }
        _ => {}
      }
    }

    if !ctors.is_empty() {
      checked_sets += 1;
      let overloads = ctors
        .iter()
        .map(|(raw, args)| Overload {
          name: "constructor".into(),
          arguments: args
            .iter()
            .map(|arg| {
              let ty = expand_typedefs_in_type(&type_ctx, &arg.ty)
                .unwrap_or_else(|e| panic!("expand typedefs in {iface_name} constructor arg {}: {e}", arg.name));
              OverloadArgument {
                name: Some(arg.name.clone()),
                ty,
                optionality: if arg.variadic {
                  Optionality::Variadic
                } else if arg.optional {
                  Optionality::Optional
                } else {
                  Optionality::Required
                },
                default: arg.default.as_ref().and_then(default_value_from_idl_literal),
              }
            })
            .collect(),
          origin: Some(Origin {
            interface: iface_name.clone(),
            raw_member: raw.clone(),
          }),
        })
        .collect::<Vec<_>>();

      if let Err(diags) = compute_dispatch_plan(&overloads, &semantic) {
        failures.push(format!(
          "{iface_name} constructor overload set failed validation:\n{}",
          diags
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
        ));
      }
    }

    for ((op_name, _static_), entries) in ops {
      checked_sets += 1;
      let overloads = entries
        .iter()
        .map(|(raw, args)| Overload {
          name: op_name.clone(),
          arguments: args
            .iter()
            .map(|arg| {
              let ty = expand_typedefs_in_type(&type_ctx, &arg.ty)
                .unwrap_or_else(|e| panic!("expand typedefs in {iface_name}.{op_name} arg {}: {e}", arg.name));
              OverloadArgument {
                name: Some(arg.name.clone()),
                ty,
                optionality: if arg.variadic {
                  Optionality::Variadic
                } else if arg.optional {
                  Optionality::Optional
                } else {
                  Optionality::Required
                },
                default: arg.default.as_ref().and_then(default_value_from_idl_literal),
              }
            })
            .collect(),
          origin: Some(Origin {
            interface: iface_name.clone(),
            raw_member: raw.clone(),
          }),
        })
        .collect::<Vec<_>>();

      if let Err(diags) = compute_dispatch_plan(&overloads, &semantic) {
        failures.push(format!(
          "{iface_name}.{op_name} overload set failed validation:\n{}",
          diags
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
        ));
      }
    }
  }

  assert!(checked_sets > 0, "expected at least one overload set to be checked");
  assert!(
    failures.is_empty(),
    "semantic-world overload planning failed:\n\n{}",
    failures.join("\n\n")
  );
}
