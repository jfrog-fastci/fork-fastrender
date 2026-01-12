// META: script=/resources/testharness.js
//
// Range.createContextualFragment() fragment parsing semantics.
//
// These tests align with WHATWG DOM + HTML fragment parsing rules, including:
// - using the parent Element when the range start is inside a Text node, and
// - falling back to a synthetic <body> context when the start node is not an Element (e.g. Document).
//
// https://dom.spec.whatwg.org/#dom-range-createcontextualfragment

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const table = document.createElement("table");
  const text = document.createTextNode("context");
  table.appendChild(text);
  document.body.appendChild(table);

  const range = document.createRange();
  range.setStart(text, 1);
  range.setEnd(text, 1);

  const frag = range.createContextualFragment("<tr><td>x</td></tr>");
  assert_true(
    frag instanceof DocumentFragment,
    "createContextualFragment should return a DocumentFragment"
  );

  // Parsing <tr> in a <table> context should produce a tbody/tr/td subtree (no <table> wrapper).
  assert_true(frag.querySelector("tbody") !== null, "expected a <tbody> element in the fragment");
  const td = frag.querySelector("td");
  assert_true(td !== null, "expected a <td> element in the fragment");
  assert_equals(td.textContent, "x", "expected <td> to contain the parsed text");
  assert_equals(frag.querySelector("table"), null, "unexpected <table> wrapper in table-context parsing");
}, "Range.createContextualFragment uses the parent element when the start node is Text");

test(() => {
  clear_children(document.body);

  // Initial Range boundary points are (document, 0); this should trigger the spec's body fallback
  // parsing context.
  const range = document.createRange();
  const frag = range.createContextualFragment("<tr><td>x</td></tr>");

  // In the HTML "in body" insertion mode, table-row tags are parse errors and are ignored, leaving
  // only their text content.
  assert_equals(frag.textContent, "x", "expected body-context parsing to drop <tr>/<td> and keep text");
  assert_equals(frag.querySelector("tbody"), null, "unexpected <tbody> when parsing in body context");
  assert_equals(frag.querySelector("td"), null, "unexpected <td> when parsing in body context");
}, "Range.createContextualFragment falls back to body when the start node is Document");

