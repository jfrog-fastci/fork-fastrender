// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated ordering test for module top-level await (TLA):
// - The synchronous portion of module evaluation runs immediately during `import()`.
// - The `await` continuation runs as a Promise job (microtask).
// - The `import()` promise reaction is queued after other already-queued microtasks.

promise_test(
  () => {
    const log = [];
    globalThis.__log = log;

    queueMicrotask(() => log.push("before"));

    const module_url =
      "data:text/javascript;base64," +
      // Source:
      //   globalThis.__log.push("module-start");
      //   await 0;
      //   globalThis.__log.push("module-after-await");
      //   export default 1;
      "Z2xvYmFsVGhpcy5fX2xvZy5wdXNoKCJtb2R1bGUtc3RhcnQiKTsKYXdhaXQgMDsKZ2xvYmFsVGhpcy5fX2xvZy5wdXNoKCJtb2R1bGUtYWZ0ZXItYXdhaXQiKTsKZXhwb3J0IGRlZmF1bHQgMTsK";

    import(module_url).then(() => log.push("import"));

    queueMicrotask(() => log.push("after"));

    return new Promise((resolve, reject) => {
      setTimeout(() => {
        try {
          assert_equals(
            log.join(","),
            "module-start,before,module-after-await,after,import"
          );
          resolve();
        } catch (e) {
          reject(e);
        }
      }, 0);
    });
  },
  "import() integrates top-level await with the microtask queue"
);

