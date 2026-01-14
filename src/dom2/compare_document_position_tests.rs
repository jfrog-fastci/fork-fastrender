#![cfg(test)]

use super::{parse_html, Document, NodeKind};
use selectors::context::QuirksMode;

const DOCUMENT_POSITION_DISCONNECTED: u16 = 0x01;
const DOCUMENT_POSITION_PRECEDING: u16 = 0x02;
const DOCUMENT_POSITION_FOLLOWING: u16 = 0x04;
const DOCUMENT_POSITION_CONTAINS: u16 = 0x08;
const DOCUMENT_POSITION_CONTAINED_BY: u16 = 0x10;
const DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: u16 = 0x20;

#[test]
fn compare_document_position_orders_siblings() {
  let html = "<!doctype html><html><body><div id=parent><span id=a></span><span id=b></span></div></body></html>";
  let doc: Document = parse_html(html).unwrap();

  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();

  assert_eq!(
    doc.compare_document_position(a, b),
    DOCUMENT_POSITION_FOLLOWING,
    "later sibling should be following"
  );
  assert_eq!(
    doc.compare_document_position(b, a),
    DOCUMENT_POSITION_PRECEDING,
    "earlier sibling should be preceding"
  );
}

#[test]
fn compare_document_position_orders_ancestor_descendant() {
  let html = "<!doctype html><html><body><div id=ancestor><span id=descendant></span></div></body></html>";
  let doc: Document = parse_html(html).unwrap();

  let ancestor = doc.get_element_by_id("ancestor").unwrap();
  let descendant = doc.get_element_by_id("descendant").unwrap();

  assert_eq!(
    doc.compare_document_position(ancestor, descendant),
    DOCUMENT_POSITION_CONTAINED_BY | DOCUMENT_POSITION_FOLLOWING,
    "descendant should be following and contained by ancestor"
  );
  assert_eq!(
    doc.compare_document_position(descendant, ancestor),
    DOCUMENT_POSITION_CONTAINS | DOCUMENT_POSITION_PRECEDING,
    "ancestor should be preceding and contain descendant"
  );
}

#[test]
fn compare_document_position_marks_disconnected_nodes() {
  let mut doc: Document = Document::new(QuirksMode::NoQuirks);

  let a = doc.create_element("div", "");
  let b = doc.create_element("div", "");

  let disconnected_mask = DOCUMENT_POSITION_DISCONNECTED | DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC;

  let pos = doc.compare_document_position(a, b);
  assert_eq!(pos & disconnected_mask, disconnected_mask);
  assert_eq!(pos & (DOCUMENT_POSITION_CONTAINS | DOCUMENT_POSITION_CONTAINED_BY), 0);
  assert_ne!(pos & (DOCUMENT_POSITION_PRECEDING | DOCUMENT_POSITION_FOLLOWING), 0);

  let pos_rev = doc.compare_document_position(b, a);
  assert_eq!(pos_rev & disconnected_mask, disconnected_mask);
  assert_eq!(pos_rev & (DOCUMENT_POSITION_CONTAINS | DOCUMENT_POSITION_CONTAINED_BY), 0);
  assert_ne!(pos_rev & (DOCUMENT_POSITION_PRECEDING | DOCUMENT_POSITION_FOLLOWING), 0);

  // Ordering is implementation-specific, but it must be consistent in both directions.
  let dir = pos & (DOCUMENT_POSITION_PRECEDING | DOCUMENT_POSITION_FOLLOWING);
  let dir_rev = pos_rev & (DOCUMENT_POSITION_PRECEDING | DOCUMENT_POSITION_FOLLOWING);
  assert_ne!(dir, dir_rev);
  assert_eq!(dir | dir_rev, DOCUMENT_POSITION_PRECEDING | DOCUMENT_POSITION_FOLLOWING);
}

#[test]
fn compare_document_position_treats_shadow_trees_as_disconnected() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowrootmode=open>",
    "<span id=inside></span>",
    "</template>",
    "</div>",
    "</body></html>",
  );
  let doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").unwrap();

  let shadow_root = doc.node(host).children[0];
  assert!(
    matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }),
    "expected host to have an attached ShadowRoot"
  );
  let inside = doc.node(shadow_root).children[0];

  let pos = doc.compare_document_position(host, inside);
  let disconnected_mask = DOCUMENT_POSITION_DISCONNECTED | DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC;
  assert_eq!(pos & disconnected_mask, disconnected_mask);
}

