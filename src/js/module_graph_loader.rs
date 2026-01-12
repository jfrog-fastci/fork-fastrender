use crate::error::{Error, Result};

use super::JsExecutionOptions;

use std::collections::{HashMap, HashSet, VecDeque};

/// Raw module fetch result used by [`load_module_graph`].
///
/// The host/embedding is responsible for resolving import specifiers to fetchable module IDs/URLs
/// and for extracting the list of static imports (e.g. by parsing the module source).
#[derive(Debug, Clone)]
pub struct FetchedModule {
  /// Raw UTF-8 bytes for the module's source text.
  ///
  /// Budgets are applied based on these bytes **before** converting to `String`.
  pub bytes: Vec<u8>,
  /// Static module specifiers requested by this module.
  pub requested_modules: Vec<String>,
}

/// Minimal module fetch interface used by [`load_module_graph`].
pub trait ModuleFetcher {
  fn fetch(&mut self, specifier: &str) -> Result<FetchedModule>;
}

#[derive(Debug, Clone)]
pub struct LoadedModule {
  pub specifier: String,
  pub source: String,
  pub requested_modules: Vec<String>,
  pub depth: usize,
  pub size_bytes: usize,
}

/// A fully loaded static module dependency graph.
#[derive(Debug, Clone)]
pub struct LoadedModuleGraph {
  pub entry_specifier: String,
  pub modules: HashMap<String, LoadedModule>,
  pub total_bytes: usize,
}

