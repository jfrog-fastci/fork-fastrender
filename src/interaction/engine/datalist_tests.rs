use super::{
  collect_datalist_option_entries, datalist_option_matches_input_value, resolve_associated_datalist,
  DatalistOption,
};
use crate::dom::{
  enumerate_dom_ids, find_node_mut_by_preorder_id, DomNode, DomNodeType, ShadowRootMode,
  HTML_NAMESPACE,
};
use selectors::context::QuirksMode;

fn doc(children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children,
  }
}

fn shadow_root(children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::ShadowRoot {
      mode: ShadowRootMode::Open,
      delegates_focus: false,
    },
    children,
  }
}

fn el(tag: &str, attrs: Vec<(&str, &str)>, children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: tag.to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: attrs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    },
    children,
  }
}

fn text(content: &str) -> DomNode {
  DomNode {
    node_type: DomNodeType::Text {
      content: content.to_string(),
    },
    children: vec![],
  }
}

fn find_by_id<'a>(root: &'a DomNode, html_id: &str) -> Option<&'a DomNode> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(html_id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn node_id(root: &DomNode, html_id: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let node = find_by_id(root, html_id).expect("node");
  ids
    .get(&(node as *const DomNode))
    .copied()
    .expect("id present")
}

#[test]
fn datalist_association_respects_shadow_root_boundary_and_trims_list() {
  let mut dom = doc(vec![el(
    "html",
    vec![],
    vec![el(
      "body",
      vec![],
      vec![el(
        "div",
        vec![("id", "host")],
        vec![
          shadow_root(vec![
            el(
              "input",
              // Include leading/trailing ASCII whitespace that should be trimmed.
              vec![("id", "in"), ("list", " \t dl \n")],
              vec![],
            ),
            el("input", vec![("id", "cross"), ("list", "dl-doc")], vec![]),
            el(
              "datalist",
              vec![("id", "dl")],
              vec![el("option", vec![("value", "x")], vec![])],
            ),
          ]),
          el(
            "datalist",
            vec![("id", "dl-doc")],
            vec![el("option", vec![("value", "y")], vec![])],
          ),
        ],
      )],
    )],
  )]);

  let in_id = node_id(&dom, "in");
  let cross_id = node_id(&dom, "cross");
  let dl_id = node_id(&dom, "dl");

  assert_eq!(
    resolve_associated_datalist(&mut dom, in_id),
    Some(dl_id),
    "input[list] should resolve within its shadow root boundary"
  );

  assert_eq!(
    resolve_associated_datalist(&mut dom, cross_id),
    None,
    "input[list] should not resolve across the shadow root boundary into the document tree"
  );
}

