// FastRender WPT reporter shim.
//
// This script bridges WPT's `testharness.js` reporting callbacks into FastRender's offline runner
// contract:
//
//   globalThis.__fastrender_wpt_report(payload)
//
// The runner defines `__fastrender_wpt_report` as a host hook. We must:
// - collect subtest results via `add_result_callback`,
// - emit exactly one deterministic plain-object payload via `add_completion_callback`,
// - never throw (reporter failures should be surfaced as a harness-level error payload).
//
// NOTE: Keep this file compatible with the in-tree vm-js backend (avoid relying on JSON.stringify
// and keep the syntax conservative).

// -----------------------------
// Global state + helpers
// -----------------------------

var __fastrender_wpt_global = undefined;
try {
  if (typeof globalThis !== "undefined") {
    __fastrender_wpt_global = globalThis;
  } else if (typeof window !== "undefined") {
    __fastrender_wpt_global = window;
  } else if (typeof self !== "undefined") {
    __fastrender_wpt_global = self;
  }
} catch (_e) {
  // Leave `__fastrender_wpt_global` as-is.
}

// Reporter state is kept in globals so callbacks don't rely on closure capture (vm-js historically
// had limited JS semantics).
var __fastrender_wpt_reporter_reported = false;
var __fastrender_wpt_reporter_subtests = [];
var __fastrender_wpt_reporter_seen = {};

// Stash the original host hook (if any) and replace it with a single-shot wrapper so:
// - tests that call `__fastrender_wpt_report(...)` directly still work,
// - the offline runner observes exactly one payload per file even if called multiple times.
var __fastrender_wpt_reporter_original_report = null;
try {
  if (
    __fastrender_wpt_global &&
    typeof __fastrender_wpt_global.__fastrender_wpt_report === "function"
  ) {
    __fastrender_wpt_reporter_original_report =
      __fastrender_wpt_global.__fastrender_wpt_report;
  }
} catch (_e2) {
  __fastrender_wpt_reporter_original_report = null;
}

// Best-effort safe string coercion: tolerate Symbols and objects with throwing `toString`.
function __fastrender_wpt_safe_string(value) {
  try {
    // Avoid relying on a global `String(...)` binding (not provided by the vm-js backend).
    //
    // `Array.prototype.join` is a native shim in vm-js and stringifies elements internally.
    return ["", value].join("");
  } catch (_e) {
    try {
      return "[unstringifiable]";
    } catch (_e2) {
      return "";
    }
  }
}

function __fastrender_wpt_optional_string(value) {
  if (value === undefined || value === null) return undefined;
  var s = __fastrender_wpt_safe_string(value);
  if (s === "") return undefined;
  return s;
}

function __fastrender_wpt_unknown_status(value) {
  // Prefer raw numeric codes when available.
  if (typeof value === "number" && value === value) {
    // Use Array#join to avoid relying on `+` semantics in very small JS engines.
    return ["unknown(", value, ")"].join("");
  }
  var s = __fastrender_wpt_safe_string(value);
  if (s === "") s = "unknown";
  return ["unknown(", s, ")"].join("");
}

function __fastrender_wpt_map_subtest_status(code) {
  // WPT constants: PASS=0, FAIL=1, TIMEOUT=2, NOTRUN=3.
  if (code === 0) return "pass";
  if (code === 1) return "fail";
  if (code === 2) return "timeout";
  if (code === 3) return "notrun";
  return __fastrender_wpt_unknown_status(code);
}

function __fastrender_wpt_map_harness_status(code) {
  // Upstream harness status codes: OK=0, ERROR=1, TIMEOUT=2.
  if (code === 0) return "ok";
  if (code === 1) return "error";
  if (code === 2) return "timeout";
  return __fastrender_wpt_unknown_status(code);
}

function __fastrender_wpt_mark_seen(name) {
  // Prefix keys to avoid `__proto__` surprises.
  __fastrender_wpt_reporter_seen[["$", name].join("")] = true;
}

function __fastrender_wpt_is_seen(name) {
  return __fastrender_wpt_reporter_seen[["$", name].join("")] === true;
}

function __fastrender_wpt_emit_payload(payload) {
  if (__fastrender_wpt_reporter_reported === true) return;
  __fastrender_wpt_reporter_reported = true;

  // For browser debugging, keep the last payload around.
  try {
    if (__fastrender_wpt_global) {
      __fastrender_wpt_global.__fastrender_wpt_last_payload = payload;
    }
  } catch (_e) {}

  try {
    if (__fastrender_wpt_reporter_original_report) {
      __fastrender_wpt_reporter_original_report(payload);
      return;
    }
  } catch (_e2) {}

  // Browser fallback: log to console if the host hook isn't present.
  try {
    if (__fastrender_wpt_global && __fastrender_wpt_global.console && __fastrender_wpt_global.console.log) {
      __fastrender_wpt_global.console.log(payload);
    }
  } catch (_e3) {}
}

function __fastrender_wpt_report_wrapper(payload) {
  // Allow direct callers (curated smoke tests) to short-circuit the file.
  __fastrender_wpt_emit_payload(payload);
}

// Install the wrapper hook exactly once.
try {
  if (__fastrender_wpt_global) {
    __fastrender_wpt_global.__fastrender_wpt_report = __fastrender_wpt_report_wrapper;
  }
} catch (_e) {}

// -----------------------------
// WPT callback plumbing
// -----------------------------

