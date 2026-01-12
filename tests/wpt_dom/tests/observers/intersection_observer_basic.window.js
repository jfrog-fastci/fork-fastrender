// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated observer API test: IntersectionObserver basic shape + delivery semantics.
//
// This is intentionally geometry-independent so it can run under FastRender's offline DOM runner
// (WindowHostState has no real layout engine). We only assert that the observer machinery exists
// and delivers at least one entry asynchronously after `observe()`.

test(() => {
  assert_equals(typeof IntersectionObserver, "function");
}, "IntersectionObserver constructor exists");

test(() => {
  let threw = false;
  try {
    // WebIDL constructors must throw when invoked without `new`.
    IntersectionObserver(function () {});
  } catch (_e) {
    threw = true;
  }
  assert_true(threw, "constructor call without new should throw");
}, "IntersectionObserver constructor requires 'new'");

promise_test(() => {
  const target = document.createElement("div");
  document.body.appendChild(target);

  let called = false;
  let resolveCb;
  let rejectCb;
  const p = new Promise((resolve, reject) => {
    resolveCb = resolve;
    rejectCb = reject;
  });

  const observer = new IntersectionObserver(function (entries, obs) {
    called = true;
    try {
      assert_true(Array.isArray(entries), "entries should be an array");
      assert_true(entries.length > 0, "entries should contain at least one entry");
      assert_equals(obs, observer, "observer arg should be the observer instance");
      assert_equals(this, observer, "`this` should be the observer instance");

      // Geometry-independent surface: target identity is stable.
      assert_equals(entries[0].target, target, "entry.target should be the observed element");

      // `takeRecords()` should return an array and drain it.
      const r1 = observer.takeRecords();
      assert_true(Array.isArray(r1), "takeRecords should return an array");
      const r2 = observer.takeRecords();
      assert_true(Array.isArray(r2), "takeRecords should return an array");
      assert_equals(r2.length, 0, "second takeRecords call should drain the queue");
    } catch (e) {
      rejectCb(e);
      return;
    }

    // `disconnect()` should prevent delivery of callbacks that were queued before disconnect.
    let disconnectedCalled = false;
    const observer2 = new IntersectionObserver(function () {
      disconnectedCalled = true;
    });
    observer2.observe(target);
    observer2.disconnect();

    setTimeout(() => {
      try {
        assert_false(disconnectedCalled, "disconnect() should prevent queued callback delivery");
        resolveCb();
      } catch (e) {
        rejectCb(e);
      }
    }, 0);
  });

  observer.observe(target);
  assert_false(called, "callback should be asynchronous");

  return p;
}, "IntersectionObserver observe delivers entries asynchronously, takeRecords exists, and disconnect prevents callbacks");

