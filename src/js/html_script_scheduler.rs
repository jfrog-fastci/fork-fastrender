//! Spec-shaped HTML `<script>` scheduling state machine.
//!
//! This scheduler is designed to be deterministic and unit-testable. It does not execute scripts or
//! fetch network resources directly. Instead, callers drive it with explicit discovery/completion
//! events and perform the returned [`HtmlScriptSchedulerAction`] values.
//!
//! Compared to [`crate::js::script_scheduler::ScriptScheduler`], this scheduler models:
//! - classic scripts (parser-blocking vs `async` vs `defer`)
//! - module scripts (`type="module"`, inline + external)
//! - import maps (`type="importmap"`, inline-only)
//! - `nomodule` (when modules are supported)
//! - spec-correct ordering for dynamically inserted external scripts without `async`
//!
//! The scheduling model follows the WHATWG HTML Standard's conceptual lists/sets:
//! - "set of scripts that will execute as soon as possible" (async)
//! - "list of scripts that will execute in order as soon as possible" (ordered-asap)
//! - "list of scripts that will execute when the document has finished parsing" (post-parse)

use crate::error::{Error, Result};
use crate::resource::FetchDestination;

use super::{ScriptElementSpec, ScriptType};

use std::collections::HashMap;

/// Opaque ID for a `<script>` element managed by [`HtmlScriptScheduler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HtmlScriptId(u64);

