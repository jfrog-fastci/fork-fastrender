// META: script=/resources/testharness.js
//
// Curated `Document.adoptNode` checks across documents.

test(() => {
  const doc1 = document;
  const doc2 = document.implementation.createHTMLDocument("t");

  const foreign_el = doc2.createElement("div");
  foreign_el.appendChild(doc2.createTextNode("hello"));
  doc2.body.appendChild(foreign_el);

  assert_equals(foreign_el.ownerDocument, doc2, "sanity: node starts owned by doc2");
  assert_equals(foreign_el.parentNode, doc2.body, "sanity: node starts attached to doc2");

  const adopted = doc1.adoptNode(foreign_el);

  assert_equals(adopted, foreign_el, "adoptNode should return the same node object");
  assert_equals(foreign_el.ownerDocument, doc1, "adopted node should now belong to the adopting document");
  assert_equals(foreign_el.firstChild.ownerDocument, doc1, "adoptNode should also adopt descendants");

  assert_equals(foreign_el.parentNode, null, "adoptNode should remove the node from its old parent");
  assert_equals(doc2.body.childNodes.length, 0, "old document should no longer contain the adopted node");
}, "Document.adoptNode updates ownerDocument and removes the node from the source tree");

