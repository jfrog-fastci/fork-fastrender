use super::Harness;
use fastrender::js::RunLimits;
use fastrender::Result;
use std::collections::HashMap;

#[test]
fn dynamic_external_script_executes_as_script_task() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;
  h.set_external_script_sources(HashMap::from([(
    "/a.js".to_string(),
    "if (!document.currentScript || document.currentScript.getAttribute('id') !== 'dyn') throw new Error('bad currentScript'); console.log('a');".to_string(),
  )]));

  h.exec_script(
    r#"
      var s = document.createElement("script");
      s.setAttribute("id", "dyn");
      s.setAttribute("src", "/a.js");
      document.body.appendChild(s);
      console.log("after-append");
    "#,
  )?;

  // Insertion should only schedule a fetch; no synchronous execution.
  assert_eq!(h.take_log(), vec!["after-append".to_string()]);

  // Fetch completion should queue a task (still not execute synchronously).
  h.complete_external_script("/a.js")?;
  assert!(h.take_log().is_empty(), "expected execution to be task-queued");

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["a".to_string()]);
  Ok(())
}

#[test]
fn dynamic_external_script_via_insert_before_executes_as_script_task() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;
  h.set_external_script_sources(HashMap::from([(
    "/a.js".to_string(),
    "if (!document.currentScript || document.currentScript.getAttribute('id') !== 'dyn') throw new Error('bad currentScript'); console.log('a');".to_string(),
  )]));

  h.exec_script(
    r#"
      var ref = document.createElement("div");
      ref.setAttribute("id", "ref");
      document.body.appendChild(ref);

      var s = document.createElement("script");
      s.setAttribute("id", "dyn");
      s.setAttribute("src", "/a.js");
      document.body.insertBefore(s, ref);
      console.log("after-insert");
    "#,
  )?;

  assert_eq!(h.take_log(), vec!["after-insert".to_string()]);

  h.complete_external_script("/a.js")?;
  assert!(h.take_log().is_empty(), "expected execution to be task-queued");

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["a".to_string()]);
  Ok(())
}

#[test]
fn dynamic_external_script_via_replace_child_executes_as_script_task() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;
  h.set_external_script_sources(HashMap::from([(
    "/a.js".to_string(),
    "if (!document.currentScript || document.currentScript.getAttribute('id') !== 'dyn') throw new Error('bad currentScript'); console.log('a');".to_string(),
  )]));

  h.exec_script(
    r#"
      var oldChild = document.createElement("div");
      document.body.appendChild(oldChild);

      var s = document.createElement("script");
      s.setAttribute("id", "dyn");
      s.setAttribute("src", "/a.js");
      document.body.replaceChild(s, oldChild);
      console.log("after-replace");
    "#,
  )?;

  assert_eq!(h.take_log(), vec!["after-replace".to_string()]);

  h.complete_external_script("/a.js")?;
  assert!(h.take_log().is_empty(), "expected execution to be task-queued");

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["a".to_string()]);
  Ok(())
}

#[test]
fn dynamic_external_script_nested_in_container_executes_as_script_task() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;
  h.set_external_script_sources(HashMap::from([(
    "/a.js".to_string(),
    "if (!document.currentScript || document.currentScript.getAttribute('id') !== 'dyn') throw new Error('bad currentScript'); console.log('a');".to_string(),
  )]));

  h.exec_script(
    r#"
      var container = document.createElement("div");

      var s = document.createElement("script");
      s.setAttribute("id", "dyn");
      s.setAttribute("src", "/a.js");
      container.appendChild(s);
      console.log("after-disconnected-append");
    "#,
  )?;

  assert_eq!(h.take_log(), vec!["after-disconnected-append".to_string()]);

  assert!(
    h.complete_external_script("/a.js").is_err(),
    "expected disconnected scripts to not start loading"
  );

  h.exec_script(
    r#"
      document.body.appendChild(container);
      console.log("after-append");
    "#,
  )?;

  assert_eq!(h.take_log(), vec!["after-append".to_string()]);

  h.complete_external_script("/a.js")?;
  assert!(h.take_log().is_empty(), "expected execution to be task-queued");

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["a".to_string()]);
  Ok(())
}

