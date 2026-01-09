use crate::error::{RenderStage, Result};
use crate::js::{EventLoop, RunLimits, ScriptBlockingStyleSheetSet, TaskSource};
use crate::render_control::{record_stage, StageGuard, StageHeartbeat};
use memchr::memchr;
use std::cell::Cell;
use std::ops::ControlFlow;
use std::rc::Rc;

pub trait StylesheetLoader<Host> {
  fn start_external_stylesheet_load(
    &mut self,
    href: &str,
    key: usize,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()>;
}

pub trait ScriptExecutor {
  fn execute(&mut self, code: &str) -> Result<()>;
}

struct JsExecutionGuard {
  depth: Rc<Cell<usize>>,
}

impl Drop for JsExecutionGuard {
  fn drop(&mut self) {
    let cur = self.depth.get();
    debug_assert!(cur > 0, "js execution depth underflow");
    self.depth.set(cur.saturating_sub(1));
  }
}

const SCRIPT_BLOCKING_STYLESHEET_SPIN_LIMITS: RunLimits = RunLimits {
  max_tasks: 1024,
  max_microtasks: 4096,
  max_wall_time: None,
};

pub struct HtmlScriptingDriver<SL, SE>
where
  SL: StylesheetLoader<Self>,
  SE: ScriptExecutor,
{
  html: String,
  cursor: usize,
  next_stylesheet_key: usize,
  pending_parser_blocking_script: Option<String>,
  script_blocking_stylesheets: ScriptBlockingStyleSheetSet,
  stylesheet_loader: SL,
  script_executor: SE,
  parse_task_scheduled: bool,
  finished: bool,
  js_execution_depth: Rc<Cell<usize>>,
}

impl<SL, SE> HtmlScriptingDriver<SL, SE>
where
  SL: StylesheetLoader<Self>,
  SE: ScriptExecutor,
{
  pub fn new(html: String, stylesheet_loader: SL, script_executor: SE) -> Self {
    Self {
      html,
      cursor: 0,
      next_stylesheet_key: 0,
      pending_parser_blocking_script: None,
      script_blocking_stylesheets: ScriptBlockingStyleSheetSet::new(),
      stylesheet_loader,
      script_executor,
      parse_task_scheduled: false,
      finished: false,
      js_execution_depth: Rc::new(Cell::new(0)),
    }
  }

  pub fn start(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    self.queue_parse_task(event_loop)
  }

  fn enter_js_execution(&mut self) -> JsExecutionGuard {
    let cur = self.js_execution_depth.get();
    self.js_execution_depth.set(cur + 1);
    JsExecutionGuard {
      depth: Rc::clone(&self.js_execution_depth),
    }
  }

  fn queue_parse_task(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    if self.finished || self.parse_task_scheduled {
      return Ok(());
    }
    self.parse_task_scheduled = true;
    if let Err(err) = event_loop.queue_task(TaskSource::DOMManipulation, |host, event_loop| {
      let result = host.parse_until_blocked(event_loop);
      host.parse_task_scheduled = false;
      result
    }) {
      self.parse_task_scheduled = false;
      return Err(err);
    }
    Ok(())
  }

  fn parse_until_blocked(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    loop {
      if !self.try_execute_pending_parser_blocking_script(event_loop)? {
        return Ok(());
      }

      let bytes = self.html.as_bytes();
      if self.cursor >= bytes.len() {
        self.finished = true;
        return Ok(());
      }

      let Some(rel) = memchr(b'<', &bytes[self.cursor..]) else {
        self.cursor = bytes.len();
        continue;
      };
      let tag_start = self.cursor + rel;

      if bytes
        .get(tag_start + 1)
        .is_some_and(|b| *b == b'!' || *b == b'?')
      {
        let Some(tag_end) = find_tag_end(bytes, tag_start) else {
          self.cursor = bytes.len();
          self.finished = true;
          return Ok(());
        };
        self.cursor = tag_end;
        continue;
      }

      let Some(tag_end) = find_tag_end(bytes, tag_start) else {
        self.cursor = bytes.len();
        self.finished = true;
        return Ok(());
      };
      let Some((is_end, name_start, name_end)) = parse_tag_name_range(bytes, tag_start, tag_end)
      else {
        self.cursor = tag_start + 1;
        continue;
      };
      if is_end {
        self.cursor = tag_end;
        continue;
      }

      let tag_name = &bytes[name_start..name_end];
      if tag_name.eq_ignore_ascii_case(b"link") {
        let tag = &self.html[tag_start..tag_end];
        let mut rel_value: Option<&str> = None;
        let mut href_value: Option<&str> = None;

        for_each_attribute(tag, |name, value| {
          if name.eq_ignore_ascii_case("rel") {
            rel_value = value;
          } else if name.eq_ignore_ascii_case("href") {
            href_value = value;
          }
          ControlFlow::Continue(())
        });

        let is_stylesheet = rel_value.is_some_and(link_rel_is_stylesheet);
        if is_stylesheet {
          if let Some(href) = href_value.map(trim_ascii_whitespace).filter(|v| !v.is_empty()) {
            let key = self.next_stylesheet_key;
            self.next_stylesheet_key += 1;
            self
              .script_blocking_stylesheets
              .register_blocking_stylesheet(key);
            self
              .stylesheet_loader
              .start_external_stylesheet_load(href, key, event_loop)?;
          }
        }
        self.cursor = tag_end;
        continue;
      }

      if tag_name.eq_ignore_ascii_case(b"style") {
        let key = self.next_stylesheet_key;
        self.next_stylesheet_key += 1;
        self
          .script_blocking_stylesheets
          .register_blocking_stylesheet(key);
        self
          .script_blocking_stylesheets
          .unregister_blocking_stylesheet(key);

        let (_end_start, end_end) = find_raw_text_element_end_range(bytes, tag_end, b"style");
        self.cursor = end_end;
        continue;
      }

      if tag_name.eq_ignore_ascii_case(b"script") {
        let (async_attr, defer_attr) = script_async_defer_attrs(&self.html[tag_start..tag_end]);
        let (end_start, end_end) = find_raw_text_element_end_range(bytes, tag_end, b"script");
        let script_text = self.html[tag_end..end_start].to_string();
        self.cursor = end_end;

        // HTML: When a parser-inserted script end tag is seen, perform a microtask checkpoint
        // *before* preparing/executing the script, but only when the JS execution context stack is
        // empty. Parsing itself runs as an event-loop task, so this must not rely on
        // `EventLoop::currently_running_task()`.
        if self.js_execution_depth.get() == 0 {
          event_loop.perform_microtask_checkpoint(self)?;
        }

        if async_attr || defer_attr {
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            {
              let _guard = host.enter_js_execution();
              host.script_executor.execute(&script_text)?;
            }
            event_loop.perform_microtask_checkpoint(host)?;
            Ok(())
          })?;
          continue;
        }

        self.pending_parser_blocking_script = Some(script_text);
        if !self.try_execute_pending_parser_blocking_script(event_loop)? {
          return Ok(());
        }
        continue;
      }

      self.cursor = tag_end;
    }
  }

  fn try_execute_pending_parser_blocking_script(
    &mut self,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<bool> {
    let Some(script_text) = self.pending_parser_blocking_script.take() else {
      return Ok(true);
    };

    if !self.execute_parser_blocking_script(&script_text, event_loop)? {
      self.pending_parser_blocking_script = Some(script_text);
      return Ok(false);
    }
    Ok(true)
  }

  fn execute_parser_blocking_script(
    &mut self,
    script_text: &str,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<bool> {
    if self.script_blocking_stylesheets.has_blocking_stylesheet() {
      let _ = event_loop.spin_until(
        self,
        SCRIPT_BLOCKING_STYLESHEET_SPIN_LIMITS,
        |host| host.script_blocking_stylesheets.has_blocking_stylesheet(),
      )?;
    }

    if self.script_blocking_stylesheets.has_blocking_stylesheet() {
      return Ok(false);
    }

    {
      let _stage_guard = StageGuard::install(Some(RenderStage::Script));
      record_stage(StageHeartbeat::Script);
      {
        let _guard = self.enter_js_execution();
        self.script_executor.execute(script_text)?;
      }
    }
    // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
    // execution context stack is empty. Nested (re-entrant) script execution must not drain
    // microtasks until the outermost script returns.
    if self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(self)?;
    }
    Ok(true)
  }

  pub fn on_stylesheet_loaded(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    self.queue_parse_task(event_loop)
  }
}

