use fastrender::dom2::Document;
use fastrender::js::{JsExecutionOptions, RunLimits, WindowHost};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::sync::Arc;
use std::time::Duration;
use vm_js::Value;

#[derive(Debug, Default)]
struct NoFetch;

impl ResourceFetcher for NoFetch {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    Err(Error::Other(format!("unexpected fetch: {url}")))
  }
}

fn js_opts_for_test() -> JsExecutionOptions {
  // `vm-js` budgets are based on wall-clock time. The library default is intentionally aggressive,
  // but under parallel `cargo test` the OS can deschedule a test thread long enough for the VM to
  // observe a false-positive deadline exceed. Use a generous limit to keep integration tests
  // deterministic while still bounding infinite loops.
  let mut opts = JsExecutionOptions::default();
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
  opts
}

#[test]
fn text_encoder_stream_is_exposed_and_has_readable_and_writable() -> Result<()> {
  let dom = Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.com/",
    Arc::new(NoFetch::default()),
    js_opts_for_test(),
  )?;

  let ok = host.exec_script(
    "(function(){\
       if (typeof TextEncoderStream !== 'function') return false;\
       const t = new TextEncoderStream();\
       return typeof t.readable === 'object' && typeof t.writable === 'object';\
     })()",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn text_encoder_stream_pipe_through_encodes_to_utf8() -> Result<()> {
  let dom = Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.com/",
    Arc::new(NoFetch::default()),
    js_opts_for_test(),
  )?;

  host.exec_script(
    r#"
    (function () {
      var g = globalThis;

      // Minimal ReadableStream/TransformStream polyfill for this test binary. When FastRender gains
      // native stream support, these branches will not run and the test will exercise the real
      // implementations.
      if (typeof g.ReadableStream !== "function") {
        function ReadableStream(underlyingSource) {
          this._queue = [];
          this._queueHead = 0;
          this._pendingReads = [];
          this._pendingHead = 0;
          this._closed = false;
          this._errored = false;
          this._error = undefined;
          var self = this;
          this._controller = {
            enqueue: function (chunk) {
              if (self._closed || self._errored) return;
              if (self._pendingHead < self._pendingReads.length) {
                var pr = self._pendingReads[self._pendingHead];
                self._pendingHead = self._pendingHead + 1;
                pr.resolve({ value: chunk, done: false });
              } else {
                self._queue[self._queue.length] = chunk;
              }
            },
            close: function () {
              if (self._closed || self._errored) return;
              self._closed = true;
              while (self._pendingHead < self._pendingReads.length) {
                var pr = self._pendingReads[self._pendingHead];
                self._pendingHead = self._pendingHead + 1;
                pr.resolve({ value: undefined, done: true });
              }
            },
            error: function (e) {
              if (self._closed || self._errored) return;
              self._errored = true;
              self._error = e;
              while (self._pendingHead < self._pendingReads.length) {
                var pr = self._pendingReads[self._pendingHead];
                self._pendingHead = self._pendingHead + 1;
                pr.reject(e);
              }
            },
          };
          if (underlyingSource && typeof underlyingSource.start === "function") {
            underlyingSource.start(this._controller);
          }
        }

        function ReadableStreamDefaultReader(stream) {
          this._stream = stream;
        }
        ReadableStreamDefaultReader.prototype.read = function () {
          var s = this._stream;
          if (s._queueHead < s._queue.length) {
            var value = s._queue[s._queueHead];
            s._queueHead = s._queueHead + 1;
            return Promise.resolve({ value: value, done: false });
          }
          if (s._errored) return Promise.reject(s._error);
          if (s._closed) return Promise.resolve({ value: undefined, done: true });
          return new Promise(function (resolve, reject) {
            s._pendingReads[s._pendingReads.length] = { resolve: resolve, reject: reject };
          });
        };

        ReadableStream.prototype.getReader = function () {
          return new ReadableStreamDefaultReader(this);
        };

        ReadableStream.prototype.pipeThrough = function (transform) {
          var reader = this.getReader();
          function pump() {
            return reader.read().then(function (result) {
              if (result.done) {
                if (transform && transform.writable && typeof transform.writable.close === "function") {
                  return transform.writable.close();
                }
                return;
              }
              return transform.writable.write(result.value).then(pump);
            });
          }
          pump().catch(function (e) {
            try {
              if (
                transform &&
                transform.readable &&
                transform.readable._controller &&
                typeof transform.readable._controller.error === "function"
              ) {
                transform.readable._controller.error(e);
              }
            } catch (_) {}
          });
          return transform.readable;
        };

        g.ReadableStream = ReadableStream;
      }

      if (typeof g.TransformStream !== "function") {
        function TransformStream(transformer) {
          if (!transformer) transformer = {};

          var readable = new g.ReadableStream({});
          var controller = {
            enqueue: function (chunk) {
              readable._controller.enqueue(chunk);
            },
            error: function (e) {
              readable._controller.error(e);
            },
            terminate: function () {
              readable._controller.close();
            },
          };
          var writable = {
            write: function (chunk) {
              try {
                if (typeof transformer.transform === "function") {
                  var r = transformer.transform(chunk, controller);
                  return Promise.resolve(r);
                }
                controller.enqueue(chunk);
                return Promise.resolve();
              } catch (e) {
                controller.error(e);
                return Promise.reject(e);
              }
            },
            close: function () {
              try {
                if (typeof transformer.flush === "function") {
                  var r = transformer.flush(controller);
                  return Promise.resolve(r).then(function () {
                    controller.terminate();
                  });
                }
                controller.terminate();
                return Promise.resolve();
              } catch (e) {
                controller.error(e);
                return Promise.reject(e);
              }
            },
          };

          this.readable = readable;
          this.writable = writable;
        }

        g.TransformStream = TransformStream;
      }
    })();

    globalThis.__out = "";
    globalThis.__err = "";

    const t = new TextEncoderStream();
    const rs = new ReadableStream({ start(c) { c.enqueue("hi"); c.close(); } }).pipeThrough(t);
    const r = rs.getReader();
    r.read()
      .then(({ value }) => { globalThis.__out = new TextDecoder().decode(value); })
      .catch(e => { globalThis.__err = String(e && e.message || e); });
    "#,
  )?;

  host.run_until_idle(RunLimits {
    max_tasks: 10,
    max_microtasks: 100,
    max_wall_time: Some(Duration::from_secs(5)),
  })?;

  let ok = host.exec_script("globalThis.__out === 'hi' && globalThis.__err === ''")?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

