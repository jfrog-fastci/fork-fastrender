// META: script=/resources/testharness.js

"use strict";

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(b);
  document.body.appendChild(parent);

  assert_equals(
    a.compareDocumentPosition(b),
    Node.DOCUMENT_POSITION_FOLLOWING,
    "later sibling should be following"
  );
  assert_equals(
    b.compareDocumentPosition(a),
    Node.DOCUMENT_POSITION_PRECEDING,
    "earlier sibling should be preceding"
  );

  document.body.removeChild(parent);
}, "Node.compareDocumentPosition() returns PRECEDING/FOLLOWING for siblings");

test(() => {
  const ancestor = document.createElement("div");
  const descendant = document.createElement("span");
  ancestor.appendChild(descendant);
  document.body.appendChild(ancestor);

  assert_equals(
    ancestor.compareDocumentPosition(descendant),
    Node.DOCUMENT_POSITION_CONTAINED_BY | Node.DOCUMENT_POSITION_FOLLOWING,
    "descendant should be following and contained by ancestor"
  );
  assert_equals(
    descendant.compareDocumentPosition(ancestor),
    Node.DOCUMENT_POSITION_CONTAINS | Node.DOCUMENT_POSITION_PRECEDING,
    "ancestor should be preceding and contain descendant"
  );

  document.body.removeChild(ancestor);
}, "Node.compareDocumentPosition() returns CONTAINS/CONTAINED_BY for ancestor relations");

test(() => {
  const a = document.createElement("div");
  const b = document.createElement("div");
  const pos = a.compareDocumentPosition(b);

  const disconnected =
    Node.DOCUMENT_POSITION_DISCONNECTED |
    Node.DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC;
  assert_equals(pos & disconnected, disconnected);

  // Disconnected nodes are not in an ancestor/descendant relationship.
  assert_equals(
    pos &
      (Node.DOCUMENT_POSITION_CONTAINS | Node.DOCUMENT_POSITION_CONTAINED_BY),
    0
  );

  // Spec: disconnected comparisons must include either PRECEDING or FOLLOWING (implementation-specific).
  assert_true(
    (pos & (Node.DOCUMENT_POSITION_PRECEDING | Node.DOCUMENT_POSITION_FOLLOWING)) !== 0
  );
}, "Node.compareDocumentPosition() sets DISCONNECTED|IMPLEMENTATION_SPECIFIC for disconnected nodes");

test(() => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const shadow = host.attachShadow({ mode: "open" });
  const inner = document.createElement("span");
  shadow.appendChild(inner);

  const pos = host.compareDocumentPosition(inner);
  const disconnected =
    Node.DOCUMENT_POSITION_DISCONNECTED |
    Node.DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC;
  assert_equals(pos & disconnected, disconnected);

  document.body.removeChild(host);
}, "Node.compareDocumentPosition() treats shadow trees as separate trees");

test(() => {
  const otherDoc = document.implementation.createHTMLDocument("");
  const a = document.createElement("div");
  const b = otherDoc.createElement("div");

  const pos = a.compareDocumentPosition(b);
  const disconnected =
    Node.DOCUMENT_POSITION_DISCONNECTED |
    Node.DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC;
  assert_equals(pos & disconnected, disconnected);
}, "Node.compareDocumentPosition() treats nodes from different Documents as disconnected");
