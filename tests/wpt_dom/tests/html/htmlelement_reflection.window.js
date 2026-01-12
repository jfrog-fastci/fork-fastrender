// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof HTMLElement, "function");

  const div = document.createElement("div");
  assert_true(div instanceof HTMLElement, "div should be instanceof HTMLElement");
  assert_true(div instanceof Element, "div should be instanceof Element");
  assert_true(div instanceof Node, "div should be instanceof Node");
}, "HTMLElement exists and participates in the Node/Element inheritance chain");

test(() => {
  const div = document.createElement("div");

  assert_false(div.hidden, "hidden should default to false");

  div.hidden = true;
  assert_true(div.hidden, "hidden getter should reflect the property setter");
  assert_true(
    div.getAttribute("hidden") !== null,
    "setting hidden should add the hidden attribute"
  );

  div.hidden = false;
  assert_false(div.hidden, "hidden getter should reflect the property setter");
  assert_equals(
    div.getAttribute("hidden"),
    null,
    "clearing hidden should remove the hidden attribute"
  );
}, "HTMLElement.hidden reflects the hidden content attribute");

test(() => {
  const div = document.createElement("div");

  assert_equals(div.title, "", "title should default to empty string");
  div.title = "hello";
  assert_equals(div.title, "hello");
  assert_equals(div.getAttribute("title"), "hello");

  assert_equals(div.lang, "", "lang should default to empty string");
  div.lang = "en";
  assert_equals(div.lang, "en");
  assert_equals(div.getAttribute("lang"), "en");

  assert_equals(div.dir, "", "dir should default to empty string");
  div.dir = "rtl";
  assert_equals(div.dir, "rtl");
  assert_equals(div.getAttribute("dir"), "rtl");
}, "HTMLElement title/lang/dir reflect to attributes");

test(() => {
  const div = document.createElement("div");

  assert_equals(typeof div.style, "object");

  div.style.cssText = "color: red;";
  assert_equals(div.getAttribute("style"), "color: red;");

  div.style.setProperty("background-color", "blue");
  assert_equals(div.style.getPropertyValue("background-color"), "blue");
}, "HTMLElement.style reflects to the style attribute and supports setProperty/getPropertyValue");

