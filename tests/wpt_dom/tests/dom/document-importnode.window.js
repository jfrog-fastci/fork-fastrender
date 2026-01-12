// META: script=/resources/testharness.js
//
// Curated `Document.importNode` checks across documents.

test(() => {
  const doc1 = document;
  const doc2 = document.implementation.createHTMLDocument("t");

  const foreign_el = doc2.createElement("div");
  foreign_el.setAttribute("data-x", "y");
  foreign_el.appendChild(doc2.createTextNode("hello"));

  const imported = doc1.importNode(foreign_el, true);

  assert_true(imported !== foreign_el, "importNode should return a new node");
  assert_equals(imported.ownerDocument, doc1, "imported node should belong to the importing document");
  assert_equals(imported.getAttribute("data-x"), "y", "imported attributes should be preserved");

  assert_equals(imported.firstChild.nodeType, Node.TEXT_NODE, "expected imported child to be a Text node");
  assert_equals(imported.firstChild.data, "hello", "expected imported Text data to be preserved");
  assert_equals(
    imported.firstChild.ownerDocument,
    doc1,
    "imported descendants should also belong to the importing document"
  );

  assert_equals(foreign_el.ownerDocument, doc2, "original node should remain owned by the source document");
  assert_equals(foreign_el.firstChild.ownerDocument, doc2, "original descendants should remain owned by the source document");
}, "Document.importNode deep-clones nodes across documents");
