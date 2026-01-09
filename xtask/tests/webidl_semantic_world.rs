use std::path::Path;

use webidl_ir::{IdlType, NamedType, NamedTypeKind, StringType};
use xtask::webidl::load::{load_combined_webidl, WebIdlSource};
use xtask::webidl::resolve::resolve_webidl_world;
use xtask::webidl::{parse_webidl, SemanticWorld};

#[test]
fn type_context_flattens_inherited_dictionary_members() {
  let idl = r#"
    dictionary Parent {
      boolean capture = false;
    };

    dictionary Child : Parent {
      DOMString foo;
    };
  "#;

  let parsed = parse_webidl(idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  let semantic = SemanticWorld::from_resolved(&resolved);
  let ctx = semantic.build_type_context();

  let members = ctx
    .flattened_dictionary_members("Child")
    .expect("Child dictionary exists");
  assert_eq!(members.len(), 2);
  assert_eq!(members[0].name, "capture");
  assert_eq!(members[1].name, "foo");
  assert_eq!(members[1].ty, IdlType::String(StringType::DomString));
}

#[test]
fn assigns_named_type_kinds_for_dom_eventtarget_and_add_event_listener_options() {
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
      eprintln!("skipping semantic WebIDL test: missing {label} source at {}", path.display());
    }
    return;
  }

  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  let semantic = SemanticWorld::from_resolved(&resolved);

  // `interface Event` has `readonly attribute EventTarget? target;` in DOM.
  let event = semantic.interfaces.get("Event").expect("Event interface");
  let target_attr = event
    .members
    .iter()
    .find_map(|m| match m.parsed.as_ref()? {
      xtask::webidl::semantic::SemanticInterfaceMemberKind::Attribute { name, ty, .. }
        if name == "target" =>
      {
        Some(ty)
      }
      _ => None,
    })
    .expect("Event.target attribute parsed");

  assert!(
    type_contains_named_kind(target_attr, "EventTarget", NamedTypeKind::Interface),
    "expected Event.target to reference EventTarget as an interface type (got {target_attr:?})"
  );

  // `EventTarget` has `addEventListener(..., optional AddEventListenerOptions options = {});`
  let event_target = semantic
    .interfaces
    .get("EventTarget")
    .expect("EventTarget interface");
  let add_event_listener = event_target
    .members
    .iter()
    .find_map(|m| match m.parsed.as_ref()? {
      xtask::webidl::semantic::SemanticInterfaceMemberKind::Operation { name: Some(name), arguments, .. }
        if name == "addEventListener" =>
      {
        Some(arguments)
      }
      _ => None,
    })
    .expect("EventTarget.addEventListener parsed");

  assert!(
    add_event_listener
      .iter()
      .any(|arg| type_contains_named_kind(&arg.ty, "AddEventListenerOptions", NamedTypeKind::Dictionary)),
    "expected an AddEventListenerOptions dictionary argument on EventTarget.addEventListener"
  );
}

#[test]
fn smoke_build_semantic_world_dom_and_html() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  let sources = [
    WebIdlSource {
      rel_path: "specs/whatwg-dom/dom.bs",
      label: "DOM",
    },
    WebIdlSource {
      rel_path: "specs/whatwg-html/source",
      label: "HTML",
    },
  ];

  let loaded = load_combined_webidl(repo_root, &sources).unwrap();
  if !loaded.missing_sources.is_empty() {
    for (label, path) in &loaded.missing_sources {
      eprintln!("skipping semantic WebIDL smoke test: missing {label} source at {}", path.display());
    }
    return;
  }

  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  let semantic = SemanticWorld::from_resolved(&resolved);
  assert!(
    !semantic.interfaces.is_empty(),
    "expected non-empty interface set from DOM+HTML"
  );
}

fn type_contains_named_kind(ty: &IdlType, name: &str, kind: NamedTypeKind) -> bool {
  match ty {
    IdlType::Named(NamedType { name: n, kind: k }) => n == name && *k == kind,
    IdlType::Nullable(inner)
    | IdlType::Sequence(inner)
    | IdlType::FrozenArray(inner)
    | IdlType::AsyncSequence(inner)
    | IdlType::Promise(inner)
    | IdlType::Annotated { inner, .. } => type_contains_named_kind(inner, name, kind),
    IdlType::Union(members) => members
      .iter()
      .any(|m| type_contains_named_kind(m, name, kind.clone())),
    IdlType::Record(k, v) => {
      type_contains_named_kind(k, name, kind.clone()) || type_contains_named_kind(v, name, kind)
    }
    IdlType::Any
    | IdlType::Undefined
    | IdlType::Boolean
    | IdlType::Numeric(_)
    | IdlType::BigInt
    | IdlType::String(_)
    | IdlType::Object
    | IdlType::Symbol => false,
  }
}
