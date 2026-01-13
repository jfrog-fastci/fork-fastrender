// META: script=/resources/testharness.js
//
// Basic `Node.getRootNode()` coverage for shadow DOM composed vs non-composed behavior.

test(() => {
  assert_equals(document.body.getRootNode(), document, "document.body.getRootNode() should be the document");
  assert_equals(
    document.body.getRootNode({ composed: true }),
    document,
    "document.body.getRootNode({composed:true}) should be the document"
  );
}, "Node.getRootNode returns document for light DOM");

test(() => {
  while (document.body.firstChild) {
    document.body.removeChild(document.body.firstChild);
  }

  const host = document.createElement("div");
  document.body.appendChild(host);

  // Declarative shadow DOM is processed during `innerHTML` parsing.
  host.innerHTML = "<template shadowroot=open><span id=child></span></template>";

  // Spec: open shadow roots are exposed via `host.shadowRoot`, not via `firstChild/childNodes`.
  const shadowRoot = host.shadowRoot;
  assert_true(!!shadowRoot, "host.shadowRoot should exist for an open shadow root");

  const shadowChild = shadowRoot.firstChild;
  assert_true(!!shadowChild, "shadow root should have a child node");

  assert_equals(shadowChild.getRootNode(), shadowRoot, "non-composed getRootNode returns the ShadowRoot");
  assert_equals(
    shadowChild.getRootNode({ composed: true }),
    document,
    "composed getRootNode returns the document when the host is connected"
  );

  assert_equals(shadowRoot.getRootNode(), shadowRoot, "ShadowRoot.getRootNode() returns itself");
  assert_equals(
    shadowRoot.getRootNode({ composed: true }),
    document,
    "ShadowRoot.getRootNode({composed:true}) returns the document when the host is connected"
  );
}, "Node.getRootNode returns shadow root vs document inside shadow DOM");
