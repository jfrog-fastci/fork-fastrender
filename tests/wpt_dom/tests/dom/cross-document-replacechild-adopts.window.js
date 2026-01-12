// META: script=/resources/testharness.js
//
// Cross-document insertion should implicitly adopt nodes into the target document.
//
// Note: FastRender's vm-js DOM shim does not yet implement `document.implementation.createHTMLDocument()`
// (true multi-document). Use `Object.create(document)` to create a second Document wrapper ID that
// shares the same underlying `dom2::Document` arena. This still exercises the cross-document wrapper
// adoption/remapping path.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const doc1 = document;
  clear_children(doc1.body);

  const doc2 = Object.create(doc1);

  const parent = doc1.createElement("div");
  const old_child = doc1.createElement("span");
  parent.appendChild(old_child);
  doc1.body.appendChild(parent);

  const foreign_el = doc2.createElement("b");
  foreign_el.appendChild(doc2.createTextNode("hello"));

  const returned = parent.replaceChild(foreign_el, old_child);

  assert_equals(returned, old_child, "replaceChild should return the removed node");
  assert_equals(foreign_el.parentNode, parent, "new node should become a child of the parent");
  assert_equals(old_child.parentNode, null, "old node should be detached");

  assert_equals(
    foreign_el.ownerDocument,
    doc1,
    "cross-document replaceChild should adopt the node into the target document"
  );
  assert_equals(
    foreign_el.firstChild.ownerDocument,
    doc1,
    "cross-document replaceChild should also adopt descendants into the target document"
  );
}, "Node.replaceChild adopts a node created in another document");