#[test]
fn dynamic_external_script_via_document_fragment_executes_as_script_task() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;
  h.set_external_script_sources(HashMap::from([(
    "/a.js".to_string(),
    "if (!document.currentScript || document.currentScript.getAttribute('id') !== 'dyn') throw new Error('bad currentScript'); console.log('a');".to_string(),
  )]));

  h.exec_script(
    r#"
      var frag = document.createDocumentFragment();

      var s = document.createElement("script");
      s.setAttribute("id", "dyn");
      s.setAttribute("src", "/a.js");
      frag.appendChild(s);
      console.log("after-fragment-append");
    "#,
  )?;

  assert_eq!(h.take_log(), vec!["after-fragment-append".to_string()]);

  assert!(
    h.complete_external_script("/a.js").is_err(),
    "expected detached fragment scripts to not start loading"
  );

  h.exec_script(
    r#"
      document.body.appendChild(frag);
      console.log("after-append");
    "#,
  )?;

  assert_eq!(h.take_log(), vec!["after-append".to_string()]);

  h.complete_external_script("/a.js")?;
  assert!(h.take_log().is_empty(), "expected execution to be task-queued");

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["a".to_string()]);
  Ok(())
}

#[test]
fn dynamic_external_script_in_inert_template_does_not_load_until_moved() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;
  h.set_external_script_sources(HashMap::from([(
    "/a.js".to_string(),
    "if (!document.currentScript || document.currentScript.getAttribute('id') !== 'dyn') throw new Error('bad currentScript'); console.log('a');".to_string(),
  )]));

  h.exec_script(
    r#"
      var tpl = document.createElement("template");
      document.body.appendChild(tpl);

      var s = document.createElement("script");
      s.setAttribute("id", "dyn");
      s.setAttribute("src", "/a.js");
      tpl.appendChild(s);
      console.log("after-template-append");
    "#,
  )?;

  assert_eq!(h.take_log(), vec!["after-template-append".to_string()]);

  assert!(
    h.complete_external_script("/a.js").is_err(),
    "expected scripts in inert <template> contents to not start loading"
  );

  h.exec_script(
    r#"
      // Move out of the inert template: now it becomes connected for scripting.
      document.body.appendChild(s);
      console.log("after-move");
    "#,
  )?;

  assert_eq!(h.take_log(), vec!["after-move".to_string()]);

  h.complete_external_script("/a.js")?;
  assert!(h.take_log().is_empty(), "expected execution to be task-queued");

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["a".to_string()]);
  Ok(())
}

#[test]
fn dynamic_inline_script_executes_and_flushes_microtasks() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      var s = document.createElement("script");
      s.setAttribute("id", "dyn");
      s.appendChild(document.createTextNode(
        "if (!document.currentScript || document.currentScript.getAttribute('id') !== 'dyn') throw new Error('bad currentScript'); console.log('INLINE'); queueMicrotask(() => console.log('microtask'));"
      ));
      document.body.appendChild(s);
      console.log("after-append");
    "#,
  )?;

  // Inline scripts are queued as Script tasks; DOM insertion itself should not execute them
  // synchronously.
  assert_eq!(h.take_log(), vec!["after-append".to_string()]);

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["INLINE".to_string(), "microtask".to_string()]);
  Ok(())
}

#[test]
fn dynamic_script_already_started_prevents_reexecution() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      var a = document.createElement("div");
      var b = document.createElement("div");
      document.body.appendChild(a);
      document.body.appendChild(b);

      var s = document.createElement("script");
      s.appendChild(document.createTextNode("console.log('ONCE');"));

      a.appendChild(s);
      // Move the same script node again: it must not execute twice.
      b.appendChild(s);
    "#,
  )?;

  assert!(h.take_log().is_empty(), "expected script task to be queued");

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["ONCE".to_string()]);
  Ok(())
}