impl HtmlScriptId {
  pub fn as_u64(self) -> u64 {
    self.0
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptEventKind {
  Load,
  Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlScriptWork {
  Classic { source_text: Option<String> },
  Module { source_text: Option<String> },
  ImportMap { source_text: String, base_url: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlDiscoveredScript<NodeId> {
  pub id: HtmlScriptId,
  pub actions: Vec<HtmlScriptSchedulerAction<NodeId>>,
}

/// Action stream produced by [`HtmlScriptScheduler`].
///
/// This is intentionally spec-shaped: it models the host obligations described by WHATWG HTML
/// ("start a fetch", "queue a task", "execute now", ...), but leaves the embedding responsible for
/// performing the actual work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlScriptSchedulerAction<NodeId> {
  /// Begin fetching an external classic script.
  StartClassicFetch {
    script_id: HtmlScriptId,
    node_id: NodeId,
    url: String,
    destination: FetchDestination,
  },
  /// Begin fetching the module graph for an external module script (`type="module" src=...`).
  ///
  /// The full [`ScriptElementSpec`] is included so the orchestrator can extract module fetch options
  /// (credentials mode, referrer policy, etc.) as the spec surface evolves.
  StartModuleGraphFetch {
    script_id: HtmlScriptId,
    node_id: NodeId,
    url: String,
    destination: FetchDestination,
    element: ScriptElementSpec,
  },
  /// Begin fetching the module graph for an inline module script (`type="module"` with no `src`).
  StartInlineModuleGraphFetch {
    script_id: HtmlScriptId,
    node_id: NodeId,
    source_text: String,
    base_url: Option<String>,
    element: ScriptElementSpec,
  },
  /// Block parsing until the referenced script has executed.
  ///
  /// This is emitted for parser-blocking external classic scripts (no `async`/`defer`).
  BlockParserUntilExecuted { script_id: HtmlScriptId, node_id: NodeId },
  /// Execute a script immediately (synchronously in the caller's stack).
  ///
  /// The orchestrator must perform a microtask checkpoint immediately after execution.
  ExecuteNow {
    script_id: HtmlScriptId,
    node_id: NodeId,
    work: HtmlScriptWork,
  },
  /// Queue script execution as an event-loop task.
  ///
  /// The event loop performs a microtask checkpoint after each task, satisfying the HTML microtask
  /// checkpoint requirement after script execution.
  QueueTask {
    script_id: HtmlScriptId,
    node_id: NodeId,
    work: HtmlScriptWork,
  },
  /// Queue an "element task" to dispatch a `load` or `error` event at a `<script>` element.
  QueueScriptEventTask {
    script_id: HtmlScriptId,
    node_id: NodeId,
    event: ScriptEventKind,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptKind {
  Classic,
  Module,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScheduleMode {
  /// Parser-blocking external classic scripts (no `async`/`defer`).
  ParserBlocking,
  /// "as soon as possible" unordered set (async scripts).
  Async,
  /// "in order as soon as possible" list (non-parser-inserted scripts without `async`).
  OrderedAsap,
  /// "when parsing is complete" list (classic defer, parser-inserted modules).
  PostParse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadyPayload {
  ClassicSource(Option<String>),
  ModuleSource(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScriptEntry<NodeId> {
  node_id: NodeId,
  #[allow(dead_code)]
  base_url_at_discovery: Option<String>,
  kind: ScriptKind,
  mode: ScheduleMode,
  ready: Option<ReadyPayload>,
  queued_for_execution: bool,
}

pub struct HtmlScriptScheduler<NodeId> {
  next_script_id: u64,
  modules_supported: bool,

  scripts: HashMap<HtmlScriptId, ScriptEntry<NodeId>>,

  // "list of scripts that will execute in order as soon as possible"
  ordered_asap: Vec<HtmlScriptId>,
  next_ordered_asap_to_queue: usize,

  // "list of scripts that will execute when the document has finished parsing"
  post_parse: Vec<HtmlScriptId>,
  next_post_parse_to_queue: usize,
  parsing_completed: bool,
}

impl<NodeId> Default for HtmlScriptScheduler<NodeId> {
  fn default() -> Self {
    Self {
      next_script_id: 1,
      modules_supported: true,
      scripts: HashMap::new(),
      ordered_asap: Vec::new(),
      next_ordered_asap_to_queue: 0,
      post_parse: Vec::new(),
      next_post_parse_to_queue: 0,
      parsing_completed: false,
    }
  }
}

impl<NodeId: Clone> HtmlScriptScheduler<NodeId> {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_modules_supported(modules_supported: bool) -> Self {
    Self {
      modules_supported,
      ..Self::default()
    }
  }

  fn alloc_script_id(&mut self) -> HtmlScriptId {
    let id = HtmlScriptId(self.next_script_id);
    self.next_script_id += 1;
    id
  }

  /// Notify the scheduler that the HTML parser has discovered a parser-inserted `<script>`.
  pub fn discovered_parser_script(
    &mut self,
    element: ScriptElementSpec,
    node_id: NodeId,
    base_url_at_discovery: Option<String>,
  ) -> Result<HtmlDiscoveredScript<NodeId>> {
    let mut element = element;
    element.parser_inserted = true;
    self.discovered_script(element, node_id, base_url_at_discovery)
  }

  /// Notify the scheduler that a `<script>` element has been discovered.
  pub fn discovered_script(
    &mut self,
    element: ScriptElementSpec,
    node_id: NodeId,
    base_url_at_discovery: Option<String>,
  ) -> Result<HtmlDiscoveredScript<NodeId>> {
    let id = self.alloc_script_id();
    let mut actions: Vec<HtmlScriptSchedulerAction<NodeId>> = Vec::new();

    // `nomodule` only applies to classic scripts when modules are supported.
    if element.script_type == ScriptType::Classic && element.nomodule_attr && self.modules_supported {
      return Ok(HtmlDiscoveredScript { id, actions });
    }

    match element.script_type {
      ScriptType::Unknown => {}
      ScriptType::ImportMap => {
        // Import maps must be inline-only.
        if element.src_attr_present {
          actions.push(HtmlScriptSchedulerAction::QueueScriptEventTask {
            script_id: id,
            node_id,
            event: ScriptEventKind::Error,
          });
          return Ok(HtmlDiscoveredScript { id, actions });
        }

        let base_url = base_url_at_discovery.clone().or_else(|| element.base_url.clone());
        actions.push(HtmlScriptSchedulerAction::ExecuteNow {
          script_id: id,
          node_id,
          work: HtmlScriptWork::ImportMap {
            source_text: element.inline_text,
            base_url,
          },
        });
        return Ok(HtmlDiscoveredScript { id, actions });
      }
      ScriptType::Classic => {
        if !element.src_attr_present {
          actions.push(HtmlScriptSchedulerAction::ExecuteNow {
            script_id: id,
            node_id,
            work: HtmlScriptWork::Classic {
              source_text: Some(element.inline_text),
            },
          });
          return Ok(HtmlDiscoveredScript { id, actions });
        }

        let Some(url) = element.src.clone().filter(|s| !s.is_empty()) else {
          // `src` attribute present but empty/invalid/unresolvable: HTML fires an error event and does
          // not fall back to inline execution.
          actions.push(HtmlScriptSchedulerAction::QueueScriptEventTask {
            script_id: id,
            node_id,
            event: ScriptEventKind::Error,
          });
          return Ok(HtmlDiscoveredScript { id, actions });
        };

        let is_async = element.async_attr || element.force_async;
        let mode = if element.parser_inserted && !is_async && !element.defer_attr {
          ScheduleMode::ParserBlocking
        } else if is_async {
          ScheduleMode::Async
        } else if element.parser_inserted {
          // `defer` scripts, in document order.
          ScheduleMode::PostParse
        } else {
          // Dynamically inserted classic scripts without async: execute in insertion order as soon as
          // possible (spec "ordered as soon as possible" list).
          ScheduleMode::OrderedAsap
        };

        if mode == ScheduleMode::OrderedAsap {
          self.ordered_asap.push(id);
        }
        if mode == ScheduleMode::PostParse {
          self.post_parse.push(id);
        }

        self.scripts.insert(
          id,
          ScriptEntry {
            node_id: node_id.clone(),
            base_url_at_discovery,
            kind: ScriptKind::Classic,
            mode,
            ready: None,
            queued_for_execution: false,
          },
        );

        let destination = if element.crossorigin.is_some() {
          FetchDestination::ScriptCors
        } else {
          FetchDestination::Script
        };
        actions.push(HtmlScriptSchedulerAction::StartClassicFetch {
          script_id: id,
          node_id: node_id.clone(),
          url,
          destination,
        });

        if mode == ScheduleMode::ParserBlocking {
          actions.push(HtmlScriptSchedulerAction::BlockParserUntilExecuted {
            script_id: id,
            node_id,
          });
        }

        return Ok(HtmlDiscoveredScript { id, actions });
      }
      ScriptType::Module => {
        // HTML: `defer` has no effect on module scripts.
        let mode = if element.async_attr {
          ScheduleMode::Async
        } else if element.parser_inserted {
          // Parser-inserted module scripts are deferred by default.
          ScheduleMode::PostParse
        } else {
          // Dynamically inserted module scripts without async execute in insertion order.
          ScheduleMode::OrderedAsap
        };

        if element.src_attr_present {
          let Some(url) = element.src.clone().filter(|s| !s.is_empty()) else {
            actions.push(HtmlScriptSchedulerAction::QueueScriptEventTask {
              script_id: id,
              node_id,
              event: ScriptEventKind::Error,
            });
            return Ok(HtmlDiscoveredScript { id, actions });
          };

          if mode == ScheduleMode::OrderedAsap {
            self.ordered_asap.push(id);
          }
          if mode == ScheduleMode::PostParse {
            self.post_parse.push(id);
          }

          self.scripts.insert(
            id,
            ScriptEntry {
              node_id: node_id.clone(),
              base_url_at_discovery,
              kind: ScriptKind::Module,
              mode,
              ready: None,
              queued_for_execution: false,
            },
          );

          // Module scripts are fetched in CORS mode regardless of the `crossorigin` attribute.
          actions.push(HtmlScriptSchedulerAction::StartModuleGraphFetch {
            script_id: id,
            node_id: node_id.clone(),
            url,
            destination: FetchDestination::ScriptCors,
            element: element.clone(),
          });
          return Ok(HtmlDiscoveredScript { id, actions });
        }

        // Inline module script.
        let base_url = base_url_at_discovery.clone().or_else(|| element.base_url.clone());

        if mode == ScheduleMode::OrderedAsap {
          self.ordered_asap.push(id);
        }
        if mode == ScheduleMode::PostParse {
          self.post_parse.push(id);
        }

        self.scripts.insert(
          id,
          ScriptEntry {
            node_id: node_id.clone(),
            base_url_at_discovery,
            kind: ScriptKind::Module,
            mode,
            ready: None,
            queued_for_execution: false,
          },
        );

        actions.push(HtmlScriptSchedulerAction::StartInlineModuleGraphFetch {
          script_id: id,
          node_id,
          source_text: element.inline_text.clone(),
          base_url,
          element,
        });
        return Ok(HtmlDiscoveredScript { id, actions });
      }
    }

    Ok(HtmlDiscoveredScript { id, actions })
  }

  /// Notify the scheduler that a previously requested classic external script fetch completed.
  pub fn classic_fetch_completed(
    &mut self,
    script_id: HtmlScriptId,
    source_text: String,
  ) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    self.classic_fetch_finished(script_id, Some(source_text))
  }

  /// Notify the scheduler that a previously requested classic external script fetch failed.
  pub fn classic_fetch_failed(&mut self, script_id: HtmlScriptId) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    self.classic_fetch_finished(script_id, None)
  }

  fn classic_fetch_finished(
    &mut self,
    script_id: HtmlScriptId,
    source_text: Option<String>,
  ) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    let Some(entry) = self.scripts.get_mut(&script_id) else {
      return Err(Error::Other(format!(
        "classic_fetch_completed called for unknown script_id={}",
        script_id.as_u64()
      )));
    };
    if entry.kind != ScriptKind::Classic {
      return Err(Error::Other(format!(
        "classic_fetch_completed called for non-classic script_id={}",
        script_id.as_u64()
      )));
    }
    if entry.ready.is_some() {
      return Err(Error::Other(format!(
        "classic_fetch_completed called more than once for script_id={}",
        script_id.as_u64()
      )));
    }

    entry.ready = Some(ReadyPayload::ClassicSource(source_text));

    match entry.mode {
      ScheduleMode::ParserBlocking => self.queue_blocking_if_ready(script_id),
      ScheduleMode::Async => self.queue_async_if_ready(script_id),
      ScheduleMode::OrderedAsap => self.queue_ordered_asap_if_ready(),
      ScheduleMode::PostParse => self.queue_post_parse_if_ready(),
    }
  }

  /// Notify the scheduler that a previously requested module graph fetch completed.
  pub fn module_graph_completed(
    &mut self,
    script_id: HtmlScriptId,
    module_source_text: String,
  ) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    self.module_graph_finished(script_id, Some(module_source_text))
  }

  /// Notify the scheduler that a previously requested module graph fetch failed.
  pub fn module_graph_failed(&mut self, script_id: HtmlScriptId) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    self.module_graph_finished(script_id, None)
  }

  fn module_graph_finished(
    &mut self,
    script_id: HtmlScriptId,
    module_source_text: Option<String>,
  ) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    let Some(entry) = self.scripts.get_mut(&script_id) else {
      return Err(Error::Other(format!(
        "module_graph_completed called for unknown script_id={}",
        script_id.as_u64()
      )));
    };
    if entry.kind != ScriptKind::Module {
      return Err(Error::Other(format!(
        "module_graph_completed called for non-module script_id={}",
        script_id.as_u64()
      )));
    }
    if entry.ready.is_some() {
      return Err(Error::Other(format!(
        "module_graph_completed called more than once for script_id={}",
        script_id.as_u64()
      )));
    }

    entry.ready = Some(ReadyPayload::ModuleSource(module_source_text));

    match entry.mode {
      ScheduleMode::ParserBlocking => {
        // Module scripts never block the parser.
        debug_assert!(false, "module scripts should never be parser-blocking");
        self.queue_blocking_if_ready(script_id)
      }
      ScheduleMode::Async => self.queue_async_if_ready(script_id),
      ScheduleMode::OrderedAsap => self.queue_ordered_asap_if_ready(),
      ScheduleMode::PostParse => self.queue_post_parse_if_ready(),
    }
  }

  /// Notify the scheduler that HTML parsing has completed.
  pub fn parsing_completed(&mut self) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    self.parsing_completed = true;
    self.queue_post_parse_if_ready()
  }

  fn take_ready_work(entry: &mut ScriptEntry<NodeId>) -> Result<HtmlScriptWork> {
    let payload = entry.ready.take().ok_or_else(|| {
      Error::Other("internal error: script queued without ready payload".to_string())
    })?;

    Ok(match payload {
      ReadyPayload::ClassicSource(source_text) => HtmlScriptWork::Classic { source_text },
      ReadyPayload::ModuleSource(source_text) => HtmlScriptWork::Module { source_text },
    })
  }

  fn queue_blocking_if_ready(
    &mut self,
    script_id: HtmlScriptId,
  ) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    let Some(entry) = self.scripts.get_mut(&script_id) else {
      return Err(Error::Other(format!(
        "internal error: missing script entry for script_id={}",
        script_id.as_u64()
      )));
    };

    if entry.queued_for_execution {
      return Ok(Vec::new());
    }

    if entry.ready.is_none() {
      return Ok(Vec::new());
    }

    entry.queued_for_execution = true;
    let node_id = entry.node_id.clone();
    let work = Self::take_ready_work(entry)?;
    Ok(vec![HtmlScriptSchedulerAction::ExecuteNow {
      script_id,
      node_id,
      work,
    }])
  }

  fn queue_async_if_ready(
    &mut self,
    script_id: HtmlScriptId,
  ) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    let Some(entry) = self.scripts.get_mut(&script_id) else {
      return Err(Error::Other(format!(
        "internal error: missing script entry for script_id={}",
        script_id.as_u64()
      )));
    };

    if entry.queued_for_execution {
      return Ok(Vec::new());
    }

    if entry.ready.is_none() {
      return Ok(Vec::new());
    }

    entry.queued_for_execution = true;
    let node_id = entry.node_id.clone();
    let work = Self::take_ready_work(entry)?;
    Ok(vec![HtmlScriptSchedulerAction::QueueTask {
      script_id,
      node_id,
      work,
    }])
  }

  fn queue_ordered_asap_if_ready(&mut self) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    let mut actions: Vec<HtmlScriptSchedulerAction<NodeId>> = Vec::new();
    while self.next_ordered_asap_to_queue < self.ordered_asap.len() {
      let script_id = self.ordered_asap[self.next_ordered_asap_to_queue];
      let Some(entry) = self.scripts.get_mut(&script_id) else {
        return Err(Error::Other(format!(
          "internal error: ordered_asap references missing script_id={}",
          script_id.as_u64()
        )));
      };

      if entry.queued_for_execution {
        self.next_ordered_asap_to_queue += 1;
        continue;
      }

      if entry.ready.is_none() {
        break;
      }

      entry.queued_for_execution = true;
      let node_id = entry.node_id.clone();
      let work = Self::take_ready_work(entry)?;
      actions.push(HtmlScriptSchedulerAction::QueueTask {
        script_id,
        node_id,
        work,
      });
      self.next_ordered_asap_to_queue += 1;
    }
    Ok(actions)
  }

