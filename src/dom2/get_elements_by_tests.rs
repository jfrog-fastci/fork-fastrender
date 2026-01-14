#![cfg(test)]

use super::{Document, NodeId, NodeKind};
use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE, SVG_NAMESPACE};
use selectors::context::QuirksMode;

fn find_node_by_id(doc: &Document, id: &str) -> Option<NodeId> {
  for (idx, node) in doc.nodes().iter().enumerate() {
    let (namespace, attributes) = match &node.kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => (namespace, attributes),
      _ => continue,
    };
    let is_html = doc.is_html_case_insensitive_namespace(namespace);
    if attributes
      .iter()
      .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == id)
    {
      return Some(NodeId::from_index(idx));
    }
  }
  None
}

#[test]
fn get_elements_by_tag_name_skips_inert_templates_and_shadow_roots() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<template><div id=inert></div></template>",
    "<div id=live></div>",
    "<div id=host>",
    "<template shadowroot=open><span id=shadow></span></template>",
    "<span id=light></span>",
    "</div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  let live = doc.get_element_by_id("live").unwrap();
  let host = doc.get_element_by_id("host").unwrap();
  let light = doc.get_element_by_id("light").unwrap();

  // Elements inside inert <template> contents and shadow roots are present in the `dom2` node list
  // but must not be reachable via `getElementsBy*`-style tree queries.
  let inert = find_node_by_id(&doc, "inert").expect("inert node not found");
  let shadow = find_node_by_id(&doc, "shadow").expect("shadow node not found");

  let divs = doc.get_elements_by_tag_name_from(doc.root(), "div");
  assert_eq!(divs, vec![live, host]);
  assert!(!divs.contains(&inert));

  let spans = doc.get_elements_by_tag_name_from(doc.root(), "span");
  assert_eq!(spans, vec![light]);
  assert!(!spans.contains(&shadow));

  // Scoping to an element still must not traverse into its shadow root.
  assert_eq!(doc.get_elements_by_tag_name_from(host, "span"), vec![light]);
}

#[test]
fn get_elements_by_tag_name_matches_html_case_insensitively_and_other_namespaces_case_sensitively() {
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![
      DomNode {
        node_type: DomNodeType::Text {
          content: "x".to_string(),
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "DIV".to_string(),
          namespace: "".to_string(),
          attributes: vec![("id".to_string(), "html".to_string())],
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "FOO".to_string(),
          namespace: SVG_NAMESPACE.to_string(),
          attributes: vec![("id".to_string(), "svg".to_string())],
        },
        children: Vec::new(),
      },
    ],
  };
  let doc = Document::from_renderer_dom(&root);

  let html = doc.get_element_by_id("html").unwrap();
  let svg = doc.get_element_by_id("svg").unwrap();

  assert_eq!(doc.get_elements_by_tag_name_from(doc.root(), "*"), vec![html, svg]);

  // HTML namespace matches ASCII case-insensitively.
  assert_eq!(doc.get_elements_by_tag_name_from(doc.root(), "div"), vec![html]);
  assert_eq!(doc.get_elements_by_tag_name_from(doc.root(), "DiV"), vec![html]);

  // Non-HTML namespaces match case-sensitively.
  assert_eq!(doc.get_elements_by_tag_name_from(doc.root(), "foo"), Vec::new());
  assert_eq!(doc.get_elements_by_tag_name_from(doc.root(), "FOO"), vec![svg]);
}