function __fastrender_wpt_result_callback(test) {
  if (__fastrender_wpt_reporter_reported === true) return;
  try {
    var name = "";
    var status_code = undefined;
    var message = undefined;
    var stack = undefined;

    if (test && typeof test === "object") {
      if (test.name !== undefined) name = __fastrender_wpt_safe_string(test.name);
      if (test.status !== undefined) status_code = test.status;
      if (test.message !== undefined) message = __fastrender_wpt_optional_string(test.message);
      if (test.stack !== undefined) stack = __fastrender_wpt_optional_string(test.stack);
    } else {
      name = __fastrender_wpt_safe_string(test);
    }

    var status = __fastrender_wpt_map_subtest_status(status_code);

    var subtest = { name: name, status: status };
    if (message !== undefined) subtest.message = message;
    if (stack !== undefined) subtest.stack = stack;

    __fastrender_wpt_reporter_subtests.push(subtest);
    __fastrender_wpt_mark_seen(name);
  } catch (e) {
    // If reporting fails, surface it as a harness-level error and stop.
    var err_payload = {
      file_status: "error",
      harness_status: "error",
      message: __fastrender_wpt_safe_string(e),
      subtests: []
    };
    try {
      if (__fastrender_wpt_global && typeof __fastrender_wpt_global.__fastrender_wpt_report === "function") {
        __fastrender_wpt_global.__fastrender_wpt_report(err_payload);
      } else {
        __fastrender_wpt_emit_payload(err_payload);
      }
    } catch (_e2) {
      __fastrender_wpt_emit_payload(err_payload);
    }
  }
}

function __fastrender_wpt_completion_callback(tests, harness_status) {
  if (__fastrender_wpt_reporter_reported === true) return;
  try {
    var hs_code = 0;
    var hs_message = undefined;
    var hs_stack = undefined;

    if (harness_status && typeof harness_status === "object") {
      if (harness_status.status !== undefined) hs_code = harness_status.status;
      if (harness_status.message !== undefined) hs_message = __fastrender_wpt_optional_string(harness_status.message);
      if (harness_status.stack !== undefined) hs_stack = __fastrender_wpt_optional_string(harness_status.stack);
    } else if (harness_status !== undefined) {
      hs_code = harness_status;
    }

    var harness_status_str = __fastrender_wpt_map_harness_status(hs_code);

    // Ensure subtests is complete: add any tests from the completion list that did not produce a
    // result callback (e.g. NOTRUN cases).
    if (tests && typeof tests.length === "number") {
      for (var i = 0; i !== tests.length; i++) {
        var t = tests[i];
        if (!t) continue;
        var name = "";
        if (t.name !== undefined) {
          name = __fastrender_wpt_safe_string(t.name);
        }
        if (__fastrender_wpt_is_seen(name)) continue;

        var status_code = t.status;
        var status = __fastrender_wpt_map_subtest_status(status_code);
        var st = { name: name, status: status };

        var msg = __fastrender_wpt_optional_string(t.message);
        var st_stack = __fastrender_wpt_optional_string(t.stack);
        if (msg !== undefined) st.message = msg;
        if (st_stack !== undefined) st.stack = st_stack;

        __fastrender_wpt_reporter_subtests.push(st);
        __fastrender_wpt_mark_seen(name);
      }
    }

    var file_status = "pass";
    if (harness_status_str === "timeout") {
      file_status = "timeout";
    } else if (harness_status_str !== "ok") {
      file_status = "error";
    } else {
      for (var j = 0; j !== __fastrender_wpt_reporter_subtests.length; j++) {
        var st2 = __fastrender_wpt_reporter_subtests[j];
        if (st2 && st2.status !== "pass") {
          file_status = "fail";
          break;
        }
      }
    }

    var payload = {
      file_status: file_status,
      harness_status: harness_status_str,
      subtests: __fastrender_wpt_reporter_subtests
    };
    if (hs_message !== undefined) payload.message = hs_message;
    if (hs_stack !== undefined) payload.stack = hs_stack;

    if (__fastrender_wpt_global && typeof __fastrender_wpt_global.__fastrender_wpt_report === "function") {
      __fastrender_wpt_global.__fastrender_wpt_report(payload);
    } else {
      __fastrender_wpt_emit_payload(payload);
    }
  } catch (e) {
    var err_payload2 = {
      file_status: "error",
      harness_status: "error",
      message: __fastrender_wpt_safe_string(e),
      subtests: []
    };
    try {
      if (__fastrender_wpt_global && typeof __fastrender_wpt_global.__fastrender_wpt_report === "function") {
        __fastrender_wpt_global.__fastrender_wpt_report(err_payload2);
      } else {
        __fastrender_wpt_emit_payload(err_payload2);
      }
    } catch (_e3) {
      __fastrender_wpt_emit_payload(err_payload2);
    }
  }
}

// Register callbacks with upstream-ish `testharness.js` if present.
try {
  if (typeof add_result_callback === "function") {
    add_result_callback(__fastrender_wpt_result_callback);
  } else if (__fastrender_wpt_global && typeof __fastrender_wpt_global.add_result_callback === "function") {
    __fastrender_wpt_global.add_result_callback(__fastrender_wpt_result_callback);
  }
} catch (_e) {}

try {
  if (typeof add_completion_callback === "function") {
    add_completion_callback(__fastrender_wpt_completion_callback);
  } else if (__fastrender_wpt_global && typeof __fastrender_wpt_global.add_completion_callback === "function") {
    __fastrender_wpt_global.add_completion_callback(__fastrender_wpt_completion_callback);
  }
} catch (_e2) {}