  fn queue_post_parse_if_ready(&mut self) -> Result<Vec<HtmlScriptSchedulerAction<NodeId>>> {
    if !self.parsing_completed {
      return Ok(Vec::new());
    }

    let mut actions: Vec<HtmlScriptSchedulerAction<NodeId>> = Vec::new();
    while self.next_post_parse_to_queue < self.post_parse.len() {
      let script_id = self.post_parse[self.next_post_parse_to_queue];
      let Some(entry) = self.scripts.get_mut(&script_id) else {
        return Err(Error::Other(format!(
          "internal error: post_parse references missing script_id={}",
          script_id.as_u64()
        )));
      };

      if entry.queued_for_execution {
        self.next_post_parse_to_queue += 1;
        continue;
      }

      if entry.ready.is_none() {
        break;
      }

      entry.queued_for_execution = true;
      let node_id = entry.node_id.clone();
      let work = Self::take_ready_work(entry)?;
      actions.push(HtmlScriptSchedulerAction::QueueTask {
        script_id,
        node_id,
        work,
      });
      self.next_post_parse_to_queue += 1;
    }
    Ok(actions)
  }
}

#[cfg(test)]
mod state_machine_tests {
  use super::*;
  use crate::js::{EventLoop, RunLimits, TaskSource};

  #[derive(Default)]
  struct Host {
    log: Vec<String>,
  }

