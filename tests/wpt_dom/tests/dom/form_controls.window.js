// META: script=/resources/testharness.js

test(() => {
  const i = document.createElement("input");
  assert_equals(i.getAttribute("value"), null);

  i.value = "x";
  assert_equals(i.getAttribute("value"), null, "setting .value must not set the value attribute");
  assert_equals(i.value, "x");

  i.setAttribute("value", "y");
  assert_equals(i.getAttribute("value"), "y");
  assert_equals(i.value, "x", "value attribute changes must not affect dirty value");
}, "HTMLInputElement.value is internal (dirty value flag) and does not reflect to the value attribute");

test(() => {
  const i = document.createElement("input");
  i.setAttribute("value", "y");
  assert_equals(i.value, "y");

  i.setAttribute("value", "z");
  assert_equals(i.value, "z", "while not dirty, value follows the content attribute");

  i.removeAttribute("value");
  assert_equals(i.value, "", "removing the value attribute resets the default value when not dirty");
}, "HTMLInputElement.value follows the value attribute while not dirty");

test(() => {
  const i = document.createElement("input");
  i.setAttribute("type", "checkbox");
  assert_equals(i.getAttribute("checked"), null);
  assert_equals(i.checked, false);

  i.checked = true;
  assert_equals(i.getAttribute("checked"), null, "setting .checked must not set the checked attribute");
  assert_equals(i.checked, true);

  i.setAttribute("checked", "");
  assert_equals(i.getAttribute("checked"), "");
  assert_equals(i.checked, true, "checked attribute changes must not affect dirty checkedness");
}, "HTMLInputElement.checked is internal (dirty checkedness flag) and does not reflect to the checked attribute");

test(() => {
  const i = document.createElement("input");
  i.setAttribute("type", "checkbox");

  i.setAttribute("checked", "");
  assert_equals(i.checked, true);
  i.removeAttribute("checked");
  assert_equals(i.checked, false);
}, "HTMLInputElement.checked follows the checked attribute while not dirty (checkbox/radio only)");

test(() => {
  const ta = document.createElement("textarea");
  ta.textContent = "default";
  assert_equals(ta.value, "default");

  ta.value = "x";
  assert_equals(ta.value, "x");
  assert_equals(
    ta.textContent,
    "default",
    "setting textarea.value must not rewrite descendant text nodes"
  );

  ta.textContent = "new default";
  assert_equals(ta.value, "x", "default value changes must not affect dirty value");
}, "HTMLTextAreaElement.value is internal (dirty value flag) and does not reflect to textContent");

test(() => {
  const ta = document.createElement("textarea");
  ta.textContent = "a";
  assert_equals(ta.value, "a");
  ta.textContent = "b";
  assert_equals(ta.value, "b", "while not dirty, value tracks descendant text");
}, "HTMLTextAreaElement.value tracks descendant text while not dirty");

test(() => {
  const form = document.createElement("form");

  const i = document.createElement("input");
  i.setAttribute("value", "y");

  const cb = document.createElement("input");
  cb.setAttribute("type", "checkbox");
  cb.setAttribute("checked", "");

  const ta = document.createElement("textarea");
  ta.textContent = "d";

  form.appendChild(i);
  form.appendChild(cb);
  form.appendChild(ta);

  i.value = "x";
  cb.checked = false;
  ta.value = "t";

  form.reset();

  assert_equals(i.value, "y");
  assert_equals(cb.checked, true);
  assert_equals(ta.value, "d");
}, "HTMLFormElement.reset restores default values and clears dirty flags");
