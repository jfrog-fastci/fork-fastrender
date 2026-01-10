use crate::dom2::NodeId;
use crate::error::{Error, Result};
use crate::js::{ScriptElementSpec, ScriptType};

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
pub struct DiscoveredScript {
  pub id: HtmlScriptId,
  pub actions: Vec<HtmlScriptSchedulerAction>,
}

/// Action stream produced by [`HtmlScriptScheduler`].
///
/// This is intentionally "spec-shaped": it models the host obligations described by WHATWG HTML
/// ("start a fetch", "queue a task", "execute now", "queue an element task", ...), but leaves the
/// embedding responsible for performing the actual work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlScriptSchedulerAction {
  /// Begin fetching an external script.
  StartFetch {
    script_id: HtmlScriptId,
    node_id: NodeId,
    url: String,
  },
  /// Block parsing until the referenced script has executed.
  ///
  /// Only classic, parser-inserted, parser-blocking scripts should emit this.
  BlockParserUntilExecuted { script_id: HtmlScriptId, node_id: NodeId },
  /// Execute a script immediately (synchronously in the caller's stack).
  ///
  /// `source_text=None` represents "result is null" in the HTML script processing model (e.g. a
  /// network error).
  ExecuteNow {
    script_id: HtmlScriptId,
    node_id: NodeId,
    script_type: ScriptType,
    external_file: bool,
    source_text: Option<String>,
  },
  /// Queue script execution as an event-loop task.
  ///
  /// `source_text=None` represents "result is null" in the HTML script processing model (e.g. a
  /// network error or module graph construction failure).
  QueueTask {
    script_id: HtmlScriptId,
    node_id: NodeId,
    script_type: ScriptType,
    external_file: bool,
    source_text: Option<String>,
  },
  /// Queue an "element task" to dispatch a `load` or `error` event at a `<script>` element.
  QueueScriptEventTask { node_id: NodeId, event: ScriptEventKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalMode {
  Blocking,
  Defer,
  Async,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchState {
  Pending,
  Ready,
}

#[derive(Debug, Clone)]
struct ExternalScriptEntry {
  node_id: NodeId,
  script_type: ScriptType,
  external_file: bool,
  mode: ExternalMode,
  fetch_state: FetchState,
  /// `Some` when the script source is available, `None` when the script's "result is null" (fetch or
  /// module graph failure).
  source_text: Option<String>,
  queued_for_execution: bool,
}

/// Minimal HTML `<script>` scheduling state machine that is capable of modeling:
/// - classic scripts (parser-blocking vs async vs defer), and
/// - module scripts (async vs deferred-by-default),
/// - plus import map edge cases required by the HTML Standard (external import maps are not
///   supported and must fire `error`).
///
/// This intentionally does *not* implement a module graph; it treats module scripts as "fetch a
/// single source string, then execute".
#[derive(Debug, Default)]
pub struct HtmlScriptScheduler {
  next_script_id: u64,
  scripts: HashMap<HtmlScriptId, ExternalScriptEntry>,
  defer_queue: Vec<HtmlScriptId>,
  next_defer_to_queue: usize,
  parsing_completed: bool,
}

impl HtmlScriptScheduler {
  pub fn new() -> Self {
    Self::default()
  }

  fn alloc_script_id(&mut self) -> HtmlScriptId {
    let id = HtmlScriptId(self.next_script_id);
    self.next_script_id = self.next_script_id.saturating_add(1);
    id
  }

  pub fn discovered_parser_script(
    &mut self,
    element: ScriptElementSpec,
    node_id: NodeId,
  ) -> Result<DiscoveredScript> {
    // Force the parser-inserted flag on this entry point.
    let mut element = element;
    element.parser_inserted = true;
    self.discovered_script(element, node_id)
  }

  pub fn discovered_script(
    &mut self,
    element: ScriptElementSpec,
    node_id: NodeId,
  ) -> Result<DiscoveredScript> {
    let id = self.alloc_script_id();

    let mut actions: Vec<HtmlScriptSchedulerAction> = Vec::new();

    match element.script_type {
      ScriptType::ImportMap => {
        // HTML: External import maps are currently not supported; they must *not* be fetched and
        // must queue an element task to fire `error`.
        if element.src_attr_present {
          actions.push(HtmlScriptSchedulerAction::QueueScriptEventTask {
            node_id,
            event: ScriptEventKind::Error,
          });
        } else {
          // Inline import maps are out of scope for this scheduler for now: they will be registered
          // once the module pipeline is implemented.
        }
      }
      ScriptType::Classic | ScriptType::Module => {
        if element.src_attr_present {
          // External script (classic/module).
          //
          // Note: presence of `src` suppresses inline execution even if the value is empty/invalid.
          let url = element.src.clone().filter(|s| !s.is_empty());
          let mode = external_mode(element.script_type, element.parser_inserted, element.async_attr, element.defer_attr);

          if let Some(url) = url {
            if mode == ExternalMode::Defer {
              self.defer_queue.push(id);
            }

            self.scripts.insert(
              id,
              ExternalScriptEntry {
                node_id,
                script_type: element.script_type,
                external_file: true,
                mode,
                fetch_state: FetchState::Pending,
                source_text: None,
                queued_for_execution: false,
              },
            );

            actions.push(HtmlScriptSchedulerAction::StartFetch {
              script_id: id,
              node_id,
              url,
            });

            if mode == ExternalMode::Blocking && element.script_type == ScriptType::Classic && element.parser_inserted {
              actions.push(HtmlScriptSchedulerAction::BlockParserUntilExecuted { script_id: id, node_id });
            }
          } else {
            // `src` attribute present but not fetchable (empty/invalid/unresolvable). HTML treats
            // this as a failure (result is null) and does not fall back to inline execution.
            //
            // Model it as an async task so the embedding can mark the script as started and then
            // dispatch an `error` event.
            actions.push(HtmlScriptSchedulerAction::QueueTask {
              script_id: id,
              node_id,
              script_type: element.script_type,
              external_file: true,
              source_text: None,
            });
          }
        } else {
          // Inline script.
          match element.script_type {
            ScriptType::Classic => {
              actions.push(HtmlScriptSchedulerAction::ExecuteNow {
                script_id: id,
                node_id,
                script_type: ScriptType::Classic,
                external_file: false,
                source_text: Some(element.inline_text),
              });
            }
            ScriptType::Module => {
              // Module scripts are deferred-by-default when parser-inserted and not async.
              let mode =
                external_mode(ScriptType::Module, element.parser_inserted, element.async_attr, element.defer_attr);
              if mode == ExternalMode::Defer {
                self.defer_queue.push(id);
              }
              self.scripts.insert(
                id,
                ExternalScriptEntry {
                  node_id,
                  script_type: ScriptType::Module,
                  external_file: false,
                  mode,
                  fetch_state: FetchState::Ready,
                  source_text: Some(element.inline_text),
                  queued_for_execution: false,
                },
              );

              if mode == ExternalMode::Async {
                actions.extend(self.queue_execution_if_ready(id)?);
              } else if self.parsing_completed {
                actions.extend(self.queue_defer_scripts_if_ready()?);
              }
            }
            _ => {}
          }
        }
      }
      ScriptType::Unknown => {}
    }

    Ok(DiscoveredScript { id, actions })
  }

  pub fn fetch_completed(
    &mut self,
    script_id: HtmlScriptId,
    source_text: String,
  ) -> Result<Vec<HtmlScriptSchedulerAction>> {
    let Some(entry) = self.scripts.get_mut(&script_id) else {
      return Err(Error::Other(format!(
        "fetch_completed called for unknown script_id={}",
        script_id.as_u64()
      )));
    };

    if entry.fetch_state != FetchState::Pending {
      return Err(Error::Other(format!(
        "fetch_completed called more than once for script_id={}",
        script_id.as_u64()
      )));
    }

    entry.fetch_state = FetchState::Ready;
    entry.source_text = Some(source_text);

    match entry.mode {
      ExternalMode::Blocking | ExternalMode::Async => self.queue_execution_if_ready(script_id),
      ExternalMode::Defer => self.queue_defer_scripts_if_ready(),
    }
  }

  pub fn fetch_failed(&mut self, script_id: HtmlScriptId) -> Result<Vec<HtmlScriptSchedulerAction>> {
    let mode = {
      let Some(entry) = self.scripts.get_mut(&script_id) else {
        return Err(Error::Other(format!(
          "fetch_failed called for unknown script_id={}",
          script_id.as_u64()
        )));
      };

      if entry.fetch_state != FetchState::Pending {
        return Err(Error::Other(format!(
          "fetch_failed called more than once for script_id={}",
          script_id.as_u64()
        )));
      }

      entry.fetch_state = FetchState::Ready;
      // `source_text` stays None => "result is null".
      entry.mode
    };

    match mode {
      ExternalMode::Blocking | ExternalMode::Async => self.queue_execution_if_ready(script_id),
      ExternalMode::Defer => self.queue_defer_scripts_if_ready(),
    }
  }

  pub fn parsing_completed(&mut self) -> Result<Vec<HtmlScriptSchedulerAction>> {
    self.parsing_completed = true;
    self.queue_defer_scripts_if_ready()
  }

  fn queue_execution_if_ready(
    &mut self,
    script_id: HtmlScriptId,
  ) -> Result<Vec<HtmlScriptSchedulerAction>> {
    let Some(entry) = self.scripts.get_mut(&script_id) else {
      return Err(Error::Other(format!(
        "internal error: queue_execution_if_ready missing script_id={}",
        script_id.as_u64()
      )));
    };

    if entry.queued_for_execution {
      return Ok(Vec::new());
    }
    if entry.fetch_state != FetchState::Ready {
      return Ok(Vec::new());
    }
    entry.queued_for_execution = true;

    let action = match entry.mode {
      ExternalMode::Blocking => HtmlScriptSchedulerAction::ExecuteNow {
        script_id,
        node_id: entry.node_id,
        script_type: entry.script_type,
        external_file: entry.external_file,
        source_text: entry.source_text.clone(),
      },
      ExternalMode::Async | ExternalMode::Defer => HtmlScriptSchedulerAction::QueueTask {
        script_id,
        node_id: entry.node_id,
        script_type: entry.script_type,
        external_file: entry.external_file,
        source_text: entry.source_text.clone(),
      },
    };

    Ok(vec![action])
  }

  fn queue_defer_scripts_if_ready(&mut self) -> Result<Vec<HtmlScriptSchedulerAction>> {
    if !self.parsing_completed {
      return Ok(Vec::new());
    }

    let mut actions: Vec<HtmlScriptSchedulerAction> = Vec::new();
    while self.next_defer_to_queue < self.defer_queue.len() {
      let script_id = self.defer_queue[self.next_defer_to_queue];
      let Some(entry) = self.scripts.get_mut(&script_id) else {
        return Err(Error::Other(format!(
          "internal error: defer_queue references missing script_id={}",
          script_id.as_u64()
        )));
      };

      if entry.queued_for_execution {
        self.next_defer_to_queue += 1;
        continue;
      }

      if entry.fetch_state != FetchState::Ready {
        break;
      }

      entry.queued_for_execution = true;
      actions.push(HtmlScriptSchedulerAction::QueueTask {
        script_id,
        node_id: entry.node_id,
        script_type: entry.script_type,
        external_file: entry.external_file,
        source_text: entry.source_text.clone(),
      });
      self.next_defer_to_queue += 1;
    }

    Ok(actions)
  }
}

fn external_mode(
  script_type: ScriptType,
  parser_inserted: bool,
  async_attr: bool,
  defer_attr: bool,
) -> ExternalMode {
  match script_type {
    ScriptType::Classic => {
      if !parser_inserted || async_attr {
        ExternalMode::Async
      } else if defer_attr {
        ExternalMode::Defer
      } else {
        ExternalMode::Blocking
      }
    }
    ScriptType::Module => {
      // Module scripts are deferred by default when parser-inserted and not async.
      if !parser_inserted || async_attr {
        ExternalMode::Async
      } else {
        ExternalMode::Defer
      }
    }
    _ => ExternalMode::Async,
  }
}

