//! Document title extraction.

use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

/// Find the document title from a parsed DOM tree.
///
/// - Prefers the first `<title>` element in the document `<head>`.
/// - Skips shadow roots and `<template>` subtrees.
/// - Title text is computed by concatenating all descendant text-node contents,
///   trimming leading/trailing whitespace.
/// - Returns `None` when no non-empty title is found.
pub fn find_document_title(dom: &DomNode) -> Option<String> {
  fn find_head(root: &DomNode) -> Option<&DomNode> {
    let mut stack: Vec<&DomNode> = vec![root];
    while let Some(node) = stack.pop() {
      if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      if node.is_template_element() {
        continue;
      }
      if node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("head"))
        && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
      {
        return Some(node);
      }

      for child in node.traversal_children().iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn extract_title_text(title: &DomNode) -> Option<String> {
    let mut buf = String::new();
    let mut stack: Vec<&DomNode> = Vec::new();

    for child in title.traversal_children().iter().rev() {
      stack.push(child);
    }

    while let Some(node) = stack.pop() {
      if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      if node.is_template_element() {
        continue;
      }
      if let Some(text) = node.text_content() {
        buf.push_str(text);
      }
      for child in node.traversal_children().iter().rev() {
        stack.push(child);
      }
    }

    let trimmed = trim_ascii_whitespace(&buf);
    if trimmed.is_empty() {
      None
    } else {
      Some(trimmed.to_string())
    }
  }

  fn find_title(root: &DomNode) -> Option<String> {
    let mut stack: Vec<(&DomNode, bool)> = vec![(root, false)];
    while let Some((node, in_foreign_namespace)) = stack.pop() {
      if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      if node.is_template_element() {
        continue;
      }
      let next_in_foreign_namespace = in_foreign_namespace
        || matches!(
          node.namespace(),
          Some(ns) if !(ns.is_empty() || ns == HTML_NAMESPACE)
        );

      if !in_foreign_namespace
        && node
          .tag_name()
          .is_some_and(|tag| tag.eq_ignore_ascii_case("title"))
        && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
      {
        if let Some(text) = extract_title_text(node) {
          return Some(text);
        }
      }

      for child in node.traversal_children().iter().rev() {
        stack.push((child, next_in_foreign_namespace));
      }
    }
    None
  }

  if let Some(head) = find_head(dom) {
    find_title(head).or_else(|| find_title(dom))
  } else {
    find_title(dom)
  }
}

#[cfg(test)]
mod tests {
  use super::find_document_title;
  use crate::dom::parse_html;
  use crate::dom::{DomNode, DomNodeType};
  use selectors::context::QuirksMode;

  #[test]
  fn finds_title_in_head() {
    let dom = parse_html("<html><head><title>Hello</title></head></html>").unwrap();
    assert_eq!(find_document_title(&dom), Some("Hello".to_string()));
  }

  #[test]
  fn document_title_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let dom = parse_html(&format!(
      "<html><head><title>{nbsp}Hello{nbsp}</title></head></html>"
    ))
    .unwrap();
    assert_eq!(
      find_document_title(&dom),
      Some(format!("{nbsp}Hello{nbsp}"))
    );
  }

  #[test]
  fn trims_whitespace() {
    let dom = parse_html("<html><head><title>  Hello \n</title></head></html>").unwrap();
    assert_eq!(find_document_title(&dom), Some("Hello".to_string()));
  }

  #[test]
  fn returns_first_title_in_head() {
    let dom =
      parse_html("<html><head><title>First</title><title>Second</title></head></html>").unwrap();
    assert_eq!(find_document_title(&dom), Some("First".to_string()));
  }

  #[test]
  fn title_outside_head_does_not_override_head_title() {
    let body_title = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "title".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: vec![DomNode {
        node_type: DomNodeType::Text {
          content: "Body".to_string(),
        },
        children: Vec::new(),
      }],
    };
    let body = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "body".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: vec![body_title],
    };
    let head_title = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "title".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: vec![DomNode {
        node_type: DomNodeType::Text {
          content: "Head".to_string(),
        },
        children: Vec::new(),
      }],
    };
    let head = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "head".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: vec![head_title],
    };
    let html = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "html".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      // Malformed order: body before head.
      children: vec![body, head],
    };
    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![html],
    };

    assert_eq!(find_document_title(&dom), Some("Head".to_string()));
  }

  #[test]
  fn ignores_titles_in_template_subtrees() {
    let dom = parse_html(
      "<html><head><template><title>Bad</title></template><title>Good</title></head></html>",
    )
    .unwrap();
    assert_eq!(find_document_title(&dom), Some("Good".to_string()));

    let dom = parse_html("<html><head><template><title>Bad</title></template></head></html>").unwrap();
    assert_eq!(find_document_title(&dom), None);
  }

  #[test]
  fn ignores_titles_in_shadow_roots() {
    let dom = parse_html(
      "<html><head><title>Good</title></head><body>
        <div id=\"host\"><template shadowroot=\"open\"><title>Bad</title></template></div>
      </body></html>",
    )
    .unwrap();

    assert_eq!(find_document_title(&dom), Some("Good".to_string()));

    let dom = parse_html(
      "<html><head></head><body>
        <div id=\"host\"><template shadowroot=\"open\"><title>Bad</title></template></div>
      </body></html>",
    )
    .unwrap();
    assert_eq!(find_document_title(&dom), None);
  }
}