fn is_ascii_whitespace_html(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html)
}

fn link_rel_is_stylesheet(rel: &str) -> bool {
  rel
    .split(is_ascii_whitespace_html)
    .filter(|token| !token.is_empty())
    .any(|token| token.eq_ignore_ascii_case("stylesheet"))
}

fn script_async_defer_attrs(tag: &str) -> (bool, bool) {
  let mut async_attr = false;
  let mut defer_attr = false;
  for_each_attribute(tag, |name, _value| {
    if name.eq_ignore_ascii_case("async") {
      async_attr = true;
    } else if name.eq_ignore_ascii_case("defer") {
      defer_attr = true;
    }
    ControlFlow::Continue(())
  });
  (async_attr, defer_attr)
}

fn is_tag_name_char(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'-' || b == b':'
}

fn find_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
  let mut quote: Option<u8> = None;
  let mut i = start + 1;
  while i < bytes.len() {
    let b = bytes[i];
    match quote {
      Some(q) => {
        if b == q {
          quote = None;
        }
      }
      None => {
        if b == b'"' || b == b'\'' {
          quote = Some(b);
        } else if b == b'>' {
          return Some(i + 1);
        }
      }
    }
    i += 1;
  }
  None
}

fn parse_tag_name_range(bytes: &[u8], start: usize, end: usize) -> Option<(bool, usize, usize)> {
  if bytes.get(start)? != &b'<' {
    return None;
  }
  let mut i = start + 1;
  while i < end && bytes[i].is_ascii_whitespace() {
    i += 1;
  }
  if i >= end {
    return None;
  }
  let mut is_end = false;
  if bytes[i] == b'/' {
    is_end = true;
    i += 1;
    while i < end && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
  }
  let name_start = i;
  while i < end && is_tag_name_char(bytes[i]) {
    i += 1;
  }
  if i == name_start {
    return None;
  }
  Some((is_end, name_start, i))
}

