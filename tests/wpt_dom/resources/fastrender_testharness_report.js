// FastRender WPT `testharness.js` reporter shim.
//
// Upstream WPT's `testharnessreport.js` is aimed at browser UI output. FastRender
// runs tests in an offline CLI harness and needs a deterministic, machine-readable
// payload emitted exactly once.
//
// Host integration contract:
//   - The runner defines `globalThis.__fastrender_wpt_report = function(payload) { ... }`
//   - This script calls `__fastrender_wpt_report(payload)` once, where `payload` is a
//     plain object suitable for JSON.stringify.
(function () {
  // Idempotency guard in case the resource is included multiple times.
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (g.__fastrender_testharness_report_installed) return;
  g.__fastrender_testharness_report_installed = true;

  function requireHostFn(name) {
    if (typeof g[name] !== "function") {
      throw new Error(
        "fastrender_testharness_report.js must be loaded after testharness.js (missing " +
          name +
          ")"
      );
    }
    return g[name];
  }

  // Provided by WPT's `testharness.js`.
  var add_result_callback = requireHostFn("add_result_callback");
  var add_completion_callback = requireHostFn("add_completion_callback");

  var collected_subtests = [];
  var did_report = false;

  function numConst(name, fallback) {
    var v = g[name];
    return typeof v === "number" ? v : fallback;
  }

  // Test status codes (per-subtest).
  // Prefer using the constants created by testharness.js, but fall back to the
  // well-known numeric values.
  var TEST_STATUS = {
    PASS: numConst("PASS", 0),
    FAIL: numConst("FAIL", 1),
    TIMEOUT: numConst("TIMEOUT", 2),
    NOTRUN: numConst("NOTRUN", 3),
    PRECONDITION_FAILED: numConst("PRECONDITION_FAILED", 4)
  };

  // Harness status codes (file-level harness infrastructure status).
  var HARNESS_STATUS = {
    OK: numConst("OK", 0),
    ERROR: numConst("ERROR", 1),
    TIMEOUT: numConst("TIMEOUT", 2)
  };

  function unknownLabel(code) {
    // Keep the string stable and explicit. Avoid JSON "undefined" by stringifying.
    return "unknown(" + String(code) + ")";
  }

  function mapTestStatus(code) {
    if (code === TEST_STATUS.PASS) return "pass";
    if (code === TEST_STATUS.FAIL) return "fail";
    if (code === TEST_STATUS.TIMEOUT) return "timeout";
    if (code === TEST_STATUS.NOTRUN) return "notrun";
    if (code === TEST_STATUS.PRECONDITION_FAILED) return "precondition_failed";
    return unknownLabel(code);
  }

  function mapHarnessStatus(code) {
    if (code === HARNESS_STATUS.OK) return "ok";
    if (code === HARNESS_STATUS.ERROR) return "error";
    if (code === HARNESS_STATUS.TIMEOUT) return "timeout";
    return unknownLabel(code);
  }

  function stringOrNull(value) {
    if (value === undefined || value === null) return null;
    if (typeof value === "string") return value;
    return String(value);
  }

  function normalizeSubtest(test) {
    return {
      name:
        test && typeof test.name === "string"
          ? test.name
          : test && test.name !== undefined
            ? String(test.name)
            : "",
      status: mapTestStatus(test ? test.status : undefined),
      message: stringOrNull(test ? test.message : undefined),
      stack: stringOrNull(test ? test.stack : undefined)
    };
  }

  function computeFileStatus(harnessStatusStr, subtests) {
    if (harnessStatusStr === "timeout") return "timeout";
    if (harnessStatusStr === "error") return "error";
    if (harnessStatusStr.indexOf("unknown(") === 0) return "error";

    var saw_timeout = false;
    var saw_nonpass = false;
    for (var i = 0; i < subtests.length; i++) {
      var st = subtests[i].status;
      if (st === "timeout") saw_timeout = true;
      if (st !== "pass") saw_nonpass = true;
    }
    if (saw_timeout) return "timeout";
    if (saw_nonpass) return "fail";
    return "pass";
  }

  function reportOnce(payload) {
    if (did_report) return;
    did_report = true;

    var report_fn = g.__fastrender_wpt_report;
    if (typeof report_fn !== "function") {
      throw new Error(
        "Missing host hook: globalThis.__fastrender_wpt_report(payload)"
      );
    }
    report_fn(payload);
  }

  add_result_callback(function (test) {
    if (did_report) return;
    collected_subtests.push(normalizeSubtest(test));
  });

  add_completion_callback(function (tests, harness_status) {
    if (did_report) return;

    // Ensure we always report something; if we fail to build the payload, emit
    // a minimal ERROR payload rather than throwing and causing the runner to hang.
    try {
      var subtests;
      if (tests && typeof tests.length === "number") {
        subtests = [];
        for (var i = 0; i < tests.length; i++) {
          subtests.push(normalizeSubtest(tests[i]));
        }
      } else {
        // Fall back to the order provided by add_result_callback.
        subtests = collected_subtests.slice();
      }

      var harness_status_str = mapHarnessStatus(
        harness_status ? harness_status.status : undefined
      );

      reportOnce({
        file_status: computeFileStatus(harness_status_str, subtests),
        harness_status: harness_status_str,
        message: stringOrNull(harness_status ? harness_status.message : undefined),
        stack: stringOrNull(harness_status ? harness_status.stack : undefined),
        subtests: subtests
      });
    } catch (e) {
      reportOnce({
        file_status: "error",
        harness_status: "error",
        message: stringOrNull(e && e.message ? e.message : e),
        stack: stringOrNull(e && e.stack),
        subtests: collected_subtests.slice()
      });
    }
  });
})();
