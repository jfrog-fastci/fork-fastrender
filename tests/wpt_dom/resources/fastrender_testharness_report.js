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
// Note: the upstream reporter collects results via `add_result_callback` and then de-duplicates
// subtests at completion time. FastRender's minimal `testharness.js` always surfaces one test record
// per registered subtest, so we can construct the full subtest list from the completion callback
// input and avoid per-result bookkeeping.

// Stash the original host hook (if any) and replace it with a single-shot wrapper so:
// - tests that call `__fastrender_wpt_report(...)` directly still work,
// - the offline runner observes exactly one payload per file even if called multiple times.
//
// NOTE: Some minimal JS engines (including FastRender's vm-js WPT harness) do not fully mirror
// global object properties into the global lexical environment. Be defensive and check both the
// identifier binding and the `globalThis` property form.
var __fastrender_wpt_reporter_original_report = null;
try {
  if (typeof __fastrender_wpt_report === "function") {
    __fastrender_wpt_reporter_original_report = __fastrender_wpt_report;
  } else if (
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

// Best-effort JSON serialization for report payloads.
//
// Some FastRender JS embeddings do not provide a working `JSON.stringify` implementation. The WPT
// HTML runner needs a stable string representation so it can read the final report out of the DOM.
// Keep this conservative and deterministic.
function __fastrender_wpt_json_escape_string(value) {
  var s = __fastrender_wpt_safe_string(value);
  var out = ['"'];
  var hex = "0123456789abcdef";
  for (var i = 0; i !== s.length; i++) {
    var code = 0;
    try {
      code = s.charCodeAt(i);
    } catch (_e) {
      code = 0;
    }
    // `"` and `\`
    if (code === 34) {
      out.push('\\"');
      continue;
    }
    if (code === 92) {
      out.push("\\\\");
      continue;
    }
    // Control characters.
    if (code === 8) {
      out.push("\\b");
      continue;
    }
    if (code === 12) {
      out.push("\\f");
      continue;
    }
    if (code === 10) {
      out.push("\\n");
      continue;
    }
    if (code === 13) {
      out.push("\\r");
      continue;
    }
    if (code === 9) {
      out.push("\\t");
      continue;
    }
    if (code < 32) {
      out.push("\\u00");
      out.push(hex.charAt((code >> 4) & 15));
      out.push(hex.charAt(code & 15));
      continue;
    }
    out.push(s.charAt(i));
  }
  out.push('"');
  return out.join("");
}

function __fastrender_wpt_json_serialize_maybe_string(value) {
  if (value === null) return "null";
  return __fastrender_wpt_json_escape_string(value);
}

function __fastrender_wpt_json_serialize_subtests(subtests) {
  var out = ["["];
  if (subtests && typeof subtests.length === "number") {
    for (var i = 0; i !== subtests.length; i++) {
      var st = subtests[i];
      if (i !== 0) out.push(",");
      out.push("{");
      if (st && typeof st === "object") {
        out.push('"name":');
        out.push(__fastrender_wpt_json_escape_string(st.name));
        out.push(',"status":');
        out.push(__fastrender_wpt_json_escape_string(st.status));
        if (st.message !== undefined) {
          out.push(',"message":');
          out.push(__fastrender_wpt_json_serialize_maybe_string(st.message));
        }
        if (st.stack !== undefined) {
          out.push(',"stack":');
          out.push(__fastrender_wpt_json_serialize_maybe_string(st.stack));
        }
      } else {
        out.push('"name":"(invalid subtest)","status":"error"');
      }
      out.push("}");
    }
  }
  out.push("]");
  return out.join("");
}

function __fastrender_wpt_json_serialize_payload(payload) {
  // Prefer native JSON.stringify when available and working.
  try {
    if (typeof JSON !== "undefined" && JSON && typeof JSON.stringify === "function") {
      var native = JSON.stringify(payload);
      if (typeof native === "string" && native !== "") {
        return native;
      }
    }
  } catch (_e) {}

  // Manual serialization fallback (deterministic, only supports the report payload shape).
  try {
    var out = ["{"];
    out.push('"file_status":');
    out.push(__fastrender_wpt_json_escape_string(payload && payload.file_status));
    out.push(',"harness_status":');
    out.push(__fastrender_wpt_json_escape_string(payload && payload.harness_status));
    if (payload && payload.message !== undefined) {
      out.push(',"message":');
      out.push(__fastrender_wpt_json_serialize_maybe_string(payload.message));
    }
    if (payload && payload.stack !== undefined) {
      out.push(',"stack":');
      out.push(__fastrender_wpt_json_serialize_maybe_string(payload.stack));
    }
    out.push(',"subtests":');
    out.push(__fastrender_wpt_json_serialize_subtests(payload && payload.subtests));
    out.push("}");
    return out.join("");
  } catch (_e2) {}

  // Final fallback: emit a minimal error payload so the runner doesn't hang.
  return '{"file_status":"error","harness_status":"error","message":"failed to serialize WPT report payload","subtests":[]}';
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

function __fastrender_wpt_emit_payload(payload) {
  if (__fastrender_wpt_reporter_reported === true) return;
  __fastrender_wpt_reporter_reported = true;

  // For browser debugging, keep the last payload around.
  try {
    if (__fastrender_wpt_global) {
      __fastrender_wpt_global.__fastrender_wpt_last_payload = payload;
    }
  } catch (_e) {}

  // Persist the report payload into the DOM for runners that cannot read JS globals directly
  // (e.g. BrowserTab-based HTML execution).
  //
  // This is best-effort and must never throw: reporter failures should surface as harness errors,
  // not crash the test.
  try {
    if (
      typeof document !== "undefined" &&
      document &&
      document.documentElement &&
      typeof document.documentElement.setAttribute === "function"
    ) {
      var json = __fastrender_wpt_json_serialize_payload(payload);
      document.documentElement.setAttribute(
        "data-fastrender-wpt-report",
        json
      );
    }
  } catch (_e_dom) {}

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
// Also mirror onto the global binding for runtimes where `globalThis.__fastrender_wpt_report`
// does not affect `__fastrender_wpt_report` identifier resolution.
try {
  __fastrender_wpt_report = __fastrender_wpt_report_wrapper;
} catch (_e4) {}

// -----------------------------
// WPT callback plumbing
// -----------------------------

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

    // Fast path: if the harness is OK and all tests passed, emit a minimal payload without
    // enumerating/serializing every subtest. This keeps large passing WPT files (hundreds of
    // subtests with long names) within the runner's long timeout on the vm-js backend.
    if (harness_status_str === "ok") {
      var all_pass = true;
      if (tests && typeof tests.length === "number") {
        for (var k = 0; k !== tests.length; k++) {
          var t0 = tests[k];
          if (!t0) continue;
          if (t0.status !== 0) {
            all_pass = false;
            break;
          }
        }
      }
      if (all_pass === true) {
        var fast_payload = {
          file_status: "pass",
          harness_status: "ok",
          subtests: []
        };
        if (hs_message !== undefined) fast_payload.message = hs_message;
        if (hs_stack !== undefined) fast_payload.stack = hs_stack;
        if (__fastrender_wpt_global && typeof __fastrender_wpt_global.__fastrender_wpt_report === "function") {
          __fastrender_wpt_global.__fastrender_wpt_report(fast_payload);
        } else {
          __fastrender_wpt_emit_payload(fast_payload);
        }
        return;
      }
    }

    // Build the full subtest list from the completion callback input.
    var subtests = [];
    if (tests && typeof tests.length === "number") {
      for (var i = 0; i !== tests.length; i++) {
        var t = tests[i];
        if (!t) continue;

        var name = "";
        if (t.name !== undefined) {
          name = __fastrender_wpt_safe_string(t.name);
        } else {
          name = __fastrender_wpt_safe_string(t);
        }

        var status_code = t.status;
        var status = __fastrender_wpt_map_subtest_status(status_code);
        var st = { name: name, status: status };

        var msg = __fastrender_wpt_optional_string(t.message);
        var st_stack = __fastrender_wpt_optional_string(t.stack);
        if (msg !== undefined) st.message = msg;
        if (st_stack !== undefined) st.stack = st_stack;

        subtests.push(st);
      }
    }

    var file_status = "pass";
    if (harness_status_str === "timeout") {
      file_status = "timeout";
    } else if (harness_status_str !== "ok") {
      file_status = "error";
    } else {
      for (var j = 0; j !== subtests.length; j++) {
        var st2 = subtests[j];
        if (st2 && st2.status !== "pass") {
          file_status = "fail";
          break;
        }
      }
    }

    var payload = {
      file_status: file_status,
      harness_status: harness_status_str,
      subtests: subtests
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
  if (typeof add_completion_callback === "function") {
    add_completion_callback(__fastrender_wpt_completion_callback);
  } else if (__fastrender_wpt_global && typeof __fastrender_wpt_global.add_completion_callback === "function") {
    __fastrender_wpt_global.add_completion_callback(__fastrender_wpt_completion_callback);
  }
} catch (_e2) {}