fn find_raw_text_element_end_range(bytes: &[u8], start: usize, tag: &'static [u8]) -> (usize, usize) {
  let mut idx = start;
  while let Some(rel) = memchr(b'<', &bytes[idx..]) {
    let pos = idx + rel;
    if bytes.get(pos + 1) == Some(&b'/') {
      let name_start = pos + 2;
      let name_end = name_start + tag.len();
      if name_end <= bytes.len()
        && bytes[name_start..name_end].eq_ignore_ascii_case(tag)
        && !bytes
          .get(name_end)
          .map(|b| is_tag_name_char(*b))
          .unwrap_or(false)
      {
        let end = find_tag_end(bytes, pos).unwrap_or(bytes.len());
        return (pos, end);
      }
    }
    idx = pos + 1;
  }
  (bytes.len(), bytes.len())
}

fn for_each_attribute<'a>(
  tag: &'a str,
  mut visit: impl FnMut(&'a str, Option<&'a str>) -> ControlFlow<()>,
) {
  const MAX_ATTRIBUTES_PER_TAG: usize = 128;

  let bytes = tag.as_bytes();
  let mut i = 0usize;
  let mut attrs_seen = 0usize;

  if bytes.get(i) == Some(&b'<') {
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    while i < bytes.len() && bytes[i] != b'>' && !bytes[i].is_ascii_whitespace() {
      i += 1;
    }
  }

  while i < bytes.len() {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'>' {
      break;
    }
    if bytes[i] == b'/' {
      i += 1;
      continue;
    }

    let name_start = i;
    while i < bytes.len()
      && !bytes[i].is_ascii_whitespace()
      && bytes[i] != b'='
      && bytes[i] != b'>'
    {
      i += 1;
    }
    let name_end = i;
    if name_end == name_start {
      i = i.saturating_add(1);
      continue;
    }
    let name = &tag[name_start..name_end];

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }

    let mut value: Option<&'a str> = None;
    if i < bytes.len() && bytes[i] == b'=' {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
        let quote = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = Some(&tag[start..i]);
        if i < bytes.len() {
          i += 1;
        }
      } else {
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
          i += 1;
        }
        value = Some(&tag[start..i]);
      }
    }

    attrs_seen += 1;
    if let ControlFlow::Break(()) = visit(name, value) {
      break;
    }
    if attrs_seen >= MAX_ATTRIBUTES_PER_TAG {
      break;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::EventLoop;

  #[derive(Default)]
  struct ManualStylesheetLoader {
    started: Vec<(String, usize)>,
  }

  impl<Host> StylesheetLoader<Host> for ManualStylesheetLoader {
    fn start_external_stylesheet_load(
      &mut self,
      href: &str,
      key: usize,
      _event_loop: &mut EventLoop<Host>,
    ) -> Result<()> {
      self.started.push((href.to_string(), key));
      Ok(())
    }
  }

  #[derive(Default)]
  struct LogExecutor {
    executed: Vec<String>,
  }

  impl ScriptExecutor for LogExecutor {
    fn execute(&mut self, code: &str) -> Result<()> {
      self.executed.push(code.to_string());
      Ok(())
    }
  }

  #[test]
  fn microtasks_run_before_parser_inserted_script_boundary_even_inside_parse_task() -> Result<()> {
    let html = "<!doctype html><script>RUN</script>";
    let mut host = HtmlScriptingDriver::new(
      html.to_string(),
      ManualStylesheetLoader::default(),
      LogExecutor::default(),
    );
    let mut event_loop = EventLoop::<HtmlScriptingDriver<ManualStylesheetLoader, LogExecutor>>::new();

    // Queue a microtask *before* parsing begins. This must run before the parser-inserted script
    // executes, even though parsing runs inside a DOMManipulation task.
    event_loop.queue_microtask(|host, _| {
      host
        .script_executor
        .executed
        .push("microtask".to_string());
      Ok(())
    })?;

    host.start(&mut event_loop)?;
    // Run the parse task first (without pre-draining microtasks) to ensure the pre-script checkpoint
    // at `</script>` boundaries is the mechanism that flushes the microtask.
    assert!(event_loop.run_next_task(&mut host)?);
    let _ = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.script_executor.executed,
      vec!["microtask".to_string(), "RUN".to_string()]
    );
    Ok(())
  }

  #[test]
  fn pre_script_microtask_checkpoint_is_skipped_when_js_execution_context_stack_nonempty() -> Result<()> {
    // Simulate re-entrant parsing (e.g. `document.write()` while a script is executing): the HTML
    // spec requires that the pre-script microtask checkpoint at `</script>` boundaries is skipped
    // when the JS execution context stack is not empty.
    let html = "<!doctype html><script>RUN</script>";
    let mut host = HtmlScriptingDriver::new(
      html.to_string(),
      ManualStylesheetLoader::default(),
      LogExecutor::default(),
    );
    let mut event_loop = EventLoop::<HtmlScriptingDriver<ManualStylesheetLoader, LogExecutor>>::new();

    event_loop.queue_microtask(|host, _| {
      host
        .script_executor
        .executed
        .push("microtask".to_string());
      Ok(())
    })?;

    {
      let _outer_js = host.enter_js_execution();
      host.parse_until_blocked(&mut event_loop)?;
      assert_eq!(host.script_executor.executed, vec!["RUN".to_string()]);
    }

    // Once the outer script returns, the JS execution context stack becomes empty and the pending
    // microtasks can run.
    event_loop.perform_microtask_checkpoint(&mut host)?;

    assert_eq!(
      host.script_executor.executed,
      vec!["RUN".to_string(), "microtask".to_string()]
    );
    Ok(())
  }

  #[test]
  fn parser_blocking_scripts_wait_for_stylesheet_load_completion() -> Result<()> {
    let html = r#"<!doctype html><link rel=stylesheet href="a.css"><script>RUN</script>"#;
    let mut host = HtmlScriptingDriver::new(
      html.to_string(),
      ManualStylesheetLoader::default(),
      LogExecutor::default(),
    );
    let mut event_loop = EventLoop::<HtmlScriptingDriver<ManualStylesheetLoader, LogExecutor>>::new();

    host.start(&mut event_loop)?;
    let _ = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.script_executor.executed, Vec::<String>::new());
    assert_eq!(host.stylesheet_loader.started.len(), 1);
    assert!(host.script_blocking_stylesheets.has_blocking_stylesheet());

    let stylesheet_key = host.stylesheet_loader.started[0].1;
    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      host
        .script_blocking_stylesheets
        .unregister_blocking_stylesheet(stylesheet_key);
      host.on_stylesheet_loaded(event_loop)?;
      Ok(())
    })?;

    let _ = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.script_executor.executed, vec!["RUN"]);
    Ok(())
  }
}
