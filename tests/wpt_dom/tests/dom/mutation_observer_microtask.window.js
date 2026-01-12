// META: script=/resources/testharness.js
//
// Curated MutationObserver scheduling coverage for FastRender's offline WPT DOM corpus.
// Focus: notification happens as a microtask (before timers) and coalesces multiple mutations.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

var mutation_observer_callback_calls = 0;
var mutation_observer_callback_records_length = 0;
var mutation_observer_callback_this_ok = false;

function mutation_observer_microtask_callback(records, observer) {
  if (mutation_observer_callback_calls === 0) {
    mutation_observer_callback_calls = 1;
  } else {
    mutation_observer_callback_calls = 2;
  }
  mutation_observer_callback_records_length = records.length;
  mutation_observer_callback_this_ok = this === observer;
}

function check_mutation_observer_microtask_state() {
  assert_equals(
    mutation_observer_callback_calls,
    1,
    "MutationObserver callback should be invoked once"
  );
  assert_equals(
    mutation_observer_callback_records_length,
    2,
    "MutationObserver callback should receive all records from the task"
  );
  assert_true(
    mutation_observer_callback_this_ok,
    "MutationObserver callback this should match the observer argument"
  );
}

function run_mutation_observer_microtask_test(t) {
  clear_children(document.body);

  mutation_observer_callback_calls = 0;
  mutation_observer_callback_records_length = 0;
  mutation_observer_callback_this_ok = false;

  const container = document.createElement("div");
  document.body.appendChild(container);

  const a = document.createElement("span");
  const b = document.createElement("span");

  const observer = new MutationObserver(mutation_observer_microtask_callback);
  observer.observe(container, { childList: true });

  container.appendChild(a);
  container.appendChild(b);

  // The MutationObserver notify microtask is queued at the time of mutation; since this microtask
  // is queued after the mutations, it must observe the delivered state.
  queueMicrotask(t.step_func(check_mutation_observer_microtask_state));

  // Timers are tasks and should run after microtasks (including MutationObserver delivery).
  setTimeout(t.step_func_done(check_mutation_observer_microtask_state), 0);
}

async_test(
  run_mutation_observer_microtask_test,
  "MutationObserver notification is a microtask and coalesces multiple mutations"
);

