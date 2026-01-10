// META: script=/resources/testharness.js

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  const inner = document.createElement("span");
  inner.className = "inner";
  a.appendChild(inner);

  assert_equals(document.querySelector("#a"), a);
  assert_equals(document.querySelector(".inner"), inner);
}, "Document.querySelector finds matching descendants of the document element");

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.className = "x";
  body.appendChild(a);

  const b = document.createElement("div");
  b.className = "x";
  body.appendChild(b);

  const matches = document.querySelectorAll(".x");
  assert_equals(matches.length, 2);
  assert_equals(matches[0], a);
  assert_equals(matches[1], b);
}, "Document.querySelectorAll returns a NodeList in tree order");

test(() => {
  let threw = false;
  let name = "";
  try {
    document.querySelector("div[");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw);
  assert_equals(name, "SyntaxError");
}, "Document.querySelector throws SyntaxError for invalid selectors");
