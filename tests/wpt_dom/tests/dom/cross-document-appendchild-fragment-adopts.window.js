// META: script=/resources/testharness.js
//
// Cross-document insertion should implicitly adopt nodes into the target document.
//
// Use a real second document so we exercise true multi-document adoption behavior.
//
// DocumentFragment insertion is special: the fragment itself stays detached and is emptied, while its
// children are moved/adopted into the destination document.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const doc1 = document;
  clear_children(doc1.body);

  const doc2 = doc1.implementation.createHTMLDocument("t");

  const parent = doc1.createElement("div");
  doc1.body.appendChild(parent);

  const frag = doc2.createDocumentFragment();
  assert_equals(frag.ownerDocument, doc2, "fragment should initially belong to doc2 wrapper");

  const a = doc2.createElement("span");
  const b = doc2.createElement("span");
  frag.appendChild(a);
  frag.appendChild(b);

  const returned = parent.appendChild(frag);
  assert_equals(returned, frag, "appendChild(fragment) should return the fragment");

  assert_equals(frag.childNodes.length, 0, "fragment should be emptied after insertion");
  assert_equals(parent.childNodes.length, 2, "fragment children should be inserted into the parent");

  assert_equals(a.ownerDocument, doc1, "inserted children should be adopted into the target document");
  assert_equals(b.ownerDocument, doc1, "inserted children should be adopted into the target document");
  assert_equals(a.parentNode, parent, "inserted children should become children of the new parent");
  assert_equals(b.parentNode, parent, "inserted children should become children of the new parent");

  // Fragment itself stays owned by its original document wrapper.
  assert_equals(frag.ownerDocument, doc2, "fragment ownerDocument should not change during insertion");
}, "Node.appendChild adopts children from a foreign DocumentFragment");