/// Load a static module dependency graph rooted at `entry_specifier`, enforcing the module graph
/// budgeting limits in [`JsExecutionOptions`].
///
/// This function is intentionally host-shaped and evaluator-independent: it does not attempt to
/// resolve specifiers relative to referrers or interpret import maps. Callers should pass in
/// already-resolved specifiers/URLs via [`ModuleFetcher`].
pub fn load_module_graph(
  fetcher: &mut dyn ModuleFetcher,
  entry_specifier: &str,
  options: JsExecutionOptions,
) -> Result<LoadedModuleGraph> {
  // Validate the entry specifier before allocating an owned copy.
  options.check_module_specifier(entry_specifier)?;
  options.check_module_graph_depth(0, entry_specifier)?;
  options.check_module_graph_modules(1, entry_specifier)?;

  let entry_specifier = entry_specifier.to_string();
  let mut scheduled: HashSet<String> = HashSet::new();
  scheduled.insert(entry_specifier.clone());

  let mut queue: VecDeque<(String, usize)> = VecDeque::new();
  queue.push_back((entry_specifier.clone(), 0));

  let mut modules: HashMap<String, LoadedModule> = HashMap::new();
  let mut total_bytes: usize = 0;

  while let Some((specifier, depth)) = queue.pop_front() {
    if modules.contains_key(&specifier) {
      continue;
    }

    options.check_module_specifier(&specifier)?;
    options.check_module_graph_depth(depth, &specifier)?;

    // Enforce module count *before* fetching so hostile graphs cannot force an unbounded number of
    // fetches/allocations.
    let next_modules = modules
      .len()
      .checked_add(1)
      .ok_or_else(|| Error::Other("Module graph module count overflowed usize".to_string()))?;
    options.check_module_graph_modules(next_modules, &specifier)?;

    let fetched = fetcher.fetch(&specifier)?;
    let module_bytes = fetched.bytes.len();

    // Per-module size cap (reuses the existing script size budget). This must happen before
    // converting bytes to a `String`.
    options.check_script_source_bytes(
      module_bytes,
      &format!("source=module specifier={specifier}"),
    )?;

    // Graph total bytes cap: must be checked before allocating the module source `String`.
    total_bytes = options.check_module_graph_total_bytes(total_bytes, module_bytes, &specifier)?;

    let source = String::from_utf8(fetched.bytes).map_err(|_| {
      Error::Other(format!(
        "Module source was not valid UTF-8 (specifier={specifier})"
      ))
    })?;

    // Validate/enqueue requested modules.
    for requested in &fetched.requested_modules {
      options.check_module_specifier(requested)?;
      let child_depth = depth
        .checked_add(1)
        .ok_or_else(|| Error::Other("Module graph depth overflowed usize".to_string()))?;
      options.check_module_graph_depth(child_depth, requested)?;

      if scheduled.contains(requested) {
        continue;
      }

      let next_scheduled = scheduled
        .len()
        .checked_add(1)
        .ok_or_else(|| Error::Other("Module graph module count overflowed usize".to_string()))?;
      // Enforce module count budget at discovery time so we do not keep queuing new modules after
      // exceeding the cap.
      options.check_module_graph_modules(next_scheduled, requested)?;

      scheduled.insert(requested.clone());
      queue.push_back((requested.clone(), child_depth));
    }

    modules.insert(
      specifier.clone(),
      LoadedModule {
        specifier,
        source,
        requested_modules: fetched.requested_modules,
        depth,
        size_bytes: module_bytes,
      },
    );
  }

  Ok(LoadedModuleGraph {
    entry_specifier,
    modules,
    total_bytes,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::error::Error;
  use std::collections::HashMap;

  #[derive(Default)]
  struct MapFetcher {
    modules: HashMap<String, FetchedModule>,
  }

  impl MapFetcher {
    fn with_module(mut self, specifier: &str, bytes: &[u8], requested: &[&str]) -> Self {
      self.modules.insert(
        specifier.to_string(),
        FetchedModule {
          bytes: bytes.to_vec(),
          requested_modules: requested.iter().map(|s| (*s).to_string()).collect(),
        },
      );
      self
    }
  }

  impl ModuleFetcher for MapFetcher {
    fn fetch(&mut self, specifier: &str) -> Result<FetchedModule> {
      self
        .modules
        .get(specifier)
        .cloned()
        .ok_or_else(|| Error::Other(format!("no module registered for specifier={specifier}")))
    }
  }

  #[test]
  fn exceeding_module_count_triggers_error() {
    let mut fetcher = MapFetcher::default()
      .with_module("entry", b"e", &["a", "b"])
      .with_module("a", b"a", &[])
      .with_module("b", b"b", &[]);

    let options = JsExecutionOptions {
      max_module_graph_modules: 2, // entry + one dependency
      ..JsExecutionOptions::default()
    };

    let err = load_module_graph(&mut fetcher, "entry", options)
      .expect_err("expected module graph load to fail");
    let Error::Other(msg) = err else {
      panic!("expected Error::Other, got {err:?}");
    };
    assert!(msg.contains("max_module_graph_modules"), "msg={msg}");
  }

  #[test]
  fn exceeding_total_bytes_triggers_error() {
    let mut fetcher = MapFetcher::default()
      .with_module("entry", b"1234", &["a"])
      .with_module("a", b"1234", &[]);

    let options = JsExecutionOptions {
      max_module_graph_total_bytes: 7, // entry=4, a=4 => 8 exceeds
      ..JsExecutionOptions::default()
    };

    let err = load_module_graph(&mut fetcher, "entry", options)
      .expect_err("expected module graph load to fail");
    let Error::Other(msg) = err else {
      panic!("expected Error::Other, got {err:?}");
    };
    assert!(msg.contains("max_module_graph_total_bytes"), "msg={msg}");
  }

  #[test]
  fn exceeding_recursion_depth_triggers_error() {
    let mut fetcher = MapFetcher::default()
      .with_module("entry", b"e", &["a"])
      .with_module("a", b"a", &["b"])
      .with_module("b", b"b", &[]);

    let options = JsExecutionOptions {
      max_module_graph_depth: 1, // entry depth=0, a depth=1 allowed, b depth=2 rejected
      ..JsExecutionOptions::default()
    };

    let err = load_module_graph(&mut fetcher, "entry", options)
      .expect_err("expected module graph load to fail");
    let Error::Other(msg) = err else {
      panic!("expected Error::Other, got {err:?}");
    };
    assert!(msg.contains("max_module_graph_depth"), "msg={msg}");
  }
}
