use fastrender::dom2::Document;
use fastrender::js::{JsExecutionOptions, WindowHost};
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
fn data_transfer_items_and_files_surfaces_exist() -> Result<()> {
  let dom = Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.invalid/",
    Arc::new(NoFetch::default()),
    js_opts_for_test(),
  )?;

  host.exec_script(
    r#"
      globalThis.__ok = false;
      globalThis.__err = "";

      const el = document.createElement("div");
      el.addEventListener("dragstart", (ev) => {
        try {
          if (!ev || !ev.dataTransfer) throw new Error("missing dataTransfer");
          const dt = ev.dataTransfer;

          if (typeof dt.items === "undefined") throw new Error("missing dataTransfer.items");
          if (dt.items !== dt.items) throw new Error("dataTransfer.items is not SameObject");

          // Should not throw.
          dt.items.add("x", "text/plain");

          const files = dt.files;
          if (!(Array.isArray(files) || typeof files.length === "number")) {
            throw new Error("dataTransfer.files is not array-like");
          }
          if (files !== dt.files) throw new Error("dataTransfer.files is not SameObject");

          // Common pattern: `files.length` should not throw.
          const len = files.length;
          if (typeof len !== "number") throw new Error("files.length is not a number");

          globalThis.__ok = true;
        } catch (e) {
          globalThis.__ok = false;
          globalThis.__err = String(e && e.message ? e.message : e);
        }
      });

      const ev = new Event("dragstart");
      ev.dataTransfer = new DataTransfer();
      el.dispatchEvent(ev);
    "#,
  )?;

  let ok = host.exec_script("globalThis.__ok")?;
  if ok != Value::Bool(true) {
    let err = host.exec_script("globalThis.__err")?;
    panic!("expected __ok === true, got {ok:?}, err={err:?}");
  }
  Ok(())
}

