// META: script=/resources/testharness.js

test(() => {
  const parent = document.createElement("div");

  const first = document.createElement("span");
  first.setAttribute("name", "x");

  const second = document.createElement("span");
  second.id = "x";

  const third = document.createElement("span");
  third.id = "y";

  parent.appendChild(first);
  parent.appendChild(second);
  parent.appendChild(third);

  const collection = parent.children;
  assert_equals(typeof collection.namedItem, "function", "HTMLCollection.namedItem should be a function");

  assert_equals(collection.namedItem(""), null, "empty string should return null");

  // Per WHATWG DOM, search is in tree order and matches either ID or (HTML) name attribute.
  assert_equals(collection.namedItem("x"), first, "should return the first matching element in tree order");
  assert_equals(collection.namedItem("y"), third, "should match ID");
  assert_equals(collection.namedItem("missing"), null, "missing name should return null");
}, "HTMLCollection.prototype.namedItem matches element id or name attribute");

