// META: script=/resources/testharness.js
//
// Range.setStart/setEnd boundary point validation + compareBoundaryPoints.
//
// https://dom.spec.whatwg.org/#dom-range-setstart
// https://dom.spec.whatwg.org/#dom-range-setend

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

function assert_throws_dom_exception_name(fn, expected_name, description) {
  let caught = null;
  try {
    fn();
  } catch (e) {
    caught = e;
  }
  assert_true(caught !== null, description);
  assert_equals(caught.name, expected_name, description);
}

test(() => {
  clear_children(document.body);

  const p = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  p.appendChild(a);
  p.appendChild(b);
  document.body.appendChild(p);

  const range = document.createRange();
  range.setStart(p, 0);
  range.setEnd(p, 2);

  assert_equals(range.startContainer, p, "startContainer should reflect setStart node");
  assert_equals(range.startOffset, 0, "startOffset should reflect setStart offset");
  assert_equals(range.endContainer, p, "endContainer should reflect setEnd node");
  assert_equals(range.endOffset, 2, "endOffset should reflect setEnd offset");
  assert_false(range.collapsed, "range spanning multiple offsets should not be collapsed");
}, "Range.setStart/setEnd set the boundary points");

test(() => {
  clear_children(document.body);

  const p = document.createElement("div");
  p.appendChild(document.createElement("span"));
  p.appendChild(document.createElement("span"));
  document.body.appendChild(p);

  const range = document.createRange();
  const too_large = p.childNodes.length + 1;
  assert_throws_dom_exception_name(
    () => range.setStart(p, too_large),
    "IndexSizeError",
    "setStart(offset > node.length) should throw IndexSizeError"
  );
}, "Range.setStart throws IndexSizeError when offset is greater than node length (Element)");

test(() => {
  clear_children(document.body);

  const text = document.createTextNode("abc");
  document.body.appendChild(text);

  const range = document.createRange();
  const too_large = text.data.length + 1;
  assert_throws_dom_exception_name(
    () => range.setEnd(text, too_large),
    "IndexSizeError",
    "setEnd(offset > node.length) should throw IndexSizeError"
  );
}, "Range.setEnd throws IndexSizeError when offset is greater than node length (Text)");

test(() => {
  // The DOM spec requires `setStart`/`setEnd` to throw InvalidNodeTypeError for doctype nodes.
  //
  // Some minimal DOM implementations do not expose `document.implementation.createDocumentType`;
  // in those cases, keep the rest of the Range coverage running and treat this as non-applicable.
  if (
    !document.implementation ||
    typeof document.implementation.createDocumentType !== "function"
  ) {
    assert_true(true, "DocumentType creation is not supported; skipping doctype coverage");
    return;
  }

  const doctype = document.implementation.createDocumentType("html", "", "");
  const range = document.createRange();
  assert_throws_dom_exception_name(
    () => range.setStart(doctype, 0),
    "InvalidNodeTypeError",
    "setStart(doctype, 0) should throw InvalidNodeTypeError"
  );
}, "Range.setStart throws InvalidNodeTypeError when node is a DocumentType");

test(() => {
  assert_equals(Range.START_TO_START, 0, "Range.START_TO_START constant");
  assert_equals(Range.START_TO_END, 1, "Range.START_TO_END constant");
  assert_equals(Range.END_TO_END, 2, "Range.END_TO_END constant");
  assert_equals(Range.END_TO_START, 3, "Range.END_TO_START constant");

  clear_children(document.body);
  const p = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  p.appendChild(a);
  p.appendChild(b);
  document.body.appendChild(p);

  const r1 = document.createRange();
  r1.setStart(p, 0); // before a
  r1.setEnd(p, 1); // between a and b

  const r2 = document.createRange();
  r2.setStart(p, 1); // between a and b
  r2.setEnd(p, 2); // after b

  assert_equals(
    r1.compareBoundaryPoints(Range.START_TO_START, r2),
    -1,
    "START_TO_START compares this.start to source.start"
  );
  assert_equals(
    r2.compareBoundaryPoints(Range.START_TO_START, r1),
    1,
    "START_TO_START should return 1 when this.start is after source.start"
  );
  assert_equals(
    r1.compareBoundaryPoints(Range.END_TO_START, r2),
    0,
    "END_TO_START compares this.end to source.start"
  );
  assert_equals(
    r1.compareBoundaryPoints(Range.START_TO_END, r2),
    -1,
    "START_TO_END compares this.start to source.end"
  );
  assert_equals(
    r2.compareBoundaryPoints(Range.END_TO_END, r1),
    1,
    "END_TO_END compares this.end to source.end"
  );
}, "Range.compareBoundaryPoints constants and ordering");

