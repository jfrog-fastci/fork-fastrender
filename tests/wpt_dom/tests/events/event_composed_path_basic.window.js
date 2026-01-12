// META: script=/resources/testharness.js
//
// Curated checks for `Event.prototype.composedPath()` on the vm-js backend.

var event_composed_path_basic_parent = null;
var event_composed_path_basic_child = null;
var event_composed_path_basic_saw_listener = false;

function event_composed_path_basic_listener(e) {
  event_composed_path_basic_saw_listener = true;

  var path1 = e.composedPath();
  assert_equals(path1[0], event_composed_path_basic_child, "first entry is the dispatch target");
  assert_true(
    path1.indexOf(event_composed_path_basic_parent) !== -1,
    "path should include the parent element"
  );
  assert_equals(path1[path1.length - 1], window, "path should end with window");

  var doc_index = path1.indexOf(document);
  var win_index = path1.indexOf(window);
  assert_true(doc_index !== -1, "path should include document");
  assert_true(win_index !== -1, "path should include window");
  assert_true(doc_index < win_index, "document should appear before window");

  // Each call must return a new array.
  var path2 = e.composedPath();
  assert_true(path1 !== path2, "repeated composedPath() calls must return distinct arrays");
}

function event_composed_path_basic_test() {
  event_composed_path_basic_saw_listener = false;

  var parent = document.createElement("div");
  var child = document.createElement("span");
  event_composed_path_basic_parent = parent;
  event_composed_path_basic_child = child;

  // Attach the subtree so the composed path includes document + window.
  document.body.appendChild(parent);
  parent.appendChild(child);

  child.addEventListener("x", event_composed_path_basic_listener);

  var ev = new Event("x", { bubbles: true });
  child.dispatchEvent(ev);
  assert_true(event_composed_path_basic_saw_listener, "listener should have run");

  // The computed path must still be observable after dispatch completes.
  var after1 = ev.composedPath();
  assert_equals(after1[0], child, "after dispatch: first entry is the dispatch target");
  assert_true(
    after1.indexOf(parent) !== -1,
    "after dispatch: path should include the parent element"
  );
  assert_equals(after1[after1.length - 1], window, "after dispatch: path should end with window");
  assert_true(after1.indexOf(document) !== -1, "after dispatch: path should include document");
  assert_true(
    after1.indexOf(document) < after1.indexOf(window),
    "after dispatch: document should appear before window"
  );

  var after2 = ev.composedPath();
  assert_true(after1 !== after2, "after dispatch: composedPath() must return a new array");

  // Even when there are no listeners, composedPath() should work after dispatch.
  child.removeEventListener("x", event_composed_path_basic_listener);
  var ev2 = new Event("x", { bubbles: true });
  child.dispatchEvent(ev2);
  var no_listener_path = ev2.composedPath();
  assert_equals(no_listener_path[0], child, "no listeners: first entry is the dispatch target");
  assert_true(
    no_listener_path.indexOf(parent) !== -1,
    "no listeners: path should include the parent element"
  );
  assert_equals(
    no_listener_path[no_listener_path.length - 1],
    window,
    "no listeners: path should end with window"
  );
  assert_true(
    no_listener_path.indexOf(document) < no_listener_path.indexOf(window),
    "no listeners: document should appear before window"
  );

  document.body.removeChild(parent);
}

test(
  event_composed_path_basic_test,
  "Event.prototype.composedPath returns target→...→window and remains available after dispatch"
);
