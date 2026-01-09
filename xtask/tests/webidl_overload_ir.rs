use std::collections::BTreeMap;

use webidl_ir::{parse_default_value, parse_idl_type_complete, IdlType, NamedType, NamedTypeKind, NumericType, StringType};

use xtask::webidl::overload_ir::{
  are_distinguishable, compute_dispatch_plan, compute_effective_overload_set, distinguishing_argument_index,
  validate_overload_set, Optionality, Overload, OverloadArgument, Origin, WorldContext,
  EffectiveOverloadEntry, EffectiveOverloadSet,
};

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