#[test]
fn get_elements_by_tag_name_ns_supports_wildcards_and_html_namespace_equivalence() {
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: "".to_string(),
          attributes: vec![("id".to_string(), "a".to_string())],
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("id".to_string(), "b".to_string())],
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: SVG_NAMESPACE.to_string(),
          attributes: vec![("id".to_string(), "c".to_string())],
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: "".to_string(),
          attributes: vec![("id".to_string(), "d".to_string())],
        },
        children: Vec::new(),
      },
    ],
  };
  let doc = Document::from_renderer_dom(&root);

  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();
  let c = doc.get_element_by_id("c").unwrap();
  let d = doc.get_element_by_id("d").unwrap();

  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(HTML_NAMESPACE), "div"),
    vec![a, b]
  );
  // HTML namespace elements match `localName` ASCII case-insensitively.
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(HTML_NAMESPACE), "DIV"),
    vec![a, b]
  );
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(""), "div"),
    vec![a, b]
  );
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(""), "DIV"),
    vec![a, b]
  );
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some("*"), "div"),
    vec![a, b, c]
  );
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(HTML_NAMESPACE), "*"),
    vec![a, b, d]
  );
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some("*"), "*"),
    vec![a, b, c, d]
  );
}

#[test]
fn get_elements_by_tag_name_ns_matches_html_local_name_case_insensitively() {
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "SPAN".to_string(),
          namespace: "".to_string(),
          attributes: vec![("id".to_string(), "a".to_string())],
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("id".to_string(), "b".to_string())],
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "FOO".to_string(),
          namespace: SVG_NAMESPACE.to_string(),
          attributes: vec![("id".to_string(), "c".to_string())],
        },
        children: Vec::new(),
      },
    ],
  };
  let doc = Document::from_renderer_dom(&root);

  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();
  let c = doc.get_element_by_id("c").unwrap();

  // HTML namespace matches localName ASCII case-insensitively.
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(HTML_NAMESPACE), "SPAN"),
    vec![a, b]
  );
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(HTML_NAMESPACE), "sPaN"),
    vec![a, b]
  );

  // Non-HTML namespaces match localName case-sensitively, even in an HTML document.
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(SVG_NAMESPACE), "foo"),
    Vec::new()
  );
  assert_eq!(
    doc.get_elements_by_tag_name_ns_from(doc.root(), Some(SVG_NAMESPACE), "FOO"),
    vec![c]
  );
}

#[test]
fn get_elements_by_class_name_tokenizes_by_dom_ascii_whitespace() {
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: "".to_string(),
        attributes: vec![
          ("id".to_string(), "outer".to_string()),
          ("class".to_string(), "foo bar".to_string()),
        ],
      },
      children: vec![
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "span".to_string(),
            namespace: "".to_string(),
            attributes: vec![
              ("id".to_string(), "a".to_string()),
              ("class".to_string(), "foo bar".to_string()),
            ],
          },
          children: Vec::new(),
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "span".to_string(),
            namespace: "".to_string(),
            attributes: vec![
              ("id".to_string(), "b".to_string()),
              ("class".to_string(), "foo\tbar".to_string()),
            ],
          },
          children: Vec::new(),
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "span".to_string(),
            namespace: "".to_string(),
            attributes: vec![
              ("id".to_string(), "c".to_string()),
              ("class".to_string(), "foo".to_string()),
            ],
          },
          children: Vec::new(),
        },
        DomNode {
          node_type: DomNodeType::Element {
            tag_name: "span".to_string(),
            namespace: "".to_string(),
            attributes: vec![
              ("id".to_string(), "d".to_string()),
              ("class".to_string(), "foo\u{000B}bar".to_string()),
            ],
          },
          children: Vec::new(),
        },
      ],
    }],
  };
  let doc = Document::from_renderer_dom(&root);

  let outer = doc.get_element_by_id("outer").unwrap();
  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();
  let d = doc.get_element_by_id("d").unwrap();

  // Query tokens should be split on DOM ASCII whitespace (TAB, LF, FF, CR, SPACE) and tolerate
  // duplicate separators.
  assert_eq!(
    doc.get_elements_by_class_name_from(doc.root(), "foo\t  bar "),
    vec![outer, a, b]
  );

  // Scoped queries should not include the scope element itself.
  assert_eq!(doc.get_elements_by_class_name_from(outer, "foo bar"), vec![a, b]);

  // U+000B VERTICAL TAB is not DOM ASCII whitespace and must not split tokens.
  assert_eq!(
    doc.get_elements_by_class_name_from(doc.root(), "foo\u{000B}bar"),
    vec![d]
  );

  // No tokens means no matches.
  assert!(doc.get_elements_by_class_name_from(doc.root(), " \t\r\n").is_empty());
}

