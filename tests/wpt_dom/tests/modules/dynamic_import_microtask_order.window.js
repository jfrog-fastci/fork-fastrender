// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated event-loop ordering test: `import()` returns a Promise, and its reaction jobs must run in
// the same microtask queue as `queueMicrotask` (and therefore before timers).

promise_test(
  () => {
    const log = [];

    queueMicrotask(() => {
      log.push("before");
    });

    import("/resources/mod_basic.js").then(
      () => {
        log.push("import");
      },
      (err) => {
        throw err;
      }
    );

    queueMicrotask(() => {
      log.push("after");
    });

    return new Promise((resolve, reject) => {
      setTimeout(() => {
        try {
          assert_equals(log.join(","), "before,import,after");
          resolve();
        } catch (e) {
          reject(e);
        }
      }, 0);
    });
  },
  "import() promise reactions integrate with the microtask queue"
);
