// META: script=/resources/testharness.js
//
// Cross-document insertion should implicitly adopt nodes into the target document.
//
// Use a real second document so we exercise true multi-document adoption behavior.
test(() => {
  const doc1 = document;
  const doc2 = doc1.implementation.createHTMLDocument("t");

  const foreign_el = doc2.createElement("div");
  assert_equals(foreign_el.ownerDocument, doc2, "sanity: element should initially belong to doc2 wrapper");

  const inserted = doc1.body.insertAdjacentElement("beforeend", foreign_el);

  assert_equals(inserted, foreign_el, "insertAdjacentElement should return the inserted node");
  assert_equals(foreign_el.parentNode, doc1.body, "node should be inserted under document.body");
  assert_equals(
    foreign_el.ownerDocument,
    doc1,
    "cross-document insertAdjacentElement should adopt the node into the target document"
  );
}, "Element.insertAdjacentElement adopts a node created in another document");
