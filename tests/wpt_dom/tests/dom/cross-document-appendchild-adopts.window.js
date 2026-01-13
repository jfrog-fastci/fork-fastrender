// META: script=/resources/testharness.js
//
// Cross-document insertion should implicitly adopt nodes into the target document.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const doc1 = document;
  clear_children(doc1.body);

  // Use a real second document so insertion exercises the cross-document adoption path.
  const doc2 = doc1.implementation.createHTMLDocument("t");
  const foreign_el = doc2.createElement("div");
  foreign_el.appendChild(doc2.createTextNode("hello"));

  const returned = doc1.body.appendChild(foreign_el);

  assert_equals(returned, foreign_el, "appendChild should return the inserted node");
  assert_equals(foreign_el.parentNode, doc1.body, "node should become a child of the new parent");

  assert_equals(
    foreign_el.ownerDocument,
    doc1,
    "cross-document appendChild should adopt the node into the target document"
  );
  assert_equals(
    foreign_el.firstChild.ownerDocument,
    doc1,
    "cross-document appendChild should also adopt descendants into the target document"
  );
}, "Node.appendChild adopts a node created in another document");
