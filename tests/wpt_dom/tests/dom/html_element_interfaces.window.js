// META: script=/resources/testharness.js
//
// Minimal HTMLElement + form element interface coverage (constructors/prototypes + global attrs).

test(() => {
  assert_equals(typeof HTMLElement, "function", "HTMLElement should be a constructor function");
  assert_true(!!HTMLElement.prototype, "HTMLElement.prototype should exist");
}, "HTMLElement constructor exists");

test(() => {
  const div = document.createElement("div");
  assert_true(div instanceof HTMLElement, "div should be an instance of HTMLElement");
}, "document.createElement('div') instanceof HTMLElement");

test(() => {
  const input = document.createElement("input");
  assert_true(input instanceof HTMLInputElement, "input should be an instance of HTMLInputElement");
  assert_true(input instanceof HTMLElement, "input should be an instance of HTMLElement");
}, "HTMLInputElement instanceof checks");

test(() => {
  const select = document.createElement("select");
  assert_true(select instanceof HTMLSelectElement, "select should be an instance of HTMLSelectElement");
  assert_true(select instanceof HTMLElement, "select should be an instance of HTMLElement");
}, "HTMLSelectElement instanceof checks");

test(() => {
  const textarea = document.createElement("textarea");
  assert_true(textarea instanceof HTMLTextAreaElement, "textarea should be an instance of HTMLTextAreaElement");
  assert_true(textarea instanceof HTMLElement, "textarea should be an instance of HTMLElement");
}, "HTMLTextAreaElement instanceof checks");

test(() => {
  const form = document.createElement("form");
  assert_true(form instanceof HTMLFormElement, "form should be an instance of HTMLFormElement");
  assert_true(form instanceof HTMLElement, "form should be an instance of HTMLElement");
}, "HTMLFormElement instanceof checks");

test(() => {
  const option = document.createElement("option");
  assert_true(option instanceof HTMLOptionElement, "option should be an instance of HTMLOptionElement");
  assert_true(option instanceof HTMLElement, "option should be an instance of HTMLElement");
}, "HTMLOptionElement instanceof checks");

test(() => {
  const el = document.createElement("div");
  assert_equals(el.hidden, false, "hidden should default to false");
  assert_equals(el.getAttribute("hidden"), null, "hidden content attribute should be missing by default");

  el.hidden = true;
  assert_equals(el.hidden, true, "setting hidden=true should reflect to IDL attribute");
  assert_true(el.getAttribute("hidden") !== null, "hidden content attribute should be present after setting true");

  el.hidden = false;
  assert_equals(el.hidden, false, "setting hidden=false should reflect to IDL attribute");
  assert_equals(el.getAttribute("hidden"), null, "hidden content attribute should be removed after setting false");
}, "HTMLElement.hidden reflects to the hidden attribute");

test(() => {
  const el = document.createElement("div");

  assert_equals(el.title, "", "title should default to empty string");
  assert_equals(el.getAttribute("title"), null, "title attribute should be missing by default");
  el.title = "hello";
  assert_equals(el.title, "hello");
  assert_equals(el.getAttribute("title"), "hello");
  el.removeAttribute("title");
  assert_equals(el.title, "", "removing title attribute should reset the IDL property");

  assert_equals(el.lang, "", "lang should default to empty string");
  assert_equals(el.getAttribute("lang"), null, "lang attribute should be missing by default");
  el.lang = "en";
  assert_equals(el.lang, "en");
  assert_equals(el.getAttribute("lang"), "en");
  el.removeAttribute("lang");
  assert_equals(el.lang, "", "removing lang attribute should reset the IDL property");

  assert_equals(el.dir, "", "dir should default to empty string");
  assert_equals(el.getAttribute("dir"), null, "dir attribute should be missing by default");
  el.dir = "rtl";
  assert_equals(el.dir, "rtl");
  assert_equals(el.getAttribute("dir"), "rtl");
  el.removeAttribute("dir");
  assert_equals(el.dir, "", "removing dir attribute should reset the IDL property");
}, "HTMLElement.title/lang/dir reflect to attributes");
