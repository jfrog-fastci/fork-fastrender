use crate::js::import_maps::{
  resolve_module_specifier as resolve_import_map_specifier, ImportMapState,
};
use crate::js::options::JsExecutionOptions;
use crate::resource::{
  cors_enforcement_enabled, ensure_cors_allows_origin, ensure_http_success, ensure_script_mime_sane,
  is_data_url, origin_from_url, CorsMode, DocumentOrigin, FetchDestination, FetchRequest,
  ReferrerPolicy,
  ResourceFetcher,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use url::Url;
use vm_js::{
  Heap, ImportAttribute, ModuleGraph, ModuleId, ModuleLoadPayload, ModuleReferrer, ModuleRequest,
  ScriptId, SourceText, SourceTextModuleRecord, VmError,
};

const BARE_SPECIFIER_TYPE_ERROR: &str =
  "Module specifier must be a URL-like string (bare specifiers are not supported)";
const RELATIVE_WITHOUT_BASE_TYPE_ERROR: &str = "Cannot resolve module specifier without a base URL";
const UNKNOWN_REFERRER_TYPE_ERROR: &str =
  "Cannot resolve module specifier: unknown referrer module";
const MODULE_FETCH_FAILED_TYPE_ERROR: &str = "Failed to fetch module";
const MODULE_FETCH_INVALID_UTF8_TYPE_ERROR: &str = "Module response was not valid UTF-8";
const MODULE_FETCHER_MISSING_ERROR: &str = "Module loader missing ResourceFetcher";
const MODULE_TOO_LARGE_TYPE_ERROR: &str = "Module is too large";
const MODULE_SRI_INTEGRITY_TYPE_ERROR: &str = "SRI integrity check failed for module";
const MODULE_GRAPH_MODULE_COUNT_LIMIT_EXCEEDED_TYPE_ERROR: &str =
  "Module graph exceeded max_module_graph_modules";
const MODULE_GRAPH_TOTAL_BYTES_LIMIT_EXCEEDED_TYPE_ERROR: &str =
  "Module graph exceeded max_module_graph_total_bytes";
const MODULE_GRAPH_DEPTH_LIMIT_EXCEEDED_TYPE_ERROR: &str =
  "Module graph exceeded max_module_graph_depth";
const MODULE_SPECIFIER_TOO_LONG_TYPE_ERROR: &str =
  "Module specifier exceeded max_module_specifier_length";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleKey {
  pub url: String,
  pub attributes: Vec<ImportAttribute>,
}

impl ModuleKey {
  fn new(url: String, attributes: Vec<ImportAttribute>) -> Self {
    Self { url, attributes }
  }
}

#[derive(Debug)]
pub struct PendingContinuation {
  pub referrer: ModuleReferrer,
  pub request: ModuleRequest,
  pub payload: ModuleLoadPayload,
}

#[derive(Debug, Clone)]
pub enum ModuleLoadOutcome {
  /// The module load can be completed synchronously by calling `Vm::finish_loading_imported_module`
  /// with `result`.
  FinishNow(Result<ModuleId, VmError>),
  /// The module is already being fetched/parsed; this request was added to the waiters list.
  InFlight,
  /// The caller should start fetching the module source for the returned key.
  StartFetch(ModuleKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModuleResolveError {
  BareSpecifier,
  RelativeWithoutBase,
  UnknownReferrer,
  Url,
}

fn resolve_error_to_vm_error(err: ModuleResolveError) -> VmError {
  match err {
    ModuleResolveError::BareSpecifier => VmError::TypeError(BARE_SPECIFIER_TYPE_ERROR),
    ModuleResolveError::RelativeWithoutBase => VmError::TypeError(RELATIVE_WITHOUT_BASE_TYPE_ERROR),
    ModuleResolveError::UnknownReferrer => VmError::TypeError(UNKNOWN_REFERRER_TYPE_ERROR),
    ModuleResolveError::Url => VmError::TypeError(RELATIVE_WITHOUT_BASE_TYPE_ERROR),
  }
}

/// Host-side ECMAScript module loader state for a single realm.
///
/// This component is intentionally host-owned and spec-shaped:
/// - resolves module specifiers to absolute URLs,
/// - fetches module source text (via FastRender's [`ResourceFetcher`]),
/// - parses module records to discover static imports,
/// - memoizes loaded modules in a per-realm module map,
/// - and deduplicates concurrent in-flight loads.
///
/// For now this is a simplified loader:
/// - Bare specifiers resolve only via the document's import map (if registered); otherwise they are
///   rejected deterministically.
/// - Fetch uses `FetchDestination::ScriptCors` and enforces `max_script_bytes`.
/// - Import-map integrity metadata (SRI) is enforced when present.
pub struct ModuleLoader {
  /// Document URL/base URL used when resolving module specifiers from `ModuleReferrer::Realm`.
  document_url: Option<String>,
  /// Origin of the realm's initial document URL (stable for the lifetime of the realm).
  ///
  /// This is intentionally distinct from `document_url`, which tracks the current *base URL* and
  /// can change as `<base href>` elements are encountered.
  document_origin: Option<DocumentOrigin>,
  cors_mode: CorsMode,
  referrer_policy: ReferrerPolicy,
  /// Integrity metadata override for the next module-script *entry* fetch.
  ///
  /// HTML module scripts can specify Subresource Integrity metadata via the `<script>` element's
  /// `integrity` attribute. This must be applied to the entry module fetch (and must take
  /// precedence over import-map `"integrity"` metadata for the same URL).
  ///
  /// Note: This override is intentionally *not* applied to module dependencies, which continue to
  /// use import-map `"integrity"` metadata only.
  entry_module_integrity_override: Option<String>,
  fetcher: Option<Arc<dyn ResourceFetcher>>,
  import_map_state: ImportMapState,
  max_script_bytes: usize,
  max_module_graph_modules: usize,
  max_module_graph_total_bytes: usize,
  max_module_graph_depth: usize,
  max_module_specifier_length: usize,
  loaded_bytes_total: usize,
  module_depths: HashMap<ModuleId, usize>,

  /// Loaded module map: `(resolved_url, import_attributes)` -> `ModuleId`.
  pub module_map: HashMap<ModuleKey, ModuleId>,
  /// Reverse mapping used for `import.meta.url` and resolving child module specifiers.
  pub module_id_to_url: HashMap<ModuleId, String>,
  /// Script referrer base URLs used when resolving module specifiers for dynamic `import()` calls
  /// originating from classic scripts.
  script_id_to_url: HashMap<ScriptId, String>,
  /// In-flight dedup map: multiple requests for the same key share the same fetch/parse work.
  pub inflight: HashMap<ModuleKey, Vec<PendingContinuation>>,
}

impl Default for ModuleLoader {
  fn default() -> Self {
    Self::new(None)
  }
}

impl ModuleLoader {
  pub fn new(document_url: Option<String>) -> Self {
    let document_origin = document_url.as_deref().and_then(origin_from_url);
    let defaults = JsExecutionOptions::default();
    Self {
      document_url,
      document_origin,
      cors_mode: CorsMode::Anonymous,
      referrer_policy: ReferrerPolicy::default(),
      entry_module_integrity_override: None,
      fetcher: None,
      import_map_state: ImportMapState::new_empty(),
      max_script_bytes: defaults.max_script_bytes,
      max_module_graph_modules: defaults.max_module_graph_modules,
      max_module_graph_total_bytes: defaults.max_module_graph_total_bytes,
      max_module_graph_depth: defaults.max_module_graph_depth,
      max_module_specifier_length: defaults.max_module_specifier_length,
      loaded_bytes_total: 0,
      module_depths: HashMap::new(),
      module_map: HashMap::new(),
      module_id_to_url: HashMap::new(),
      script_id_to_url: HashMap::new(),
      inflight: HashMap::new(),
    }
  }

  pub fn set_fetcher(&mut self, fetcher: Arc<dyn ResourceFetcher>) {
    self.fetcher = Some(fetcher);
  }

  pub fn set_document_url(&mut self, document_url: Option<String>) {
    self.document_url = document_url;
  }

  /// Overrides the stable origin used for CORS enforcement when fetching modules.
  ///
  /// `document_url` is allowed to be opaque (e.g. `about:blank`); in those cases, the embedder may
  /// still know the correct origin (e.g. an inherited origin) and can provide it here.
  pub fn set_document_origin(&mut self, document_origin: Option<DocumentOrigin>) {
    self.document_origin = document_origin;
  }

  pub fn set_cors_mode(&mut self, mode: CorsMode) {
    self.cors_mode = mode;
  }

  pub fn set_referrer_policy(&mut self, policy: ReferrerPolicy) {
    self.referrer_policy = policy;
  }

  /// Override integrity metadata for the next module-script entry fetch.
  ///
  /// When set, this metadata is used instead of import-map `"integrity"` metadata for the entry
  /// module fetch performed by [`ModuleLoader::get_or_fetch_module`].
  pub fn set_entry_module_integrity_override(&mut self, integrity: Option<String>) {
    self.entry_module_integrity_override = integrity;
  }

  pub fn set_js_execution_options(&mut self, options: JsExecutionOptions) {
    self.max_script_bytes = options.max_script_bytes;
    self.max_module_graph_modules = options.max_module_graph_modules;
    self.max_module_graph_total_bytes = options.max_module_graph_total_bytes;
    self.max_module_graph_depth = options.max_module_graph_depth;
    self.max_module_specifier_length = options.max_module_specifier_length;
  }

  pub fn set_max_script_bytes(&mut self, max_script_bytes: usize) {
    self.max_script_bytes = max_script_bytes;
  }

  pub fn import_map_state_mut(&mut self) -> &mut ImportMapState {
    &mut self.import_map_state
  }

  pub fn module_url(&self, module: ModuleId) -> Option<&str> {
    self.module_id_to_url.get(&module).map(|s| s.as_str())
  }

  pub fn register_script_url(&mut self, script_id: ScriptId, url: String) -> Result<(), VmError> {
    self
      .script_id_to_url
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self.script_id_to_url.insert(script_id, url);
    Ok(())
  }

  pub fn unregister_script_url(&mut self, script_id: ScriptId) {
    self.script_id_to_url.remove(&script_id);
  }

  fn script_url(&self, script_id: ScriptId) -> Option<&str> {
    self.script_id_to_url.get(&script_id).map(|s| s.as_str())
  }

  fn register_module(
    &mut self,
    key: ModuleKey,
    module_id: ModuleId,
    depth: usize,
    loaded_bytes_total: usize,
    effective_url: String,
  ) -> Result<ModuleId, VmError> {
    self
      .module_map
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self
      .module_id_to_url
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self
      .module_depths
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;

    self.module_map.insert(key, module_id);
    self.module_id_to_url.insert(module_id, effective_url);
    self.module_depths.insert(module_id, depth);
    self.loaded_bytes_total = loaded_bytes_total;
    Ok(module_id)
  }

  /// Load a module by an already-resolved URL.
  ///
  /// This is used by HTML module-script entrypoints that already have a fully resolved URL and
  /// want to synchronously fetch + parse the root module before asking `vm-js` to load the rest of
  /// the dependency graph.
  pub fn get_or_fetch_module(
    &mut self,
    heap: &mut Heap,
    modules: &mut ModuleGraph,
    key: ModuleKey,
  ) -> Result<ModuleId, VmError> {
    if let Some(existing) = self.module_map.get(&key).copied() {
      return Ok(existing);
    }

    if self.max_module_specifier_length != usize::MAX
      && key.url.encode_utf16().count() > self.max_module_specifier_length
    {
      return Err(VmError::TypeError(MODULE_SPECIFIER_TOO_LONG_TYPE_ERROR));
    }

    let used_modules = self
      .module_map
      .len()
      .checked_add(self.inflight.len())
      .ok_or(VmError::OutOfMemory)?;
    let next_modules = used_modules.checked_add(1).ok_or(VmError::OutOfMemory)?;
    if next_modules > self.max_module_graph_modules {
      return Err(VmError::TypeError(
        MODULE_GRAPH_MODULE_COUNT_LIMIT_EXCEEDED_TYPE_ERROR,
      ));
    }

    if self.max_module_graph_total_bytes != usize::MAX
      && self.loaded_bytes_total >= self.max_module_graph_total_bytes
    {
      return Err(VmError::TypeError(
        MODULE_GRAPH_TOTAL_BYTES_LIMIT_EXCEEDED_TYPE_ERROR,
      ));
    }

    let remaining_total = self
      .max_module_graph_total_bytes
      .saturating_sub(self.loaded_bytes_total);
    let max_fetch = self.max_script_bytes.min(remaining_total);
    let max_fetch = max_fetch.saturating_add(1);

    let fetched = if is_data_url(&key.url) {
      // `data:` URL module scripts are fully self-contained and must not use the network fetcher.
      //
      // Decode only up to `max_fetch` so oversized inline payloads fail deterministically instead
      // of attempting to allocate unbounded memory.
      crate::resource::data_url::decode_data_url_prefix(&key.url, max_fetch)
        .map_err(|_| VmError::TypeError(MODULE_FETCH_FAILED_TYPE_ERROR))?
    } else {
      let Some(fetcher) = &self.fetcher else {
        return Err(VmError::Unimplemented(MODULE_FETCHER_MISSING_ERROR));
      };

      let mut req = FetchRequest::new(&key.url, FetchDestination::ScriptCors);
      if let Some(referrer_url) = self.document_url.as_deref() {
        req = req.with_referrer_url(referrer_url);
      }
      if let Some(origin) = self.document_origin.as_ref() {
        req = req.with_client_origin(origin);
      }
      req = req.with_referrer_policy(self.referrer_policy);
      req = req.with_credentials_mode(self.cors_mode.credentials_mode());

      if self.max_script_bytes == usize::MAX && remaining_total == usize::MAX {
        fetcher.fetch_with_request(req)
      } else {
        fetcher.fetch_partial_with_request(req, max_fetch)
      }
      .map_err(|_| VmError::TypeError(MODULE_FETCH_FAILED_TYPE_ERROR))?
    };

    ensure_http_success(&fetched, &key.url)
      .and_then(|_| ensure_script_mime_sane(&fetched, &key.url))
      .map_err(|_| VmError::TypeError(MODULE_FETCH_FAILED_TYPE_ERROR))?;

    if cors_enforcement_enabled() {
      ensure_cors_allows_origin(
        self.document_origin.as_ref(),
        &fetched,
        &key.url,
        self.cors_mode,
      )
      .map_err(|_| VmError::TypeError(MODULE_FETCH_FAILED_TYPE_ERROR))?;
    }

    if self.max_script_bytes != usize::MAX && fetched.bytes.len() > self.max_script_bytes {
      return Err(VmError::TypeError(MODULE_TOO_LARGE_TYPE_ERROR));
    }

    let module_bytes = fetched.bytes.len();
    let next_total = self
      .loaded_bytes_total
      .checked_add(module_bytes)
      .ok_or(VmError::OutOfMemory)?;
    if self.max_module_graph_total_bytes != usize::MAX
      && next_total > self.max_module_graph_total_bytes
    {
      return Err(VmError::TypeError(
        MODULE_GRAPH_TOTAL_BYTES_LIMIT_EXCEEDED_TYPE_ERROR,
      ));
    }

    if let Some(integrity) = self.entry_module_integrity_override.as_deref() {
      // If the `<script type="module">` element provides integrity metadata, enforce it for the
      // entry module fetch. (This is applied even for empty/invalid metadata, which must block
      // execution deterministically.)
      if crate::js::sri::verify_integrity(&fetched.bytes, integrity).is_err() {
        return Err(VmError::TypeError(MODULE_SRI_INTEGRITY_TYPE_ERROR));
      }
    } else {
      // Fall back to import-map integrity metadata ("integrity" top-level key).
      let integrity = Url::parse(&key.url)
        .ok()
        .map(|url| self.import_map_state.resolve_module_integrity_metadata(&url))
        .unwrap_or("");
      if !integrity.is_empty() {
        if crate::js::sri::verify_integrity(&fetched.bytes, integrity).is_err() {
          return Err(VmError::TypeError(MODULE_SRI_INTEGRITY_TYPE_ERROR));
        }
      }
    }

    let effective_url = fetched.final_url.clone().unwrap_or_else(|| key.url.clone());

    let source_text = String::from_utf8(fetched.bytes)
      .map_err(|_| VmError::TypeError(MODULE_FETCH_INVALID_UTF8_TYPE_ERROR))?;

    let source = SourceText::new_charged_arc(heap, effective_url.clone(), source_text)?;
    let record = SourceTextModuleRecord::parse_source(source)?;
    let module_id = modules.add_module(record)?;
    self.register_module(key, module_id, 0, next_total, effective_url)
  }

  /// Parse and register a module provided as inline text.
  ///
  /// Used for inline `<script type="module">` as well as for cases where the host has already
  /// fetched the module source text and wants to reuse it without performing another fetch.
  pub fn get_or_parse_inline_module(
    &mut self,
    heap: &mut Heap,
    modules: &mut ModuleGraph,
    key: ModuleKey,
    source_text: &str,
  ) -> Result<ModuleId, VmError> {
    if let Some(existing) = self.module_map.get(&key).copied() {
      return Ok(existing);
    }

    if self.max_module_specifier_length != usize::MAX
      && key.url.encode_utf16().count() > self.max_module_specifier_length
    {
      return Err(VmError::TypeError(MODULE_SPECIFIER_TOO_LONG_TYPE_ERROR));
    }

    let used_modules = self
      .module_map
      .len()
      .checked_add(self.inflight.len())
      .ok_or(VmError::OutOfMemory)?;
    let next_modules = used_modules.checked_add(1).ok_or(VmError::OutOfMemory)?;
    if next_modules > self.max_module_graph_modules {
      return Err(VmError::TypeError(
        MODULE_GRAPH_MODULE_COUNT_LIMIT_EXCEEDED_TYPE_ERROR,
      ));
    }

    if self.max_script_bytes != usize::MAX && source_text.len() > self.max_script_bytes {
      return Err(VmError::TypeError(MODULE_TOO_LARGE_TYPE_ERROR));
    }

    if self.max_module_graph_total_bytes != usize::MAX
      && self.loaded_bytes_total >= self.max_module_graph_total_bytes
    {
      return Err(VmError::TypeError(
        MODULE_GRAPH_TOTAL_BYTES_LIMIT_EXCEEDED_TYPE_ERROR,
      ));
    }
    let module_bytes = source_text.len();
    let next_total = self
      .loaded_bytes_total
      .checked_add(module_bytes)
      .ok_or(VmError::OutOfMemory)?;
    if self.max_module_graph_total_bytes != usize::MAX
      && next_total > self.max_module_graph_total_bytes
    {
      return Err(VmError::TypeError(
        MODULE_GRAPH_TOTAL_BYTES_LIMIT_EXCEEDED_TYPE_ERROR,
      ));
    }

    let source = SourceText::new_charged_arc(heap, key.url.clone(), source_text)?;
    let record = SourceTextModuleRecord::parse_source(source)?;
    let module_id = modules.add_module(record)?;
    let effective_url = key.url.clone();
    self.register_module(key, module_id, 0, next_total, effective_url)
  }

  fn resolve_request_url(
    &mut self,
    referrer: ModuleReferrer,
    request: &ModuleRequest,
  ) -> Result<String, ModuleResolveError> {
    let specifier = request.specifier_utf8_lossy();
    let base_url = match referrer {
      ModuleReferrer::Module(m) => Some(
        self
          .module_id_to_url
          .get(&m)
          .map(|s| s.as_str())
          .ok_or(ModuleResolveError::UnknownReferrer)?,
      ),
      ModuleReferrer::Realm(_) => self.document_url.as_deref(),
      ModuleReferrer::Script(script) => self.script_url(script).or(self.document_url.as_deref()),
    };

    let Some(base_url) = base_url else {
      // If there's no base URL for this realm/script, we can still resolve URL-like specifiers
      // (e.g. `data:` or `https:`). Relative URLs and bare specifiers require a base URL.
      return Url::parse(&specifier)
        .map(|url| url.to_string())
        .map_err(|_| ModuleResolveError::RelativeWithoutBase);
    };
    let base_url = Url::parse(base_url).map_err(|_| ModuleResolveError::Url)?;
    resolve_import_map_specifier(
      &mut self.import_map_state,
      &specifier,
      &base_url,
    )
    .map(|url| url.to_string())
    .map_err(|_| ModuleResolveError::BareSpecifier)
  }

  /// Resolve a module specifier the same way the loader would, without fetching or instantiating a
  /// module.
  ///
  /// This is used by `import.meta.resolve(specifier)` and must mirror the loader's resolution
  /// behavior (including import maps and specifier length limits).
  pub fn resolve_module_specifier_for_import_meta(
    &mut self,
    specifier: &str,
    base_url: Option<&str>,
  ) -> Result<String, VmError> {
    if self.max_module_specifier_length != usize::MAX
      && specifier.encode_utf16().count() > self.max_module_specifier_length
    {
      return Err(VmError::TypeError(MODULE_SPECIFIER_TOO_LONG_TYPE_ERROR));
    }

    let Some(base_url) = base_url else {
      // If there's no base URL, we can still resolve URL-like specifiers (e.g. `data:` or `https:`).
      // Relative URLs and bare specifiers require a base URL.
      return Url::parse(specifier)
        .map(|url| url.to_string())
        .map_err(|_| VmError::TypeError(RELATIVE_WITHOUT_BASE_TYPE_ERROR));
    };

    let base_url = Url::parse(base_url).map_err(|_| VmError::TypeError(RELATIVE_WITHOUT_BASE_TYPE_ERROR))?;

    resolve_import_map_specifier(&mut self.import_map_state, specifier, &base_url)
      .map(|url| url.to_string())
      .map_err(|_| VmError::TypeError(BARE_SPECIFIER_TYPE_ERROR))
  }

  /// Handle `HostLoadImportedModule` for a single requested module.
  ///
  /// The caller is responsible for:
  /// - starting the fetch when [`ModuleLoadOutcome::StartFetch`] is returned, and
  /// - eventually calling `Vm::finish_loading_imported_module` for *every* request stored in the
  ///   in-flight waiter list when the fetch completes (or fails).
  pub fn request_module(
    &mut self,
    referrer: ModuleReferrer,
    module_request: &ModuleRequest,
    payload: &ModuleLoadPayload,
  ) -> ModuleLoadOutcome {
    if self.max_module_specifier_length != usize::MAX
      && module_request.specifier.len_code_units() > self.max_module_specifier_length
    {
      return ModuleLoadOutcome::FinishNow(Err(VmError::TypeError(
        MODULE_SPECIFIER_TOO_LONG_TYPE_ERROR,
      )));
    }

    let resolved_url = match self.resolve_request_url(referrer, &module_request) {
      Ok(url) => url,
      Err(err) => {
        return ModuleLoadOutcome::FinishNow(Err(resolve_error_to_vm_error(err)));
      }
    };

    let key = ModuleKey::new(resolved_url, module_request.attributes.clone());

    if let Some(existing) = self.module_map.get(&key).copied() {
      return ModuleLoadOutcome::FinishNow(Ok(existing));
    }

    // Attach this continuation to an existing in-flight fetch if present.
    if let Some(waiters) = self.inflight.get_mut(&key) {
      if waiters.try_reserve(1).is_err() {
        return ModuleLoadOutcome::FinishNow(Err(VmError::OutOfMemory));
      }
      waiters.push(PendingContinuation {
        referrer,
        request: module_request.clone(),
        payload: payload.clone(),
      });
      return ModuleLoadOutcome::InFlight;
    }

    let depth = match referrer {
      ModuleReferrer::Module(m) => match self
        .module_depths
        .get(&m)
        .copied()
        .and_then(|d| d.checked_add(1))
      {
        Some(depth) => depth,
        None => {
          return ModuleLoadOutcome::FinishNow(Err(VmError::TypeError(UNKNOWN_REFERRER_TYPE_ERROR)))
        }
      },
      ModuleReferrer::Realm(_) | ModuleReferrer::Script(_) => 0,
    };

    if depth > self.max_module_graph_depth {
      return ModuleLoadOutcome::FinishNow(Err(VmError::TypeError(
        MODULE_GRAPH_DEPTH_LIMIT_EXCEEDED_TYPE_ERROR,
      )));
    }

    let used_modules = match self.module_map.len().checked_add(self.inflight.len()) {
      Some(v) => v,
      None => return ModuleLoadOutcome::FinishNow(Err(VmError::OutOfMemory)),
    };
    let next_modules = match used_modules.checked_add(1) {
      Some(v) => v,
      None => return ModuleLoadOutcome::FinishNow(Err(VmError::OutOfMemory)),
    };
    if next_modules > self.max_module_graph_modules {
      return ModuleLoadOutcome::FinishNow(Err(VmError::TypeError(
        MODULE_GRAPH_MODULE_COUNT_LIMIT_EXCEEDED_TYPE_ERROR,
      )));
    }

    // Start a new in-flight fetch.
    if self.inflight.try_reserve(1).is_err() {
      return ModuleLoadOutcome::FinishNow(Err(VmError::OutOfMemory));
    }

    let mut waiters = Vec::new();
    if waiters.try_reserve(1).is_err() {
      return ModuleLoadOutcome::FinishNow(Err(VmError::OutOfMemory));
    }
    waiters.push(PendingContinuation {
      referrer,
      request: module_request.clone(),
      payload: payload.clone(),
    });
    self.inflight.insert(key.clone(), waiters);
    ModuleLoadOutcome::StartFetch(key)
  }

  /// Removes and returns the list of waiters for `key`, if any.
  pub fn take_inflight(&mut self, key: &ModuleKey) -> Option<Vec<PendingContinuation>> {
    self.inflight.remove(key)
  }

  /// Fetch, decode, parse, and register a module for `key`.
  ///
  /// This removes the in-flight waiter list and returns it along with a completion record.
  ///
  /// The caller must later pass the returned `result` into `Vm::finish_loading_imported_module` for
  /// each waiter.
  ///
  /// Intended to run in a `TaskSource::Networking` task.
  pub fn fetch_and_register(
    &mut self,
    heap: &mut Heap,
    modules: &mut ModuleGraph,
    key: ModuleKey,
  ) -> Option<(Vec<PendingContinuation>, Result<ModuleId, VmError>)> {
    let Some(waiters) = self.inflight.remove(&key) else {
      return None;
    };

    let result = (|| -> Result<ModuleId, VmError> {
      let depth = match waiters.first().map(|w| w.referrer) {
        Some(ModuleReferrer::Module(m)) => self
          .module_depths
          .get(&m)
          .copied()
          .and_then(|d| d.checked_add(1))
          .ok_or(VmError::TypeError(UNKNOWN_REFERRER_TYPE_ERROR))?,
        Some(ModuleReferrer::Realm(_)) | Some(ModuleReferrer::Script(_)) | None => 0,
      };
      if depth > self.max_module_graph_depth {
        return Err(VmError::TypeError(
          MODULE_GRAPH_DEPTH_LIMIT_EXCEEDED_TYPE_ERROR,
        ));
      }

      let used_modules = self
        .module_map
        .len()
        .checked_add(self.inflight.len())
        .ok_or(VmError::OutOfMemory)?;
      let next_modules = used_modules.checked_add(1).ok_or(VmError::OutOfMemory)?;
      if next_modules > self.max_module_graph_modules {
        return Err(VmError::TypeError(
          MODULE_GRAPH_MODULE_COUNT_LIMIT_EXCEEDED_TYPE_ERROR,
        ));
      }

      if self.max_module_graph_total_bytes != usize::MAX
        && self.loaded_bytes_total >= self.max_module_graph_total_bytes
      {
        return Err(VmError::TypeError(
          MODULE_GRAPH_TOTAL_BYTES_LIMIT_EXCEEDED_TYPE_ERROR,
        ));
      }

      let referrer_url = waiters.first().and_then(|waiter| match waiter.referrer {
        ModuleReferrer::Module(m) => self.module_id_to_url.get(&m).cloned(),
        ModuleReferrer::Realm(_) => self.document_url.clone(),
        ModuleReferrer::Script(script) => self
          .script_id_to_url
          .get(&script)
          .cloned()
          .or_else(|| self.document_url.clone()),
      });

      let remaining_total = self
        .max_module_graph_total_bytes
        .saturating_sub(self.loaded_bytes_total);
      let max_fetch = self.max_script_bytes.min(remaining_total);
      let max_fetch = max_fetch.saturating_add(1);
      let fetched = if is_data_url(&key.url) {
        // Inline `data:` URL modules bypass the network fetcher. Decode with the same bounded
        // prefix strategy used for `fetch_partial_with_request` so large payloads fail
        // deterministically.
        crate::resource::data_url::decode_data_url_prefix(&key.url, max_fetch)
          .map_err(|_| VmError::TypeError(MODULE_FETCH_FAILED_TYPE_ERROR))?
      } else {
        let Some(fetcher) = &self.fetcher else {
          return Err(VmError::Unimplemented(MODULE_FETCHER_MISSING_ERROR));
        };

        let mut req = FetchRequest::new(&key.url, FetchDestination::ScriptCors);
        if let Some(referrer_url) = referrer_url.as_deref() {
          req = req.with_referrer_url(referrer_url);
        }
        if let Some(origin) = self.document_origin.as_ref() {
          req = req.with_client_origin(origin);
        }
        req = req.with_referrer_policy(self.referrer_policy);
        req = req.with_credentials_mode(self.cors_mode.credentials_mode());
        if self.max_script_bytes == usize::MAX && remaining_total == usize::MAX {
          fetcher.fetch_with_request(req)
        } else {
          fetcher.fetch_partial_with_request(req, max_fetch)
        }
        .map_err(|_| VmError::TypeError(MODULE_FETCH_FAILED_TYPE_ERROR))?
      };

      // Keep behavior aligned with classic script loading: avoid feeding obvious HTML error pages
      // into the module parser.
      ensure_http_success(&fetched, &key.url)
        .and_then(|_| ensure_script_mime_sane(&fetched, &key.url))
        .map_err(|_| VmError::TypeError(MODULE_FETCH_FAILED_TYPE_ERROR))?;

      if cors_enforcement_enabled() {
        ensure_cors_allows_origin(
          self.document_origin.as_ref(),
          &fetched,
          &key.url,
          self.cors_mode,
        )
        .map_err(|_| VmError::TypeError(MODULE_FETCH_FAILED_TYPE_ERROR))?;
      }

      if self.max_script_bytes != usize::MAX && fetched.bytes.len() > self.max_script_bytes {
        return Err(VmError::TypeError(MODULE_TOO_LARGE_TYPE_ERROR));
      }

      let module_bytes = fetched.bytes.len();
      let next_total = self
        .loaded_bytes_total
        .checked_add(module_bytes)
        .ok_or(VmError::OutOfMemory)?;
      if self.max_module_graph_total_bytes != usize::MAX
        && next_total > self.max_module_graph_total_bytes
      {
        return Err(VmError::TypeError(
          MODULE_GRAPH_TOTAL_BYTES_LIMIT_EXCEEDED_TYPE_ERROR,
        ));
      }

      // Enforce import-map integrity metadata ("integrity" top-level key). This is keyed by the
      // module's serialized URL.
      let integrity = Url::parse(&key.url)
        .ok()
        .map(|url| {
          self
            .import_map_state
            .resolve_module_integrity_metadata(&url)
        })
        .unwrap_or("");
      if !integrity.is_empty() {
        if crate::js::sri::verify_integrity(&fetched.bytes, integrity).is_err() {
          return Err(VmError::TypeError(MODULE_SRI_INTEGRITY_TYPE_ERROR));
        }
      }

      let effective_url = fetched.final_url.clone().unwrap_or_else(|| key.url.clone());

      let source_text = String::from_utf8(fetched.bytes)
        .map_err(|_| VmError::TypeError(MODULE_FETCH_INVALID_UTF8_TYPE_ERROR))?;

      let source = SourceText::new_charged_arc(heap, effective_url.clone(), source_text)?;
      let record = SourceTextModuleRecord::parse_source(source)?;

      let module_id = modules.add_module(record)?;
      self.register_module(key, module_id, depth, next_total, effective_url)
    })();

    Some((waiters, result))
  }
}

/// Shared handle to a realm's [`ModuleLoader`].
pub type ModuleLoaderHandle = Rc<RefCell<ModuleLoader>>;

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::FetchedResource;
  use vm_js::{Heap, HeapLimits, JsString, Realm, Scope, Vm, VmHostHooks, VmOptions};

  #[derive(Default)]
  struct MapFetcher {
    // url -> bytes
    map: HashMap<String, Vec<u8>>,
    fetch_count: std::sync::atomic::AtomicUsize,
  }

  impl MapFetcher {
    fn insert(&mut self, url: &str, body: &[u8]) {
      self.map.insert(url.to_string(), body.to_vec());
    }
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> crate::Result<FetchedResource> {
      self
        .fetch_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      let bytes = self
        .map
        .get(url)
        .cloned()
        .ok_or_else(|| crate::error::Error::Other(format!("no entry for url={url}")))?;
      Ok(FetchedResource {
        bytes,
        content_type: None,
        nosniff: false,
        content_encoding: None,
        status: None,
        etag: None,
        last_modified: None,
        access_control_allow_origin: None,
        timing_allow_origin: None,
        vary: None,
        response_referrer_policy: None,
        access_control_allow_credentials: false,
        final_url: None,
        cache_policy: None,
        response_headers: None,
      })
    }
  }

  /// Create a dummy `ModuleLoadPayload` for tests.
  ///
  /// `ModuleLoadPayload` is an opaque token with no public constructor; we obtain one by invoking
  /// `vm-js`'s module-graph loading entry point (`load_requested_modules`) and capturing the payload
  /// passed to [`vm_js::VmHostHooks::host_load_imported_module`].
  fn make_dummy_payload() -> ModuleLoadPayload {
    struct CaptureHost {
      captured: Option<(ModuleReferrer, ModuleRequest, ModuleLoadPayload)>,
    }

    impl VmHostHooks for CaptureHost {
      fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {}

      fn host_load_imported_module(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _modules: &mut vm_js::ModuleGraph,
        referrer: ModuleReferrer,
        request: ModuleRequest,
        _host_defined: vm_js::HostDefined,
        payload: ModuleLoadPayload,
      ) -> Result<(), VmError> {
        if self.captured.is_none() {
          self.captured = Some((referrer, request, payload));
        }
        Ok(())
      }
    }

    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap).expect("Realm::new");

    let payload_clone = {
      let mut scope = heap.scope();

      let root_record = vm_js::SourceTextModuleRecord::parse(scope.heap_mut(), "import './dep.js';")
        .expect("parse root module");
      let mut modules = vm_js::ModuleGraph::new();
      let root_id = modules.add_module(root_record).expect("add module");

      let mut host = CaptureHost { captured: None };
      let _promise = vm_js::load_requested_modules(
        &mut vm,
        &mut scope,
        &mut modules,
        &mut host,
        root_id,
        vm_js::HostDefined::default(),
      )
      .expect("load_requested_modules");

      let (referrer, request, payload) = host.captured.take().expect("expected payload");
      let payload_clone = payload.clone();

      // Clean up promise roots to avoid leaking persistent roots in debug builds.
      let _ = vm.finish_loading_imported_module(
        &mut scope,
        &mut modules,
        &mut host,
        referrer,
        request,
        payload,
        Err(VmError::Unimplemented("test cleanup")),
      );

      payload_clone
    };

    realm.teardown(&mut heap);
    payload_clone
  }

  #[test]
  fn resolves_relative_specifiers_against_referrer_module_url() {
    let mut loader = ModuleLoader::new(Some("https://example.com/doc/page.html".to_string()));

    // Seed a module URL so we have a referrer base for resolution.
    let module_id = ModuleId::from_raw(1);
    loader
      .module_id_to_url
      .insert(module_id, "https://example.com/dir/a.js".to_string());

    let resolved = loader.resolve_request_url(
      ModuleReferrer::Module(module_id),
      &ModuleRequest::new(JsString::from_str("./b.js").unwrap(), Vec::new()),
    );
    assert_eq!(
      resolved.unwrap(),
      "https://example.com/dir/b.js",
      "expected relative specifier to resolve against referrer module URL"
    );
  }

  #[test]
  fn resolves_relative_specifiers_against_document_url_for_realm_referrer() {
    let mut loader = ModuleLoader::new(Some("https://example.com/doc/page.html".to_string()));
    let resolved = loader.resolve_request_url(
      ModuleReferrer::Realm(vm_js::RealmId::from_raw(1)),
      &ModuleRequest::new(JsString::from_str("./b.js").unwrap(), Vec::new()),
    );
    assert_eq!(resolved.unwrap(), "https://example.com/doc/b.js");
  }

  #[test]
  fn resolves_relative_specifiers_against_script_url_for_script_referrer() {
    let mut loader = ModuleLoader::new(Some("https://example.com/doc/page.html".to_string()));
    let script_id = ScriptId::from_raw(1);
    loader
      .register_script_url(script_id, "https://example.com/scripts/main.js".to_string())
      .expect("register script url");
    let resolved = loader.resolve_request_url(
      ModuleReferrer::Script(script_id),
      &ModuleRequest::new(JsString::from_str("./b.js").unwrap(), Vec::new()),
    );
    assert_eq!(resolved.unwrap(), "https://example.com/scripts/b.js");
  }

  #[test]
  fn bare_specifiers_fail_deterministically() {
    let mut loader = ModuleLoader::new(Some("https://example.com/doc/page.html".to_string()));
    let err = loader
      .resolve_request_url(
        ModuleReferrer::Realm(vm_js::RealmId::from_raw(1)),
        &ModuleRequest::new(JsString::from_str("foo").unwrap(), Vec::new()),
      )
      .unwrap_err();
    assert_eq!(err, ModuleResolveError::BareSpecifier);
  }

  #[test]
  fn module_map_caches_loaded_modules_and_skips_refetch() {
    let payload = make_dummy_payload();
    let mut fetcher = MapFetcher::default();
    fetcher.insert("https://example.com/a.js", b"export const x = 1;");
    let fetcher = Arc::new(fetcher);
    let fetcher_for_loader: Arc<dyn ResourceFetcher> = fetcher.clone();

    let mut loader = ModuleLoader::new(Some("https://example.com/doc/page.html".to_string()));
    loader.set_fetcher(fetcher_for_loader);

    let request = ModuleRequest::new(JsString::from_str("https://example.com/a.js").unwrap(), Vec::new());

    let outcome = loader.request_module(
      ModuleReferrer::Realm(vm_js::RealmId::from_raw(1)),
      &request,
      &payload,
    );
    let ModuleLoadOutcome::StartFetch(key) = outcome else {
      panic!("expected StartFetch for first request, got {outcome:?}");
    };
    assert_eq!(
      fetcher
        .fetch_count
        .load(std::sync::atomic::Ordering::Relaxed),
      0,
      "fetcher should not be called until the host starts fetching"
    );

    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut modules = vm_js::ModuleGraph::new();
    let (waiters, result) = loader
      .fetch_and_register(&mut heap, &mut modules, key.clone())
      .expect("expected inflight entry");
    assert_eq!(waiters.len(), 1);
    let module_id = result.expect("expected module load to succeed");
    assert_eq!(
      fetcher
        .fetch_count
        .load(std::sync::atomic::Ordering::Relaxed),
      1,
      "expected exactly one fetch for the first load"
    );

    // Second request should hit the module map and not start a new fetch.
    let outcome = loader.request_module(
      ModuleReferrer::Realm(vm_js::RealmId::from_raw(1)),
      &request,
      &payload,
    );
    match outcome {
      ModuleLoadOutcome::FinishNow(Ok(id)) => assert_eq!(id, module_id),
      other => panic!("expected FinishNow(Ok(..)) for cached request, got {other:?}"),
    };
    assert_eq!(
      fetcher
        .fetch_count
        .load(std::sync::atomic::Ordering::Relaxed),
      1,
      "expected cached load to not re-fetch"
    );
  }

  #[test]
  fn inflight_deduplicates_concurrent_requests() {
    let payload = make_dummy_payload();
    let mut fetcher = MapFetcher::default();
    fetcher.insert("https://example.com/a.js", b"export const x = 1;");
    let fetcher = Arc::new(fetcher);
    let fetcher_for_loader: Arc<dyn ResourceFetcher> = fetcher.clone();

    let mut loader = ModuleLoader::new(Some("https://example.com/doc/page.html".to_string()));
    loader.set_fetcher(fetcher_for_loader);

    let request = ModuleRequest::new(JsString::from_str("https://example.com/a.js").unwrap(), Vec::new());

    let outcome1 = loader.request_module(
      ModuleReferrer::Realm(vm_js::RealmId::from_raw(1)),
      &request,
      &payload,
    );
    let ModuleLoadOutcome::StartFetch(key1) = outcome1 else {
      panic!("expected StartFetch for first request, got {outcome1:?}");
    };

    // Second request joins inflight and should not start a second fetch.
    let outcome2 = loader.request_module(
      ModuleReferrer::Realm(vm_js::RealmId::from_raw(1)),
      &request,
      &payload,
    );
    assert!(
      matches!(outcome2, ModuleLoadOutcome::InFlight),
      "expected inflight request to be deduped, got {outcome2:?}"
    );

    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut modules = vm_js::ModuleGraph::new();
    let (waiters, result) = loader
      .fetch_and_register(&mut heap, &mut modules, key1)
      .expect("expected inflight entry");
    assert_eq!(
      fetcher
        .fetch_count
        .load(std::sync::atomic::Ordering::Relaxed),
      1,
      "expected exactly one fetch for deduped in-flight requests"
    );
    assert_eq!(waiters.len(), 2, "expected two inflight waiters");
    let module_id = result.expect("expected module load to succeed");
    assert_eq!(
      loader.module_url(module_id),
      Some("https://example.com/a.js")
    );
  }
}