  fn execute_fake_work(host: &mut Host, event_loop: &mut EventLoop<Host>, work: &HtmlScriptWork) -> Result<()> {
    let label = match work {
      HtmlScriptWork::Classic { source_text } => {
        let body = source_text.as_deref().unwrap_or("<null>");
        format!("classic:{body}")
      }
      HtmlScriptWork::Module { source_text } => {
        let body = source_text.as_deref().unwrap_or("<null>");
        format!("module:{body}")
      }
      HtmlScriptWork::ImportMap { source_text, .. } => format!("importmap:{source_text}"),
    };

    host.log.push(label.clone());
    let micro = format!("microtask:{label}");
    event_loop.queue_microtask(move |host, _| {
      host.log.push(micro);
      Ok(())
    })?;
    Ok(())
  }

  fn classic_external(src: &str, async_attr: bool, defer_attr: bool, parser_inserted: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(src.to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr,
      defer_attr,
      nomodule_attr: false,
      crossorigin: None,
      integrity: None,
      referrer_policy: None,
      parser_inserted,
      force_async: false,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  fn classic_inline(source: &str, nomodule_attr: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      src_attr_present: false,
      inline_text: source.to_string(),
      async_attr: false,
      defer_attr: false,
      nomodule_attr,
      crossorigin: None,
      integrity: None,
      referrer_policy: None,
      parser_inserted: true,
      force_async: false,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  fn module_external(src: &str, async_attr: bool, parser_inserted: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(src.to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity: None,
      referrer_policy: None,
      parser_inserted,
      force_async: false,
      node_id: None,
      script_type: ScriptType::Module,
    }
  }

  fn module_inline(source: &str, async_attr: bool, parser_inserted: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: Some("https://example.com/".to_string()),
      src: None,
      src_attr_present: false,
      inline_text: source.to_string(),
      async_attr,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity: None,
      referrer_policy: None,
      parser_inserted,
      force_async: false,
      node_id: None,
      script_type: ScriptType::Module,
    }
  }

  fn import_map_inline(source: &str) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: Some("https://example.com/".to_string()),
      src: None,
      src_attr_present: false,
      inline_text: source.to_string(),
      async_attr: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity: None,
      referrer_policy: None,
      parser_inserted: true,
      force_async: false,
      node_id: None,
      script_type: ScriptType::ImportMap,
    }
  }