#[test]
fn datalist_option_extraction_extracts_value_label_and_disabled() {
  let mut dom = doc(vec![el(
    "html",
    vec![],
    vec![el(
      "body",
      vec![],
      vec![el(
        "datalist",
        vec![("id", "dl")],
        vec![
          el(
            "option",
            vec![("id", "o1"), ("value", "A"), ("label", "Alpha")],
            vec![],
          ),
          el("option", vec![("id", "o2"), ("value", "B")], vec![text("Bravo")]),
          el("option", vec![("id", "o3")], vec![text("  Charlie  ")]),
          el("option", vec![("id", "o4"), ("label", "")], vec![text("Delta")]),
          el(
            "option",
            vec![("id", "o5"), ("value", ""), ("disabled", "")],
            vec![text("Echo")],
          ),
          el(
            "option",
            vec![("id", "o6")],
            vec![
              text("Hi"),
              el("script", vec![], vec![text("ignored")]),
              text(" There"),
            ],
          ),
        ],
      )],
    )],
  )]);

  let dl_id = node_id(&dom, "dl");
  let entries = collect_datalist_option_entries(&mut dom, dl_id);
  let options: Vec<DatalistOption> = entries.iter().map(|entry| entry.option.clone()).collect();
  assert_eq!(
    options,
    vec![
      DatalistOption {
        value: "A".to_string(),
        label: "Alpha".to_string(),
        disabled: false,
      },
      DatalistOption {
        value: "B".to_string(),
        label: "Bravo".to_string(),
        disabled: false,
      },
      DatalistOption {
        value: "Charlie".to_string(),
        label: "Charlie".to_string(),
        disabled: false,
      },
      DatalistOption {
        value: "Delta".to_string(),
        label: "Delta".to_string(),
        disabled: false,
      },
      DatalistOption {
        value: "".to_string(),
        label: "Echo".to_string(),
        disabled: true,
      },
      DatalistOption {
        value: "Hi There".to_string(),
        label: "Hi There".to_string(),
        disabled: false,
      },
    ]
  );

  // Returned option node ids should reference real `<option>` nodes in the DOM.
  assert_eq!(entries[0].node_id, node_id(&dom, "o1"));
  for entry in &entries {
    let node = find_node_mut_by_preorder_id(&mut dom, entry.node_id).expect("option node");
    assert!(
      node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("option")),
      "datalist option entry ids must reference <option> elements"
    );
  }

  // Case-insensitive prefix matching on either value or label.
  assert!(datalist_option_matches_input_value(&options[0], "a"));
  assert!(datalist_option_matches_input_value(&options[0], "AL"));
  assert!(datalist_option_matches_input_value(&options[1], "br"));
  assert!(datalist_option_matches_input_value(&options[4], "e"));
  assert!(!datalist_option_matches_input_value(&options[2], "delta"));
}

#[test]
fn datalist_option_extraction_ignores_template_descendants() {
  let mut dom = doc(vec![el(
    "html",
    vec![],
    vec![el(
      "body",
      vec![],
      vec![el(
        "datalist",
        vec![("id", "dl")],
        vec![
          el(
            "template",
            vec![],
            vec![el(
              "option",
              vec![("id", "t"), ("value", "t")],
              vec![text("Template")],
            )],
          ),
          el(
            "option",
            vec![("id", "r"), ("value", "r")],
            vec![text("Real")],
          ),
          el(
            "div",
            vec![],
            vec![
              el(
                "template",
                vec![],
                vec![el(
                  "option",
                  vec![("id", "t2"), ("value", "t2")],
                  vec![text("Template2")],
                )],
              ),
              el(
                "option",
                vec![("id", "r2"), ("value", "r2")],
                vec![text("Real2")],
              ),
            ],
          ),
        ],
      )],
    )],
  )]);

  let dl_id = node_id(&dom, "dl");
  let entries = collect_datalist_option_entries(&mut dom, dl_id);
  let options: Vec<DatalistOption> = entries.iter().map(|entry| entry.option.clone()).collect();
  assert_eq!(
    options,
    vec![
      DatalistOption {
        value: "r".to_string(),
        label: "Real".to_string(),
        disabled: false,
      },
      DatalistOption {
        value: "r2".to_string(),
        label: "Real2".to_string(),
        disabled: false,
      },
    ]
  );

  // Ensure we keep pre-order order and only include real options (not inside templates).
  assert_eq!(
    entries.iter().map(|entry| entry.node_id).collect::<Vec<_>>(),
    vec![node_id(&dom, "r"), node_id(&dom, "r2")]
  );
}

#[test]
fn datalist_association_ignores_datalist_inside_template() {
  let mut dom = doc(vec![el(
    "html",
    vec![],
    vec![el(
      "body",
      vec![],
      vec![
        el(
          "template",
          vec![],
          vec![el(
            "datalist",
            vec![("id", "dl")],
            vec![el("option", vec![("value", "x")], vec![])],
          )],
        ),
        el("input", vec![("id", "i"), ("list", "dl")], vec![]),
      ],
    )],
  )]);

  let input_id = node_id(&dom, "i");
  assert_eq!(
    resolve_associated_datalist(&mut dom, input_id),
    None,
    "datalists inside inert <template> contents should not be resolved by input[list]"
  );
}