#[test]
fn get_elements_by_name_matches_name_attribute() {
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "template".to_string(),
          namespace: "".to_string(),
          attributes: vec![("id".to_string(), "tpl".to_string())],
        },
        children: vec![DomNode {
          node_type: DomNodeType::Element {
            tag_name: "div".to_string(),
            namespace: "".to_string(),
            attributes: vec![
              ("id".to_string(), "inert".to_string()),
              ("name".to_string(), "foo".to_string()),
            ],
          },
          children: Vec::new(),
        }],
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: "".to_string(),
          attributes: vec![
            ("id".to_string(), "a".to_string()),
            ("name".to_string(), "foo".to_string()),
          ],
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: "".to_string(),
          attributes: vec![
            ("id".to_string(), "b".to_string()),
            ("name".to_string(), "foo".to_string()),
          ],
        },
        children: Vec::new(),
      },
      DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: "".to_string(),
          attributes: vec![
            ("id".to_string(), "c".to_string()),
            ("name".to_string(), "bar".to_string()),
          ],
        },
        children: Vec::new(),
      },
    ],
  };
  let doc = Document::from_renderer_dom(&root);

  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();
  let inert = find_node_by_id(&doc, "inert").expect("inert node not found");

  let foo = doc.get_elements_by_name_from(doc.root(), "foo");
  assert_eq!(foo, vec![a, b]);
  assert!(!foo.contains(&inert));
}

#[test]
fn element_traversal_helpers_skip_non_element_nodes() {
  let root = crate::dom::parse_html(concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=parent>hi<span id=a></span>there<span id=b></span></div>",
    "</body></html>"
  ))
  .unwrap();
  let doc = Document::from_renderer_dom(&root);

  let parent = doc.get_element_by_id("parent").unwrap();
  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();

  assert_eq!(doc.first_element_child(parent), Some(a));
  assert_eq!(doc.last_element_child(parent), Some(b));
  assert_eq!(doc.child_element_count(parent), 2);
  assert_eq!(doc.children_elements(parent), vec![a, b]);

  assert_eq!(doc.previous_element_sibling(a), None);
  assert_eq!(doc.next_element_sibling(a), Some(b));
  assert_eq!(doc.previous_element_sibling(b), Some(a));
  assert_eq!(doc.next_element_sibling(b), None);

  let parent_children = &doc.node(parent).children;
  let pos_a = parent_children
    .iter()
    .position(|&child| child == a)
    .expect("a not found");
  let pos_b = parent_children
    .iter()
    .position(|&child| child == b)
    .expect("b not found");
  let text_between = parent_children[pos_a + 1..pos_b]
    .iter()
    .copied()
    .find(|&child| matches!(&doc.node(child).kind, NodeKind::Text { .. }))
    .expect("expected a text node between <span> siblings");

  assert_eq!(doc.previous_element_sibling(text_between), Some(a));
  assert_eq!(doc.next_element_sibling(text_between), Some(b));
}

#[test]
fn template_element_has_no_element_children() {
  let root = crate::dom::parse_html(concat!(
    "<!doctype html>",
    "<html><body>",
    "<template id=tpl><div id=inert></div></template>",
    "</body></html>"
  ))
  .unwrap();
  let doc = Document::from_renderer_dom(&root);
  let tpl = doc.get_element_by_id("tpl").unwrap();

  assert_eq!(doc.first_element_child(tpl), None);
  assert_eq!(doc.last_element_child(tpl), None);
  assert_eq!(doc.child_element_count(tpl), 0);
  assert!(doc.children_elements(tpl).is_empty());
}