  struct Harness {
    scheduler: HtmlScriptScheduler<u32>,
    event_loop: EventLoop<Host>,
    host: Host,

    started_classic_fetches: Vec<(HtmlScriptId, u32, String, FetchDestination)>,
    started_module_fetches: Vec<(HtmlScriptId, u32, String, FetchDestination, usize)>,
    started_inline_module_fetches: Vec<(HtmlScriptId, u32, String, usize)>,

    import_map_version: usize,
  }

  impl Harness {
    fn new() -> Self {
      Self::new_with_modules_supported(true)
    }

    fn new_with_modules_supported(modules_supported: bool) -> Self {
      Self {
        scheduler: HtmlScriptScheduler::with_modules_supported(modules_supported),
        event_loop: EventLoop::new(),
        host: Host::default(),
        started_classic_fetches: Vec::new(),
        started_module_fetches: Vec::new(),
        started_inline_module_fetches: Vec::new(),
        import_map_version: 0,
      }
    }

    fn apply_actions(&mut self, actions: Vec<HtmlScriptSchedulerAction<u32>>) -> Result<()> {
      for action in actions {
        match action {
          HtmlScriptSchedulerAction::StartClassicFetch {
            script_id,
            node_id,
            url,
            destination,
          } => {
            self
              .started_classic_fetches
              .push((script_id, node_id, url, destination));
          }
          HtmlScriptSchedulerAction::StartModuleGraphFetch {
            script_id,
            node_id,
            url,
            destination,
            element: _,
          } => {
            self
              .started_module_fetches
              .push((script_id, node_id, url, destination, self.import_map_version));
          }
          HtmlScriptSchedulerAction::StartInlineModuleGraphFetch {
            script_id,
            node_id,
            source_text,
            base_url: _,
            element: _,
          } => {
            self
              .started_inline_module_fetches
              .push((script_id, node_id, source_text, self.import_map_version));
          }
          HtmlScriptSchedulerAction::BlockParserUntilExecuted { .. } => {
            // These state-machine tests do not model parser pausing; legacy `ScriptScheduler` tests
            // cover that contract. Here we focus on ordering decisions.
          }
          HtmlScriptSchedulerAction::ExecuteNow { work, .. } => {
            if matches!(&work, HtmlScriptWork::ImportMap { .. }) {
              self.import_map_version += 1;
            }
            execute_fake_work(&mut self.host, &mut self.event_loop, &work)?;
            self
              .event_loop
              .perform_microtask_checkpoint(&mut self.host)?;
          }
          HtmlScriptSchedulerAction::QueueTask { work, .. } => {
            if matches!(&work, HtmlScriptWork::ImportMap { .. }) {
              self.import_map_version += 1;
            }
            self
              .event_loop
              .queue_task(TaskSource::Script, move |host, event_loop| {
                execute_fake_work(host, event_loop, &work)
              })?;
          }
          HtmlScriptSchedulerAction::QueueScriptEventTask { event, .. } => {
            let name = match event {
              ScriptEventKind::Load => "load",
              ScriptEventKind::Error => "error",
            };
            self
              .event_loop
              .queue_task(TaskSource::DOMManipulation, move |host, _| {
                host.log.push(format!("event:{name}"));
                Ok(())
              })?;
          }
        }
      }
      Ok(())
    }

