// META: script=/resources/testharness.js

test(() => {
  assert_true(typeof HTMLInputElement === "function");
  assert_true(typeof HTMLTextAreaElement === "function");
  assert_true(typeof HTMLSelectElement === "function");
  assert_true(typeof HTMLOptionElement === "function");
  assert_true(typeof HTMLFormElement === "function");
  assert_true(typeof HTMLOptionsCollection === "function");
  assert_true(typeof HTMLFormControlsCollection === "function");
}, "Form control constructors exist");

test(() => {
  const input = document.createElement("input");
  const textarea = document.createElement("textarea");
  const select = document.createElement("select");
  const option = document.createElement("option");
  const form = document.createElement("form");

  assert_true(input instanceof HTMLInputElement);
  assert_true(textarea instanceof HTMLTextAreaElement);
  assert_true(select instanceof HTMLSelectElement);
  assert_true(option instanceof HTMLOptionElement);
  assert_true(form instanceof HTMLFormElement);
}, "document.createElement returns correctly typed form control instances");

test(() => {
  const input = document.createElement("input");

  // value reflects the "value" attribute.
  assert_equals(input.value, "");
  assert_equals(input.getAttribute("value"), null);
  input.value = "abc";
  assert_equals(input.value, "abc");
  assert_equals(input.getAttribute("value"), "abc");

  // checked reflects the boolean "checked" attribute.
  assert_false(input.checked);
  assert_equals(input.getAttribute("checked"), null);
  input.checked = true;
  assert_true(input.checked);
  assert_true(input.getAttribute("checked") !== null);
  input.checked = false;
  assert_false(input.checked);
  assert_equals(input.getAttribute("checked"), null);

  // disabled reflects the boolean "disabled" attribute.
  assert_false(input.disabled);
  assert_equals(input.getAttribute("disabled"), null);
  input.disabled = true;
  assert_true(input.disabled);
  assert_true(input.getAttribute("disabled") !== null);
  input.disabled = false;
  assert_false(input.disabled);
  assert_equals(input.getAttribute("disabled"), null);
}, "HTMLInputElement.value/checked/disabled roundtrip and reflect attributes");

test(() => {
  const ta = document.createElement("textarea");
  assert_true(ta instanceof HTMLTextAreaElement);

  assert_equals(ta.value, "");
  ta.value = "hello";
  assert_equals(ta.value, "hello");
  assert_equals(ta.textContent, "hello");
}, "HTMLTextAreaElement.value reflects to textContent");

test(() => {
  const opt = document.createElement("option");
  opt.textContent = "Hello";
  assert_equals(
    opt.value,
    "Hello",
    "option.value falls back to textContent when no value attribute",
  );

  opt.value = "v";
  assert_equals(opt.getAttribute("value"), "v");
  assert_equals(opt.value, "v");
}, "HTMLOptionElement.value basics");

test(() => {
  const select = document.createElement("select");
  const o1 = document.createElement("option");
  o1.value = "a";
  o1.textContent = "A";
  const o2 = document.createElement("option");
  o2.value = "b";
  o2.textContent = "B";

  const options1 = select.options;
  const options2 = select.options;
  assert_equals(options1, options2, "options is [SameObject]");
  assert_true(options1 instanceof HTMLOptionsCollection);
  assert_true(options1 instanceof HTMLCollection);
  assert_equals(options1.length, 0);

  select.appendChild(o1);
  assert_equals(options1.length, 1);
  assert_equals(options1[0], o1);
  assert_equals(options1.item(0), o1);

  select.appendChild(o2);
  assert_equals(options1.length, 2);
  assert_equals(options1[1], o2);

  const spread = [...options1];
  assert_equals(spread.length, 2);
  assert_equals(spread[0], o1);
  assert_equals(spread[1], o2);
}, "HTMLSelectElement.options is a live [SameObject] HTMLOptionsCollection");

test(() => {
  const select = document.createElement("select");
  const o1 = document.createElement("option");
  o1.value = "a";
  o1.textContent = "A";
  const o2 = document.createElement("option");
  o2.value = "b";
  o2.textContent = "B";
  select.appendChild(o1);
  select.appendChild(o2);

  assert_equals(select.selectedIndex, 0);
  assert_equals(select.value, "a");

  select.selectedIndex = 1;
  assert_equals(select.selectedIndex, 1);
  assert_equals(select.value, "b");
  assert_equals(o1.getAttribute("selected"), null);
  assert_true(o2.getAttribute("selected") !== null);

  select.value = "a";
  assert_equals(select.selectedIndex, 0);
  assert_equals(select.value, "a");
  assert_true(o1.getAttribute("selected") !== null);
  assert_equals(o2.getAttribute("selected"), null);
}, "HTMLSelectElement.value and selectedIndex reflect option selection");

test(() => {
  const form = document.createElement("form");
  const input = document.createElement("input");
  const select = document.createElement("select");
  const textarea = document.createElement("textarea");

  const elements1 = form.elements;
  const elements2 = form.elements;
  assert_equals(elements1, elements2, "elements is [SameObject]");
  assert_true(elements1 instanceof HTMLFormControlsCollection);
  assert_true(elements1 instanceof HTMLCollection);
  assert_equals(elements1.length, 0);

  form.appendChild(input);
  assert_equals(elements1.length, 1);
  assert_equals(elements1[0], input);

  form.appendChild(select);
  assert_equals(elements1.length, 2);
  assert_equals(elements1[1], select);

  form.appendChild(textarea);
  assert_equals(elements1.length, 3);
  assert_equals(elements1[2], textarea);

  form.removeChild(select);
  assert_equals(elements1.length, 2);
  assert_equals(elements1[0], input);
  assert_equals(elements1[1], textarea);

  assert_equals(typeof form.submit, "function");
  assert_equals(typeof form.reset, "function");
  form.submit();
  form.reset();
}, "HTMLFormElement.elements is live and submit/reset are callable");
