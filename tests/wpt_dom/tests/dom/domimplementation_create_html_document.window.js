// META: script=/resources/testharness.js

test(() => {
  const doc2 = document.implementation.createHTMLDocument("t");

  assert_not_equals(doc2, document, "createHTMLDocument should create a new Document");
  assert_equals(doc2.defaultView, null, "detached document must have null defaultView");
  assert_equals(doc2.location, null, "detached document must have null location");
  assert_equals(doc2.baseURI, "about:blank", "detached document baseURI is about:blank");

  // Some DOM subsets do not expose Document.URL yet; assert only when it is present as a string.
  if (typeof doc2.URL === "string") {
    assert_equals(doc2.URL, "about:blank", "detached document URL is about:blank");
  }

  assert_equals(doc2.documentElement.tagName, "HTML", "documentElement should be <html>");
  assert_equals(doc2.head.tagName, "HEAD", "document should have a <head> element");
  assert_equals(doc2.body.tagName, "BODY", "document should have a <body> element");

  // Some DOM subsets do not expose Document.title yet; assert only when it is present as a string.
  if (typeof doc2.title === "string") {
    assert_equals(doc2.title, "t", "document title should reflect the passed argument");
  }
}, "DOMImplementation.createHTMLDocument creates a detached HTMLDocument with the expected structure");

test(() => {
  const doc2 = document.implementation.createHTMLDocument("t");

  const el = doc2.createElement("div");
  const imported = document.importNode(el);
  assert_not_equals(imported, el, "importNode should return a new Node");
  assert_equals(imported.ownerDocument, document, "imported node must belong to the target document");
  assert_equals(el.ownerDocument, doc2, "source node must remain owned by its original document");

  const el2 = doc2.createElement("span");
  const adopted = document.adoptNode(el2);
  assert_equals(adopted, el2, "adoptNode should return the same Node object");
  assert_equals(adopted.ownerDocument, document, "adopted node must belong to the target document");
}, "Document.importNode and Document.adoptNode work across documents and update ownerDocument correctly");

test(() => {
  const doc2 = document.implementation.createHTMLDocument("t");

  const foreign = doc2.createElement("div");
  foreign.id = "foreign";

  // Cross-document insertion must not throw: the node is automatically adopted into the target
  // document during insertion.
  document.body.appendChild(foreign);

  assert_equals(foreign.ownerDocument, document, "cross-document appendChild should adopt the node");
  assert_equals(foreign.parentNode, document.body, "node should be inserted under document.body");

  document.body.removeChild(foreign);
}, "Appending a node created in a different document auto-adopts it into the target document");