    fn discover(&mut self, element: ScriptElementSpec) -> Result<HtmlScriptId> {
      let discovered = self
        .scheduler
        .discovered_script(element, /* node_id */ 1, /* base_url_at_discovery */ None)?;
      let id = discovered.id;
      self.apply_actions(discovered.actions)?;
      Ok(id)
    }

    fn classic_fetch_complete(&mut self, script_id: HtmlScriptId, source_text: &str) -> Result<()> {
      let actions = self
        .scheduler
        .classic_fetch_completed(script_id, source_text.to_string())?;
      self.apply_actions(actions)
    }

    fn module_graph_complete(&mut self, script_id: HtmlScriptId, source_text: &str) -> Result<()> {
      let actions = self
        .scheduler
        .module_graph_completed(script_id, source_text.to_string())?;
      self.apply_actions(actions)
    }

    fn parsing_completed(&mut self) -> Result<()> {
      let actions = self.scheduler.parsing_completed()?;
      self.apply_actions(actions)
    }

    fn run_event_loop(&mut self) -> Result<()> {
      self
        .event_loop
        .run_until_idle(&mut self.host, RunLimits::unbounded())?;
      Ok(())
    }
  }

  #[test]
  fn parser_inserted_deferred_module_scripts_execute_after_parsing_completed_in_order() -> Result<()> {
    let mut h = Harness::new();

    let m1 = h.discover(module_external("m1.js", /* async */ false, /* parser_inserted */ true))?;
    let m2 = h.discover(module_external("m2.js", /* async */ false, /* parser_inserted */ true))?;

    // Complete out-of-order before parsing completes.
    h.module_graph_complete(m2, "2")?;
    h.parsing_completed()?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      Vec::<String>::new(),
      "deferred module scripts must not run until prior scripts are ready"
    );

    h.module_graph_complete(m1, "1")?;
    h.run_event_loop()?;

    assert_eq!(
      h.host.log,
      vec![
        "module:1".to_string(),
        "microtask:module:1".to_string(),
        "module:2".to_string(),
        "microtask:module:2".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn async_module_script_queues_task_on_graph_ready_without_waiting_for_parsing_completed() -> Result<()> {
    let mut h = Harness::new();

    // A parser-inserted, non-async module script is deferred by default.
    let deferred = h.discover(module_external(
      "deferred.js",
      /* async */ false,
      /* parser_inserted */ true,
    ))?;

    // An async module script should run ASAP once its graph is ready, even before parsing completes.
    let async_id = h.discover(module_external("async.js", /* async */ true, /* parser_inserted */ true))?;
    h.module_graph_complete(async_id, "9")?;
    h.run_event_loop()?;

    assert_eq!(
      h.host.log,
      vec!["module:9".to_string(), "microtask:module:9".to_string()]
    );

    // The deferred module script still must not run before parsing_completed.
    h.module_graph_complete(deferred, "1")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["module:9".to_string(), "microtask:module:9".to_string()]
    );

    h.parsing_completed()?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec![
        "module:9".to_string(),
        "microtask:module:9".to_string(),
        "module:1".to_string(),
        "microtask:module:1".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn dynamic_module_scripts_without_async_execute_in_insertion_order_asap() -> Result<()> {
    let mut h = Harness::new();

    let m1 = h.discover(module_external(
      "dyn1.js",
      /* async */ false,
      /* parser_inserted */ false,
    ))?;
    let m2 = h.discover(module_external(
      "dyn2.js",
      /* async */ false,
      /* parser_inserted */ false,
    ))?;

    // Complete out of order: m2 ready before m1.
    h.module_graph_complete(m2, "2")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      Vec::<String>::new(),
      "ordered-asap scripts must not run ahead of earlier scripts"
    );

    h.module_graph_complete(m1, "1")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec![
        "module:1".to_string(),
        "microtask:module:1".to_string(),
        "module:2".to_string(),
        "microtask:module:2".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn dynamic_classic_external_without_async_executes_in_insertion_order_asap() -> Result<()> {
    let mut h = Harness::new();

    let c1 = h.discover(classic_external(
      "c1.js",
      /* async */ false,
      /* defer */ false,
      /* parser_inserted */ false,
    ))?;
    let c2 = h.discover(classic_external(
      "c2.js",
      /* async */ false,
      /* defer */ false,
      /* parser_inserted */ false,
    ))?;

    // Complete out of order: c2 finishes first.
    h.classic_fetch_complete(c2, "C2")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      Vec::<String>::new(),
      "ordered-asap classic scripts must not run ahead of earlier scripts"
    );

    h.classic_fetch_complete(c1, "C1")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec![
        "classic:C1".to_string(),
        "microtask:classic:C1".to_string(),
        "classic:C2".to_string(),
        "microtask:classic:C2".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn import_map_executes_synchronously_at_discovery_and_is_visible_to_later_module_fetch() -> Result<()> {
    let mut h = Harness::new();

    // Import map must execute synchronously at discovery.
    let _map_id = h.discover(import_map_inline("{\"imports\":{}}"))?;
    assert_eq!(h.import_map_version, 1);
    assert_eq!(
      h.host.log,
      vec![
        "importmap:{\"imports\":{}}".to_string(),
        "microtask:importmap:{\"imports\":{}}".to_string(),
      ]
    );

    // A later module script should observe the updated import map state when starting its graph
    // fetch (represented here by our harness recording the version at StartInlineModuleGraphFetch).
    let _m = h.discover(module_inline("export {}", /* async */ false, /* parser_inserted */ true))?;
    assert_eq!(h.started_inline_module_fetches.len(), 1);
    assert_eq!(h.started_inline_module_fetches[0].3, 1);
    Ok(())
  }

  #[test]
  fn script_fetch_actions_use_script_destinations() -> Result<()> {
    let mut h = Harness::new();

    let _classic = h.discover(classic_external(
      "classic.js",
      /* async */ false,
      /* defer */ false,
      /* parser_inserted */ true,
    ))?;
    assert_eq!(h.started_classic_fetches.len(), 1);
    assert_eq!(h.started_classic_fetches[0].3, FetchDestination::Script);

    let mut classic_cors = classic_external(
      "classic_cors.js",
      /* async */ false,
      /* defer */ false,
      /* parser_inserted */ true,
    );
    classic_cors.crossorigin = Some(crate::resource::CorsMode::Anonymous);
    let _classic_cors_id = h.discover(classic_cors)?;
    assert_eq!(h.started_classic_fetches.len(), 2);
    assert_eq!(h.started_classic_fetches[1].3, FetchDestination::ScriptCors);

    let _module = h.discover(module_external(
      "mod.js",
      /* async */ false,
      /* parser_inserted */ true,
    ))?;
    assert_eq!(h.started_module_fetches.len(), 1);
    assert_eq!(h.started_module_fetches[0].3, FetchDestination::ScriptCors);
    Ok(())
  }

  #[test]
  fn nomodule_classic_script_is_ignored_when_modules_are_supported() -> Result<()> {
    let mut h = Harness::new_with_modules_supported(true);
    h.discover(classic_inline("A", /* nomodule */ true))?;
    assert!(h.host.log.is_empty());
    Ok(())
  }

  #[test]
  fn nomodule_classic_script_executes_when_modules_are_not_supported() -> Result<()> {
    let mut h = Harness::new_with_modules_supported(false);
    h.discover(classic_inline("A", /* nomodule */ true))?;
    assert_eq!(
      h.host.log,
      vec!["classic:A".to_string(), "microtask:classic:A".to_string()]
    );
    Ok(())
  }
}
