use crate::animation::TransitionState;
use crate::debug::runtime::RuntimeToggles;
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::{Point, Rect, Size};
use crate::interaction::InteractionState;
use crate::interaction::state::DocumentSelectionStateDom2;
use crate::clock::{Clock, RealClock};
use crate::js::host_document::{ActiveEventGuard, ActiveEventStack};
use crate::js::CurrentScriptStateHandle;
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;
use crate::style::cascade::StyledNode;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxType, FormControlKind, ReplacedType, SelectItem};
use crate::web::dom::DocumentVisibilityState;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use super::browser_document::prepare_dom_inner;
use super::{PreparedDocument, PreparedPaintOptions, RenderOptions};

/// Counters describing how `BrowserDocumentDom2` satisfied invalidations over time.
///
/// These are intended for tests and performance diagnostics; they are conservative and prioritize
/// correctness over minimality (i.e. a fall back to a full pipeline run is counted as "full" even if
/// only a small part of the document changed).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BrowserDocumentDom2InvalidationCounters {
  /// Full style recomputations (cascade + layout).
  pub full_restyles: u64,
  /// Incremental style recomputations that reuse unaffected subtrees from the previous styled tree.
  pub incremental_restyles: u64,
  /// Full layout recomputations that included a restyle.
  pub full_relayouts: u64,
  /// Layout recomputations performed without rerunning cascade.
  pub incremental_relayouts: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserDocumentDom2LayoutFlushKind {
  /// No layout work was required; cached layout artifacts were already up to date.
  Noop,
  /// Layout was satisfied via an incremental relayout (currently: text-only and some form-control
  /// text changes).
  IncrementalRelayout,
  /// Layout required a full pipeline run (DOM snapshot + cascade + layout).
  Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserDocumentDom2LayoutFlushErrorRecovery {
  /// Restore the previous prepared cache on incremental relayout errors.
  RestorePreparedCache,
  /// Preserve (possibly partially updated) prepared artifacts on incremental relayout errors so
  /// callers can retry without discarding intermediate state.
  PreservePreparedCache,
}

#[derive(Debug, Clone, Copy)]
struct BrowserDocumentDom2LayoutFlushRequest {
  /// When true, ensure `last_dom_mapping` is available so renderer preorder IDs can be mapped back
  /// to stable `dom2::NodeId`s.
  require_dom_mapping: bool,
  /// How to handle incremental relayout errors.
  incremental_error_recovery: BrowserDocumentDom2LayoutFlushErrorRecovery,
}

impl BrowserDocumentDom2LayoutFlushRequest {
  const FOR_RENDER_OR_DOM_QUERIES: Self = Self {
    require_dom_mapping: true,
    incremental_error_recovery: BrowserDocumentDom2LayoutFlushErrorRecovery::RestorePreparedCache,
  };

  const FOR_HIT_TESTING: Self = Self {
    require_dom_mapping: true,
    incremental_error_recovery: BrowserDocumentDom2LayoutFlushErrorRecovery::RestorePreparedCache,
  };
}

/// Result of hit testing in viewport coordinates, including both renderer hit metadata and a stable
/// `dom2::NodeId` for JS/event dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dom2HitTestResult {
  /// Stable `dom2` node id corresponding to `hit.dom_node_id` (renderer preorder id).
  pub node: crate::dom2::NodeId,
  /// Hit-test metadata computed from layout/box tree.
  pub hit: crate::interaction::HitTestResult,
}

/// Mutable, multi-frame renderer backed by a live `dom2` document.
///
/// `BrowserDocumentDom2` mirrors [`super::BrowserDocument`] but stores a spec-ish mutable
/// [`crate::dom2::Document`] as the authoritative DOM (e.g. for JavaScript). The renderer only
/// snapshots the `dom2` document into the renderer's immutable [`crate::dom::DomNode`] form when a
/// layout recomputation is needed.
///
/// This type does **not** execute JavaScript or run an HTML event loop by itself. JavaScript
/// execution is hosted by [`super::BrowserTab`] (or by a custom embedder built on top of `dom2`).
pub struct BrowserDocumentDom2 {
  renderer: super::FastRender,
  dom: Box<crate::dom2::Document>,
  active_events: ActiveEventStack,
  visibility_state: DocumentVisibilityState,
  /// Host-side `Document.currentScript` bookkeeping shared with JS bindings.
  ///
  /// `BrowserTabHost` owns the authoritative current-script state, but `vm-js` native handlers see
  /// the embedder `VmHost` as the document (`BrowserDocumentDom2`). Storing the handle here allows
  /// `document.currentScript` to be resolved via downcast on the real `VmHost` without relying on
  /// any per-call host shim.
  current_script: CurrentScriptStateHandle,
  options: RenderOptions,
  media_provider: Option<Arc<dyn crate::media::MediaFrameProvider>>,
  prepared: Option<PreparedDocument>,
  last_dom_mapping: Option<crate::dom2::RendererDomMapping>,
  animation_state_store: crate::animation::AnimationStateStore,
  style_dirty: bool,
  layout_dirty: bool,
  paint_dirty: bool,
  interaction_state: Option<InteractionState>,
  /// Optional document selection keyed by stable `dom2::NodeId` endpoints.
  ///
  /// This allows callers to store selection state robustly across DOM mutations (where renderer
  /// preorder ids can shift). The selection is projected through the most recent
  /// [`crate::dom2::RendererDomMapping`] at paint time.
  document_selection_dom2: Option<DocumentSelectionStateDom2>,
  /// Hash of the most recently prepared interaction state's CSS-affecting subset.
  ///
  /// This captures pseudo-class matching (`:hover`, `:focus`, etc.) and other inputs that influence
  /// selector matching / the CSS cascade. When this changes, cached style/layout results must be
  /// treated as invalid even if the DOM itself is unchanged.
  interaction_css_hash: u64,
  /// Hash of the most recently painted interaction state's paint-only subset.
  ///
  /// This captures state that affects painting but must not force a cascade/layout rerun (caret /
  /// selection / IME preedit / document selection / file-input labels).
  interaction_paint_hash: u64,
  dirty_style_nodes: FxHashSet<crate::dom2::NodeId>,
  /// Nodes whose style may change due to render-affecting form-control state mutations.
  ///
  /// Unlike `dirty_style_nodes`, this set does **not** imply a stylesheet-affecting mutation (and
  /// must not force `style_dirty = true`). It is only used as an input to incremental restyle reuse
  /// so the cascade can recompute `:checked`, `:placeholder-shown`, attribute selectors on
  /// projected `value`, etc without recascading the entire document.
  dirty_form_state_style_nodes: FxHashSet<crate::dom2::NodeId>,
  dirty_text_nodes: FxHashSet<crate::dom2::NodeId>,
  dirty_structure_nodes: FxHashSet<crate::dom2::NodeId>,
  /// Whether we observed a `dom2::MutationLog.form_state_changed` mutation since the last layout
  /// flush.
  ///
  /// Form control state is projected into the renderer DOM snapshot before cascade/layout (see
  /// `dom2::Document::project_form_control_state_into_renderer_dom_snapshot`). Incremental relayout
  /// paths that patch an existing box tree (e.g. for text node changes) do not currently update that
  /// projected state, so we must force a full pipeline run when it changes.
  form_state_dirty: bool,
  /// Whether the author stylesheet contains any `:has(...)` selectors.
  ///
  /// `:has()` introduces reverse dependencies (ancestors/previous siblings) that our current dom2
  /// dirty sets do not model, so incremental restyle reuse must be disabled when present.
  author_stylesheet_has_has_selectors: bool,
  invalidation_counters: BrowserDocumentDom2InvalidationCounters,
  last_seen_dom_mutation_generation: u64,
  realtime_animations_enabled: bool,
  animation_clock: Arc<dyn Clock>,
  animation_timeline_origin: Option<Duration>,
  last_painted_animation_clock: Option<Duration>,
  last_painted_animation_time: Option<f32>,
}

fn interaction_state_css_fingerprint(state: Option<&InteractionState>) -> u64 {
  let mut hasher = DefaultHasher::new();
  match state {
    None => {
      0u8.hash(&mut hasher);
    }
    Some(state) => {
      1u8.hash(&mut hasher);
      // Avoid per-frame hashing/sorting of large sets by using the cached interaction digest.
      state.interaction_css_hash().hash(&mut hasher);
    }
  }
  hasher.finish()
}

fn interaction_state_paint_fingerprint(state: Option<&InteractionState>) -> u64 {
  let mut hasher = DefaultHasher::new();
  match state {
    None => {
      0u8.hash(&mut hasher);
    }
    Some(state) => {
      1u8.hash(&mut hasher);
      // Avoid per-frame hashing/sorting of large sets/maps by using the cached interaction digest.
      state.interaction_paint_hash().hash(&mut hasher);
    }
  }
  hasher.finish()
}

impl BrowserDocumentDom2 {
  /// Creates a new live document from an HTML string using a fresh renderer.
  pub fn from_html(html: &str, options: RenderOptions) -> Result<Self> {
    Self::new(super::FastRender::new()?, html, options)
  }

  /// Creates a new live document from an HTML string using the provided renderer.
  pub fn new(renderer: super::FastRender, html: &str, options: RenderOptions) -> Result<Self> {
    // Keep HTML parsing cancellable via `RenderOptions::{timeout,cancel_callback}` (see
    // `BrowserDocument::new` for details).
    let deadline_enabled = options.timeout.is_some() || options.cancel_callback.is_some();
    let dom = if deadline_enabled {
      let deadline = crate::render_control::RenderDeadline::new(
        options.timeout,
        options.cancel_callback.clone(),
      );
      let _guard = crate::render_control::DeadlineGuard::install(Some(&deadline));
      renderer.parse_html(html)?
    } else {
      renderer.parse_html(html)?
    };
    let dom = crate::dom2::Document::from_renderer_dom(&dom);
    let last_seen_dom_mutation_generation = dom.mutation_generation();
    let dom = Box::new(dom);
    Ok(Self {
      renderer,
      dom,
      active_events: ActiveEventStack::default(),
      visibility_state: DocumentVisibilityState::Visible,
      current_script: CurrentScriptStateHandle::default(),
      options,
      media_provider: None,
      prepared: None,
      last_dom_mapping: None,
      animation_state_store: crate::animation::AnimationStateStore::new(),
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
      interaction_state: None,
      document_selection_dom2: None,
      interaction_css_hash: interaction_state_css_fingerprint(None),
      interaction_paint_hash: interaction_state_paint_fingerprint(None),
      dirty_style_nodes: FxHashSet::default(),
      dirty_form_state_style_nodes: FxHashSet::default(),
      dirty_text_nodes: FxHashSet::default(),
      dirty_structure_nodes: FxHashSet::default(),
      form_state_dirty: false,
      author_stylesheet_has_has_selectors: false,
      invalidation_counters: BrowserDocumentDom2InvalidationCounters::default(),
      last_seen_dom_mutation_generation,
      realtime_animations_enabled: false,
      animation_clock: Arc::new(RealClock::default()),
      animation_timeline_origin: None,
      last_painted_animation_clock: None,
      last_painted_animation_time: None,
    })
  }

  pub(crate) fn push_active_event(
    &mut self,
    event_id: u64,
    event: &mut crate::web::events::Event,
  ) -> ActiveEventGuard {
    self.active_events.push(event_id, event)
  }

  pub(crate) fn with_active_event<R>(
    &mut self,
    event_id: u64,
    f: impl FnOnce(&mut crate::web::events::Event) -> R,
  ) -> Option<R> {
    self.active_events.with_event(event_id, f)
  }

  pub(crate) fn current_script_handle(&self) -> &CurrentScriptStateHandle {
    &self.current_script
  }

  pub(crate) fn set_current_script_handle(&mut self, handle: CurrentScriptStateHandle) {
    self.current_script = handle;
  }

  pub(crate) fn visibility_state(&self) -> DocumentVisibilityState {
    self.visibility_state
  }

  pub(crate) fn set_visibility_state(&mut self, state: DocumentVisibilityState) {
    self.visibility_state = state;
  }

  /// Fetches and prepares a URL using the internal renderer, replacing the live `dom2` document
  /// in-place.
  pub fn navigate_url(
    &mut self,
    url: &str,
    options: RenderOptions,
  ) -> Result<super::BrowserNavigationReport> {
    let prev_document_url = self.renderer.document_url.clone();
    let prev_base_url = self.renderer.base_url.clone();
    let prev_document_csp = self.renderer.document_csp.clone();
    let super::PreparedDocumentReport {
      document,
      diagnostics,
      final_url,
      base_url,
    } = match self.renderer.prepare_url(url, options.clone()) {
      Ok(report) => report,
      Err(err) => {
        // Restore URL hints so cancellation/errors don't perturb the currently committed document.
        self.renderer.document_url = prev_document_url;
        match prev_base_url {
          Some(url) => self.renderer.set_base_url(url),
          None => self.renderer.clear_base_url(),
        }
        self.renderer.document_csp = prev_document_csp;
        return Err(err);
      }
    };

    self.reset_with_prepared(document, options);

    Ok(super::BrowserNavigationReport {
      diagnostics,
      final_url,
      base_url,
    })
  }

  /// Overrides the clock used to derive the document timeline for real-time animation sampling.
  ///
  /// This resets the timeline origin so the next frame starts at 0ms (when real-time sampling is
  /// enabled).
  pub fn set_animation_clock(&mut self, clock: Arc<dyn Clock>) {
    self.animation_clock = clock;
    self.animation_timeline_origin = None;
    self.animation_state_store = crate::animation::AnimationStateStore::new();
    self.last_painted_animation_clock = None;
    self.last_painted_animation_time = None;
    self.paint_dirty = true;
  }

  /// Enables/disables real-time animation sampling based on this document's timeline.
  ///
  /// When enabled and `RenderOptions.animation_time` is `None`, each paint call samples CSS
  /// animations/transitions at the time elapsed since the first rendered frame after enabling.
  pub fn set_realtime_animations_enabled(&mut self, enabled: bool) {
    if enabled && !self.realtime_animations_enabled {
      self.realtime_animations_enabled = true;
      self.animation_timeline_origin = None;
      self.animation_state_store = crate::animation::AnimationStateStore::new();
      self.last_painted_animation_clock = None;
      self.last_painted_animation_time = None;
      self.paint_dirty = true;
    } else if !enabled && self.realtime_animations_enabled {
      self.realtime_animations_enabled = false;
      self.animation_timeline_origin = None;
      self.animation_state_store = crate::animation::AnimationStateStore::new();
      self.last_painted_animation_clock = None;
      self.last_painted_animation_time = None;
      self.paint_dirty = true;
    }
  }

  /// Overrides the media provider used during paint (e.g. to supply `<video>` frames).
  ///
  /// Changing the provider only invalidates paint; cached style/layout artifacts remain valid.
  pub fn set_media_provider(&mut self, provider: Option<Arc<dyn crate::media::MediaFrameProvider>>) {
    self.media_provider = provider;
    self.paint_dirty = true;
  }

  pub fn needs_animation_frame(&self) -> bool {
    if self.options.animation_time.is_some() || !self.realtime_animations_enabled {
      return false;
    }
    let Some(last) = self.last_painted_animation_clock else {
      return self.prepared.is_some();
    };
    self.animation_clock.now() != last
  }

  pub(crate) fn renderer_mut(&mut self) -> &mut super::FastRender {
    &mut self.renderer
  }

  pub(crate) fn document_csp(&self) -> Option<crate::html::content_security_policy::CspPolicy> {
    self.renderer.document_csp.clone()
  }

  pub(crate) fn shared_diagnostics(&self) -> Option<super::SharedRenderDiagnostics> {
    self
      .renderer
      .diagnostics
      .as_ref()
      .map(|diag| super::SharedRenderDiagnostics {
        inner: Arc::clone(diag),
      })
  }

  pub(crate) fn fetcher(&self) -> std::sync::Arc<dyn crate::resource::ResourceFetcher> {
    self.renderer.resource_fetcher()
  }

  pub fn options(&self) -> &RenderOptions {
    &self.options
  }

  /// Replaces the live DOM and clears any cached preparation state.
  pub fn reset_with_dom(&mut self, dom: crate::dom2::Document, options: RenderOptions) {
    self.current_script.reset();
    self.last_seen_dom_mutation_generation = dom.mutation_generation();
    let dom = Box::new(dom);
    self.dom = dom;
    self.options = options;
    self.prepared = None;
    self.last_dom_mapping = None;
    self.interaction_state = None;
    self.document_selection_dom2 = None;
    self.interaction_css_hash = interaction_state_css_fingerprint(None);
    self.interaction_paint_hash = interaction_state_paint_fingerprint(None);
    self.author_stylesheet_has_has_selectors = false;
    // Reset per-document CSP state. `reset_with_dom` replaces the entire document, so any previously
    // captured CSP headers/meta should not leak into the new DOM.
    self.renderer.document_csp = None;
    self.animation_state_store = crate::animation::AnimationStateStore::new();
    self.animation_timeline_origin = None;
    self.last_painted_animation_clock = None;
    self.last_painted_animation_time = None;
    self.invalidate_all();
  }

  /// Replaces the live DOM with a prepared document's DOM and installs the prepared cache.
  ///
  /// The next `render_if_needed` call will paint using the prepared layout without re-running
  /// cascade/layout.
  pub fn reset_with_prepared(&mut self, prepared: PreparedDocument, options: RenderOptions) {
    self.current_script.reset();
    let author_has_has = prepared.stylesheet().contains_has_selectors();
    let dom = crate::dom2::Document::from_renderer_dom(&prepared.dom);
    self.last_seen_dom_mutation_generation = dom.mutation_generation();
    let dom = Box::new(dom);
    self.dom = dom;
    self.options = options;
    self.prepared = Some(prepared);
    self.last_dom_mapping = Some(self.dom.as_ref().to_renderer_dom_with_mapping().mapping);
    self.animation_state_store = crate::animation::AnimationStateStore::new();
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = true;
    self.interaction_state = None;
    self.document_selection_dom2 = None;
    self.interaction_css_hash = interaction_state_css_fingerprint(None);
    self.interaction_paint_hash = interaction_state_paint_fingerprint(None);
    self.dirty_style_nodes.clear();
    self.dirty_form_state_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
    self.form_state_dirty = false;
    self.animation_timeline_origin = None;
    self.last_painted_animation_clock = None;
    self.last_painted_animation_time = None;
    self.author_stylesheet_has_has_selectors = author_has_has;
  }

  /// Parses HTML using the internal renderer and resets the document state.
  pub fn reset_with_html(&mut self, html: &str, options: RenderOptions) -> Result<()> {
    let deadline_enabled = options.timeout.is_some() || options.cancel_callback.is_some();
    let dom = if deadline_enabled {
      let deadline = crate::render_control::RenderDeadline::new(
        options.timeout,
        options.cancel_callback.clone(),
      );
      let _guard = crate::render_control::DeadlineGuard::install(Some(&deadline));
      self.renderer.parse_html(html)?
    } else {
      self.renderer.parse_html(html)?
    };
    let dom = crate::dom2::Document::from_renderer_dom(&dom);
    self.reset_with_dom(dom, options);
    Ok(())
  }

  /// Returns a stable pointer to this document's backing `dom2::Document`.
  ///
  /// The returned pointer is stable even when the `BrowserDocumentDom2` is moved, because the live
  /// DOM is stored on the heap.
  pub fn dom_non_null(&mut self) -> NonNull<crate::dom2::Document> {
    NonNull::from(self.dom.as_mut())
  }

  /// Returns an immutable reference to the live `dom2` document.
  pub fn dom(&self) -> &crate::dom2::Document {
    self.dom.as_ref()
  }

  /// Returns the nearest scroll container ancestor for `element`, approximating
  /// `HTMLElement.scrollParent`.
  pub(crate) fn element_scroll_parent(
    &mut self,
    element: crate::dom2::NodeId,
  ) -> Option<crate::dom2::NodeId> {
    if self.ensure_layout_for_dom_queries().is_err() {
      return None;
    }
    let dom = self.dom();

    match dom.node(element).kind {
      crate::dom2::NodeKind::Element { .. } | crate::dom2::NodeKind::Slot { .. } => {}
      _ => return None,
    }

    if dom.document_element() == Some(element) {
      return None;
    }
    if dom.body() == Some(element) {
      return None;
    }

    let prepared = self.prepared.as_ref()?;
    let mapping = self.last_dom_mapping.as_ref()?;

    let mut principal_styles: FxHashMap<usize, &crate::style::ComputedStyle> =
      FxHashMap::default();
    let mut stack: Vec<&BoxNode> = vec![&prepared.box_tree().root];
    while let Some(node) = stack.pop() {
      if node.generated_pseudo.is_none() {
        if let Some(styled_id) = node.styled_node_id {
          principal_styles.entry(styled_id).or_insert(node.style.as_ref());
        }
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }

    let element_preorder = mapping.preorder_for_node_id(element)?;
    let element_style = principal_styles.get(&element_preorder).copied()?;

    if element_style.display.is_none() {
      return None;
    }

    if matches!(element_style.position, crate::style::position::Position::Fixed) {
      let mut current = element;
      let mut has_fixed_cb = false;
      while let Some(parent) = dom.parent_node(current) {
        if matches!(
          dom.node(parent).kind,
          crate::dom2::NodeKind::Element { .. } | crate::dom2::NodeKind::Slot { .. }
        ) {
          if let Some(parent_preorder) = mapping.preorder_for_node_id(parent) {
            if let Some(parent_style) = principal_styles.get(&parent_preorder).copied() {
              if !parent_style.transform.is_empty() || parent_style.perspective.is_some() {
                has_fixed_cb = true;
                break;
              }
            }
          }
        }
        current = parent;
      }
      if !has_fixed_cb {
        return None;
      }
    }

    let is_scroll_container = |style: &crate::style::ComputedStyle| -> bool {
      use crate::style::types::Overflow;
      matches!(style.overflow_x, Overflow::Auto | Overflow::Scroll | Overflow::Hidden)
        || matches!(style.overflow_y, Overflow::Auto | Overflow::Scroll | Overflow::Hidden)
    };

    let mut current = element;
    while let Some(parent) = dom.parent_node(current) {
      if matches!(
        dom.node(parent).kind,
        crate::dom2::NodeKind::Element { .. } | crate::dom2::NodeKind::Slot { .. }
      ) {
        if let Some(parent_preorder) = mapping.preorder_for_node_id(parent) {
          if let Some(parent_style) = principal_styles.get(&parent_preorder).copied() {
            if is_scroll_container(parent_style) {
              return Some(parent);
            }
          }
        }
      }
      current = parent;
    }

    // Fallback to the document scrolling element (CSSOM View).
    //
    // In quirks mode the scrolling element is usually `<body>`, otherwise it is the document
    // element (`<html>`).
    let quirks_mode = match &dom.node(dom.root()).kind {
      crate::dom2::NodeKind::Document { quirks_mode } => *quirks_mode,
      _ => selectors::context::QuirksMode::NoQuirks,
    };
    if quirks_mode == selectors::context::QuirksMode::Quirks {
      if let Some(body) = dom.body() {
        return Some(body);
      }
    }
    dom.document_element()
  }

  /// Returns a monotonically increasing counter that changes whenever the DOM might have mutated.
  ///
  /// This is intended for host integrations that need to perform bounded whole-document scans (for
  /// example: detecting dynamically inserted `<script>` elements after JS-driven DOM mutations).
  pub fn dom_mutation_generation(&self) -> u64 {
    self.dom.mutation_generation()
  }

  /// Returns a mutable reference to the live `dom2` document, marking the document dirty.
  ///
  /// Note: `dom_mut()` is intentionally conservative. Callers that want incremental invalidation
  /// should prefer [`BrowserDocumentDom2::mutate_dom`] or JS bindings that route mutations through
  /// `DomHost::mutate_dom`.
  pub fn dom_mut(&mut self) -> &mut crate::dom2::Document {
    self.invalidate_all();
    self.dom.clear_mutations();
    self.dom.as_mut()
  }

  /// Mutates the DOM tree, marking the document dirty only when `f` reports that it changed
  /// something.
  ///
  /// When possible, mutations are classified to avoid re-running expensive pipeline stages (e.g.
  /// text updates can often skip cascade).
  pub fn mutate_dom<F>(&mut self, f: F) -> bool
  where
    F: FnOnce(&mut crate::dom2::Document) -> bool,
  {
    let generation_before = self.dom.mutation_generation();
    let generation_in_sync_before =
      generation_before == self.last_seen_dom_mutation_generation;
    let changed = f(self.dom.as_mut());
    if changed {
      let mutations = self.dom.take_mutations();
      if mutations.is_empty() {
        // The caller reported changes but we have no structured mutation data (e.g. direct `node_mut`
        // edits). Fall back to a full invalidation to preserve correctness.
        self.invalidate_all();
      } else {
        let render_affecting = self.apply_mutation_log(mutations);
        if !render_affecting && generation_in_sync_before {
          // `dom2::Document` bumps `mutation_generation` for *all* mutations, including changes in
          // disconnected/inert subtrees. `apply_mutation_log` filters these changes out because they
          // cannot affect rendering, so keep the host "clean" by recording that we've already seen
          // this generation.
          //
          // Guard against clearing a generation mismatch that predates this `mutate_dom` call (for
          // example: out-of-band mutations performed through a raw `dom2::Document` pointer).
          self.last_seen_dom_mutation_generation = self.dom.mutation_generation();
        }
      }
    } else {
      // Ensure no stale mutation records linger across no-op closures.
      self.dom.clear_mutations();
    }
    changed
  }

  pub(crate) fn dom_ptr(&self) -> NonNull<crate::dom2::Document> {
    NonNull::from(self.dom.as_ref())
  }

  /// Updates the viewport size (in CSS px), marking layout+paint dirty.
  pub fn set_viewport(&mut self, width: u32, height: u32) {
    self.options.viewport = Some((width, height));
    // Viewport changes can affect media queries and thus cascade.
    self.style_dirty = true;
    self.layout_dirty = true;
    self.paint_dirty = true;
  }

  /// Updates the interaction state used for pseudo-class matching and form-control paint hints.
  ///
  /// Interaction state is keyed by renderer DOM pre-order IDs (see
  /// `crate::dom::enumerate_dom_ids`).
  ///
  /// Changes that can affect selector matching / cascade (`:hover`, `:active`, `:focus`, etc.)
  /// invalidate style/layout. Paint-only changes (document selection highlights, caret/IME preedit,
  /// file-input labels) only invalidate paint and are applied during cached paint via
  /// `interaction::paint_overlays`.
  pub fn set_interaction_state(&mut self, state: Option<InteractionState>) {
    let css_fingerprint = interaction_state_css_fingerprint(state.as_ref());
    let paint_fingerprint = interaction_state_paint_fingerprint(state.as_ref());
    self.interaction_state = state;
    if css_fingerprint != self.interaction_css_hash {
      self.invalidate_all();
    } else if paint_fingerprint != self.interaction_paint_hash {
      self.paint_dirty = true;
    }
  }

  /// Updates the document (non-form-control) selection using stable `dom2::NodeId` endpoints.
  ///
  /// This selection is projected through the current renderer DOM mapping so it remains stable
  /// across DOM mutations that change renderer preorder ids.
  pub fn set_document_selection_dom2(&mut self, selection: Option<DocumentSelectionStateDom2>) {
    self.document_selection_dom2 = selection;
    self.paint_dirty = true;
    self.apply_document_selection_overlay();
  }

  /// Updates the device pixel ratio used for media queries and resolution-dependent resources.
  ///
  /// Non-finite or non-positive values clear the override (falling back to the renderer default).
  /// Changing DPR invalidates layout+paint.
  pub fn set_device_pixel_ratio(&mut self, dpr: f32) {
    let sanitized = super::sanitize_scale(Some(dpr));
    if sanitized != self.options.device_pixel_ratio {
      self.options.device_pixel_ratio = sanitized;
      // DPR affects media queries (`resolution`) and resource selection (`image-set`).
      self.style_dirty = true;
      self.layout_dirty = true;
      self.paint_dirty = true;
    }
  }

  /// Updates (or clears) the runtime toggle override used by the renderer.
  ///
  /// Runtime toggles can affect the media-query surface (e.g. `prefers-*`) via
  /// [`crate::style::media::MediaContext::with_env_overrides`], so changing them must invalidate
  /// style/layout (and therefore paint).
  pub fn set_runtime_toggles(&mut self, toggles: Option<Arc<RuntimeToggles>>) {
    let changed = match (&self.options.runtime_toggles, &toggles) {
      (None, None) => false,
      (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
      _ => true,
    };
    if changed {
      self.options.runtime_toggles = toggles;
      self.invalidate_all();
    }
  }

  /// Marks the paint stage dirty without invalidating style/layout.
  ///
  /// This is intended for dynamic sources whose pixels can change between frames without any DOM
  /// mutations (for example: video playback). Calling this ensures the next
  /// [`render_if_needed`](Self::render_if_needed) produces a fresh frame while reusing cached
  /// style/layout artifacts when possible.
  ///
  /// This sets `paint_dirty = true` while leaving `style_dirty`/`layout_dirty` unchanged, and does
  /// not clear any existing dirtiness flags.
  pub fn invalidate_paint(&mut self) {
    self.paint_dirty = true;
  }

  /// Marks the layout stage dirty without invalidating style.
  ///
  /// This is intended for dynamic sources whose intrinsic sizing information can change between
  /// frames without any DOM mutations (for example: video metadata becoming available, or aspect
  /// ratio updates). Calling this ensures the next [`render_if_needed`](Self::render_if_needed)
  /// recomputes layout (and then paints) while allowing cached style artifacts to be reused when
  /// possible.
  ///
  /// This sets `layout_dirty = true` and `paint_dirty = true` while leaving `style_dirty`
  /// unchanged, and does not clear any existing dirtiness flags.
  pub fn invalidate_layout(&mut self) {
    self.layout_dirty = true;
    self.paint_dirty = true;
  }

  /// Returns true when style/layout must be recomputed before painting.
  pub fn needs_layout(&self) -> bool {
    self.prepared.is_none()
      || self.style_dirty
      || self.layout_dirty
      || interaction_state_css_fingerprint(self.interaction_state.as_ref()) != self.interaction_css_hash
      || self.dom.mutation_generation() != self.last_seen_dom_mutation_generation
  }

  /// Ensures style/layout caches are up-to-date for DOM/layout queries without painting.
  ///
  /// This mirrors the "layout" portion of [`BrowserDocumentDom2::render_frame_with_deadlines`] while
  /// intentionally skipping paint/pixmap allocation. It is the host-side foundation for CSSOM View
  /// APIs such as `getComputedStyle` and `getBoundingClientRect`.
  ///
  /// On success:
  /// - `self.prepared` and `self.last_dom_mapping` reflect the latest layout,
  /// - style/layout dirty flags and per-node dirty sets are cleared, and
  /// - `paint_dirty` remains (or becomes) true so a subsequent `render_if_needed()` will repaint.
  pub fn ensure_layout_for_dom_queries(&mut self) -> Result<()> {
    self.flush_layout(BrowserDocumentDom2LayoutFlushRequest::FOR_RENDER_OR_DOM_QUERIES)?;
    Ok(())
  }

  /// Ensure this document has up-to-date layout artifacts, without painting.
  ///
  /// This function centralizes the common layout-flush behaviour shared by:
  /// - DOM/layout query APIs (CSSOM View, `getComputedStyle`, etc)
  /// - Hit testing (`elementFromPoint` / `elementsFromPoint`)
  ///
  /// It runs at most cascade+layout and intentionally does **not** paint.
  fn flush_layout(
    &mut self,
    request: BrowserDocumentDom2LayoutFlushRequest,
  ) -> Result<BrowserDocumentDom2LayoutFlushKind> {
    let interaction_css_hash =
      interaction_state_css_fingerprint(self.interaction_state.as_ref());
    if interaction_css_hash != self.interaction_css_hash {
      // CSS-affecting interaction state affects pseudo-class matching / selector matching, so we
      // must re-run cascade/layout when it changes.
      self.invalidate_all();
    }

    // Match `render_frame_with_deadlines`: if we haven't prepared before, force a full pipeline run
    // even if dirty flags were cleared out-of-band.
    if self.prepared.is_none() {
      self.invalidate_all();
    }

    // If the live `dom2` document was mutated out-of-band (e.g. via raw pointers), consume any
    // pending structured mutation log so incremental relayout paths can be used.
    self.sync_dirty_from_pending_dom_mutations();

    let needs_layout = self.style_dirty
      || self.layout_dirty
      || interaction_css_hash != self.interaction_css_hash
      || self.dom.mutation_generation() != self.last_seen_dom_mutation_generation
      || self.prepared.is_none()
      || (request.require_dom_mapping && self.last_dom_mapping.is_none());
    if !needs_layout {
      self.dom.clear_mutations();
      return Ok(BrowserDocumentDom2LayoutFlushKind::Noop);
    }

    // Layout without style changes can often avoid a full cascade by patching the existing box tree
    // and rerunning only layout (e.g. text content changes).
    //
    // Future incremental paths (e.g. form state changes) should be hooked up here so all layout
    // flush call sites stay in sync.
    let can_incremental_relayout = !self.style_dirty
      && !self.form_state_dirty
      && self.layout_dirty
      && !self.dirty_text_nodes.is_empty()
      && self.dirty_style_nodes.is_empty()
      && self.dirty_structure_nodes.is_empty()
      && self.prepared.is_some()
      && self.last_dom_mapping.is_some();

    let mut did_incremental_layout = false;
    if can_incremental_relayout {
      let mut prepared = self
        .prepared
        .take()
        .expect("prepared exists when can_incremental_relayout=true");
      let restore_on_incremental_error = matches!(
        request.incremental_error_recovery,
        BrowserDocumentDom2LayoutFlushErrorRecovery::RestorePreparedCache
      );

      let (prev_fragment_tree, prev_text_by_box_id) = if restore_on_incremental_error {
        let prev_fragment_tree = prepared.fragment_tree.clone();

        // Capture the box-tree text we are about to patch so we can restore on incremental failure.
        let mapping = self
          .last_dom_mapping
          .as_ref()
          .expect("mapping exists when can_incremental_relayout=true");
        let mut updated_styled_ids: FxHashSet<usize> = FxHashSet::default();
        for &node in &self.dirty_text_nodes {
          if let Some(preorder) = mapping.preorder_for_node_id(node) {
            updated_styled_ids.insert(preorder);
          }
        }

        let prev_text_by_box_id = if updated_styled_ids.is_empty() {
          FxHashMap::default()
        } else {
          capture_text_for_styled_node_ids(&prepared.box_tree.root, &updated_styled_ids)
        };

        (Some(prev_fragment_tree), prev_text_by_box_id)
      } else {
        (None, FxHashMap::default())
      };

      let incremental_result = match self.incremental_relayout_for_text_changes(&mut prepared) {
        Ok(true) => Ok(true),
        Ok(false) => self.incremental_relayout_for_form_control_text_changes(&mut prepared),
        Err(err) => Err(err),
      };

      match incremental_result {
        Ok(true) => {
          self.invalidation_counters.incremental_relayouts = self
            .invalidation_counters
            .incremental_relayouts
            .saturating_add(1);
          // Incremental relayout does not take a new renderer-DOM snapshot, but it *does* satisfy
          // the outstanding layout invalidation for the current DOM generation. Record the live DOM
          // generation so subsequent layout flushes do not force an extra full pipeline run.
          self.last_seen_dom_mutation_generation = self.dom.mutation_generation();
          self.prepared = Some(prepared);
          did_incremental_layout = true;
        }
        Ok(false) => {
          // Could not safely apply incremental relayout; fall back to a full pipeline run.
          self.prepared = Some(prepared);
        }
        Err(err) => {
          if restore_on_incremental_error {
            // Restore the last known-good layout artifacts if incremental relayout fails so callers
            // do not lose the prepared cache.
            if let Some(prev_fragment_tree) = prev_fragment_tree {
              prepared.fragment_tree = prev_fragment_tree;
            }
            if !prev_text_by_box_id.is_empty() {
              restore_text_for_box_ids(&mut prepared.box_tree.root, &prev_text_by_box_id);
            }
          }
          self.prepared = Some(prepared);
          return Err(err);
        }
      }
    }

    let mut flush_kind = BrowserDocumentDom2LayoutFlushKind::IncrementalRelayout;
    if !did_incremental_layout {
      flush_kind = BrowserDocumentDom2LayoutFlushKind::Full;
      let mut prev_prepared = self.prepared.take();
      let prev_mapping = self.last_dom_mapping.take();
      let prev_seen_generation = self.last_seen_dom_mutation_generation;

      let (mut prepared, did_incremental_restyle) = match self.prepare_dom_with_options(
        prev_prepared.as_ref(),
        prev_mapping.as_ref(),
      ) {
        Ok(result) => result,
        Err(err) => {
          self.prepared = prev_prepared;
          self.last_dom_mapping = prev_mapping;
          self.last_seen_dom_mutation_generation = prev_seen_generation;
          return Err(err);
        }
      };

      if did_incremental_restyle {
        self.invalidation_counters.incremental_restyles =
          self.invalidation_counters.incremental_restyles.saturating_add(1);
      } else {
        self.invalidation_counters.full_restyles =
          self.invalidation_counters.full_restyles.saturating_add(1);
      }
      self.invalidation_counters.full_relayouts =
        self.invalidation_counters.full_relayouts.saturating_add(1);

      let now_ms = super::sanitize_animation_time_ms(self.animation_time_for_paint());
      match now_ms {
        None => {
          prepared.fragment_tree.transition_state = None;
        }
        Some(now_ms) => {
          let prev_state = prev_prepared
            .as_ref()
            .and_then(|prepared| prepared.fragment_tree.transition_state.as_deref());
          let prev_box_tree = prev_prepared.as_ref().map(|prepared| prepared.box_tree());
          let mut transition_state = TransitionState::update_for_style_change(
            prev_state,
            prev_box_tree,
            prepared.box_tree(),
            now_ms,
          );
          transition_state.capture_layout_from_fragment_tree(&prepared.fragment_tree);
          prepared.fragment_tree.transition_state = Some(Arc::new(transition_state));
        }
      }

      if let Some(prev_prepared) = prev_prepared.as_mut() {
        let current_scroll_state = self.scroll_state();
        let next_viewport = prepared.layout_viewport();
        let anchored = crate::scroll::apply_scroll_anchoring_with_scroll_snap(
          &mut prev_prepared.fragment_tree,
          &mut prepared.fragment_tree,
          next_viewport,
          &current_scroll_state,
        );
        self.set_scroll_state(anchored);
      }

      self.prepared = Some(prepared);
    }

    // Style/layout are now satisfied, but we intentionally do not paint here.
    self.style_dirty = false;
    self.layout_dirty = false;
    self.dirty_style_nodes.clear();
    self.dirty_form_state_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
    self.form_state_dirty = false;
    // Layout changes always require a paint attempt. Keep paint marked dirty so a cancelled paint
    // can be retried.
    self.paint_dirty = true;
    self.interaction_css_hash = interaction_css_hash;
    self.dom.clear_mutations();
    Ok(flush_kind)
  }

  /// Ensures style/layout artifacts are available and up to date, without painting.
  ///
  /// Alias for [`BrowserDocumentDom2::ensure_layout_for_dom_queries`], used by geometry/scroll metric
  /// query helpers.
  pub fn ensure_layout(&mut self) -> Result<()> {
    self.ensure_layout_for_dom_queries()
  }

  /// Returns the most recently prepared layout artifacts, ensuring layout is up-to-date first.
  pub fn prepared_layout(&mut self) -> Result<&PreparedDocument> {
    self.ensure_layout_for_dom_queries()?;
    self.prepared.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no prepared layout after ensure_layout_for_dom_queries()"
          .to_string(),
      })
    })
  }

  /// Returns the principal box id for a given DOM node, when one exists.
  ///
  /// Elements like `display: contents` do not generate a principal box, in which case this returns
  /// `Ok(None)`.
  pub fn principal_box_id_for_node(&mut self, node: crate::dom2::NodeId) -> Result<Option<usize>> {
    self.ensure_layout_for_dom_queries()?;
    let prepared = self.prepared.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no prepared layout after ensure_layout_for_dom_queries()"
          .to_string(),
      })
    })?;
    let mapping = self.last_dom_mapping.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 is missing dom2↔renderer mapping after layout".to_string(),
      })
    })?;

    let Some(preorder_id) = mapping.preorder_for_node_id(node) else {
      // Detached or otherwise not in the current renderer snapshot.
      return Ok(None);
    };

    // Traverse box tree in pre-order and return the first non-pseudo box for this styled node id.
    let mut stack: Vec<&BoxNode> = vec![&prepared.box_tree().root];
    while let Some(node) = stack.pop() {
      if node.generated_pseudo.is_none() && node.styled_node_id == Some(preorder_id) {
        return Ok(Some(node.id));
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }

    Ok(None)
  }

  /// Returns the element's border-box rect in *page* coordinates.
  ///
  /// Element scroll offsets are applied; viewport scroll is not.
  pub fn border_box_rect_page(&mut self, node: crate::dom2::NodeId) -> Result<Option<Rect>> {
    let Some(box_id) = self.principal_box_id_for_node(node)? else {
      return Ok(None);
    };
    let prepared = self.prepared.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no prepared layout after ensure_layout_for_dom_queries()"
          .to_string(),
      })
    })?;
    let scroll_state = ScrollState::from_parts_with_deltas(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
      self.options.scroll_delta,
      self.options.element_scroll_deltas.clone(),
    );
    let fragment_tree = crate::interaction::hit_testing::fragment_tree_with_scroll(
      prepared.fragment_tree(),
      &scroll_state,
    );
    Ok(crate::interaction::fragment_geometry::absolute_bounds_for_box_id(&fragment_tree, box_id))
  }

  /// Returns the element's border-box rect in *viewport* coordinates.
  ///
  /// Element scroll offsets are applied and viewport scroll is subtracted.
  pub fn border_box_rect_viewport(&mut self, node: crate::dom2::NodeId) -> Result<Option<Rect>> {
    let Some(page_rect) = self.border_box_rect_page(node)? else {
      return Ok(None);
    };
    let scroll_state = ScrollState::from_parts_with_deltas(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
      self.options.scroll_delta,
      self.options.element_scroll_deltas.clone(),
    );
    Ok(Some(page_rect.translate(Point::new(
      -scroll_state.viewport.x,
      -scroll_state.viewport.y,
    ))))
  }

  /// Returns the stored scroll offset for an element scroll container by box id.
  pub fn element_scroll_offset(&self, box_id: usize) -> Point {
    self
      .options
      .element_scroll_offsets
      .get(&box_id)
      .copied()
      .unwrap_or(Point::ZERO)
  }

  /// Updates the stored scroll offset for an element scroll container by box id.
  pub fn set_element_scroll_offset(&mut self, box_id: usize, new: Point) {
    let old = self.element_scroll_offset(box_id);
    self.options.element_scroll_offsets.insert(box_id, new);
    self
      .options
      .element_scroll_deltas
      .insert(box_id, Point::new(new.x - old.x, new.y - old.y));
    self.paint_dirty = true;
  }

  /// Clamps an element scroll offset based on the current prepared layout (when possible).
  ///
  /// If the element cannot be found in the current fragment tree, this returns `desired`
  /// (sanitized for non-finite values).
  pub fn clamp_element_scroll_offset(&mut self, box_id: usize, desired: Point) -> Result<Point> {
    let prepared = self.prepared_layout()?;
    let viewport_size = prepared.layout_viewport();
    let fragment_tree = prepared.fragment_tree();

    struct Frame<'a> {
      node: &'a crate::tree::fragment_tree::FragmentNode,
      has_fixed_cb_ancestor: bool,
    }

    let mut stack: Vec<Frame<'_>> = Vec::new();
    for root in fragment_tree.additional_fragments.iter().rev() {
      stack.push(Frame {
        node: root,
        has_fixed_cb_ancestor: false,
      });
    }
    stack.push(Frame {
      node: &fragment_tree.root,
      has_fixed_cb_ancestor: false,
    });

    let mut found: Option<(&crate::tree::fragment_tree::FragmentNode, bool)> = None;
    while let Some(frame) = stack.pop() {
      if frame.node.box_id() == Some(box_id) {
        found = Some((frame.node, frame.has_fixed_cb_ancestor));
        break;
      }
      let establishes_fixed_cb = frame
        .node
        .style
        .as_deref()
        .is_some_and(|style| style.establishes_fixed_containing_block());
      let has_fixed_cb_ancestor_for_children = frame.has_fixed_cb_ancestor || establishes_fixed_cb;
      for child in frame.node.children.iter().rev() {
        stack.push(Frame {
          node: child,
          has_fixed_cb_ancestor: has_fixed_cb_ancestor_for_children,
        });
      }
    }

    let desired = Point::new(
      if desired.x.is_finite() {
        desired.x
      } else {
        0.0
      },
      if desired.y.is_finite() {
        desired.y
      } else {
        0.0
      },
    );

    let Some((node, has_fixed_cb_ancestor)) = found else {
      return Ok(desired);
    };

    let mut bounds = crate::scroll::scroll_bounds_for_fragment(
      node,
      Point::ZERO,
      node.bounds.size,
      viewport_size,
      false,
      has_fixed_cb_ancestor,
    );

    // Mirror the paint pipeline's listbox <select> approximation for scroll bounds.
    if let Some(style) = node.style.as_deref() {
      if let crate::tree::fragment_tree::FragmentContent::Replaced { replaced_type, .. } =
        &node.content
      {
        if let crate::tree::box_tree::ReplacedType::FormControl(control) = replaced_type {
          if let crate::tree::box_tree::FormControlKind::Select(select) = &control.control {
            if select.multiple || select.size > 1 {
              let row_height =
                crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport(
                  style,
                  None,
                  Some(viewport_size),
                  None,
                );
              if row_height.is_finite() && row_height > 0.0 {
                let content_height = row_height * select.items.len() as f32;
                if content_height.is_finite() {
                  let viewport_height = node.bounds.height();
                  if viewport_height.is_finite() {
                    bounds.min_y = 0.0;
                    bounds.max_y = (content_height - viewport_height).max(0.0);
                  }
                }
              }
            }
          }
        }
      }
    }

    Ok(bounds.clamp(desired))
  }
  /// Updates the viewport scroll offset (in CSS px), marking paint dirty.
  pub fn set_scroll(&mut self, scroll_x: f32, scroll_y: f32) {
    if self.options.scroll_x != scroll_x || self.options.scroll_y != scroll_y {
      self.options.scroll_delta = Point::new(
        scroll_x - self.options.scroll_x,
        scroll_y - self.options.scroll_y,
      );
      self.options.scroll_x = scroll_x;
      self.options.scroll_y = scroll_y;
      self.paint_dirty = true;
    }
  }

  /// Updates the full scroll state (viewport + element scroll offsets), marking paint dirty.
  pub fn set_scroll_state(&mut self, state: ScrollState) {
    let ScrollState {
      viewport,
      elements,
      viewport_delta,
      elements_delta,
    } = state;
    let changed = self.options.scroll_x != viewport.x
      || self.options.scroll_y != viewport.y
      || self.options.element_scroll_offsets != elements
      || self.options.scroll_delta != viewport_delta
      || self.options.element_scroll_deltas != elements_delta;
    if changed {
      self.options.scroll_x = viewport.x;
      self.options.scroll_y = viewport.y;
      self.options.element_scroll_offsets = elements;
      self.options.scroll_delta = viewport_delta;
      self.options.element_scroll_deltas = elements_delta;
      self.paint_dirty = true;
    }
  }

  /// Returns the current scroll state used by this document.
  pub fn scroll_state(&self) -> ScrollState {
    ScrollState::from_parts_with_deltas(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
      self.options.scroll_delta,
      self.options.element_scroll_deltas.clone(),
    )
  }

  /// Applies a scroll wheel delta at a point in viewport coordinates.
  ///
  /// This updates both viewport scroll and element scroll container offsets (e.g. `<select size>`
  /// listboxes) using the cached layout's fragment tree.
  pub fn wheel_scroll_at_viewport_point(
    &mut self,
    viewport_point_css: Point,
    delta_css: (f32, f32),
  ) -> Result<bool> {
    let prepared = self.prepared.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no cached layout; call render_frame() first".to_string(),
      })
    })?;

    let current_scroll_state = self.scroll_state();
    let page_point_css = viewport_point_css.translate(current_scroll_state.viewport);
    let (delta_x, delta_y) = delta_css;
    let mut next = crate::interaction::scroll_wheel::apply_wheel_scroll_at_point(
      prepared.fragment_tree(),
      &current_scroll_state,
      prepared.layout_viewport(),
      page_point_css,
      crate::interaction::scroll_wheel::ScrollWheelInput { delta_x, delta_y },
    );

    let changed_offsets =
      next.viewport != current_scroll_state.viewport || next.elements != current_scroll_state.elements;
    if changed_offsets {
      next.update_deltas_from(&current_scroll_state);
      self.set_scroll_state(next);
      Ok(true)
    } else {
      Ok(false)
    }
  }

  /// Returns the current viewport scroll offset (in CSS px).
  pub fn viewport_scroll_offset(&self) -> Point {
    Point::new(self.options.scroll_x, self.options.scroll_y)
  }

  /// Clamp a desired viewport scroll offset to the valid range for the current document layout.
  ///
  /// This ensures layout before computing the scroll bounds and is intended for JS/Web API
  /// integrations like `window.scrollTo`.
  pub fn clamp_viewport_scroll_offset(&mut self, desired: Point) -> Result<Point> {
    let prepared = self.prepared_layout()?;
    let viewport = prepared.layout_viewport();

    let desired = Point::new(
      if desired.x.is_finite() { desired.x } else { 0.0 },
      if desired.y.is_finite() { desired.y } else { 0.0 },
    );

    let bounds = crate::scroll::scroll_bounds_for_fragment(
      &prepared.fragment_tree().root,
      Point::ZERO,
      viewport,
      viewport,
      true,
      false,
    );
    Ok(bounds.clamp(desired))
  }

  fn is_root_scrolling_element(&self, node: crate::dom2::NodeId) -> bool {
    let dom = self.dom();
    dom.document_element() == Some(node) || dom.body() == Some(node)
  }

  pub fn bounding_client_rect(&mut self, node: crate::dom2::NodeId) -> Option<Rect> {
    let _ = self.ensure_layout();
    let prepared = self.prepared.as_ref()?;
    let mapping = self.last_dom_mapping.as_ref()?;
    let styled_id = mapping.preorder_for_node_id(node)?;

    let box_ids =
      crate::interaction::dom_geometry::collect_box_ids_for_styled_node(prepared.box_tree(), styled_id);
    let _principal_box_id = *box_ids.first()?;

    let rect_page = crate::interaction::dom_geometry::union_scrolled_absolute_bounds_for_box_ids(
      prepared.fragment_tree(),
      &self.scroll_state(),
      &box_ids,
    )?;

    // `union_scrolled_absolute_bounds_for_box_ids` translates viewport-fixed fragments into page
    // coordinates by applying viewport-scroll cancel semantics. Subtract viewport scroll for all
    // nodes (including viewport-fixed) to convert page → viewport coordinates.
    let viewport_scroll = self.viewport_scroll_offset();
    Some(rect_page.translate(Point::new(
      -viewport_scroll.x,
      -viewport_scroll.y,
    )))
  }

  pub fn offset_rect(&mut self, node: crate::dom2::NodeId) -> Option<Rect> {
    let _ = self.ensure_layout();
    let prepared = self.prepared.as_ref()?;
    let mapping = self.last_dom_mapping.as_ref()?;
    let styled_id = mapping.preorder_for_node_id(node)?;

    let box_ids =
      crate::interaction::dom_geometry::collect_box_ids_for_styled_node(prepared.box_tree(), styled_id);
    let principal_box_id = *box_ids.first()?;

    let rect_page = crate::interaction::dom_geometry::union_absolute_bounds_for_box_ids(
      prepared.fragment_tree(),
      &box_ids,
    )?;

    let is_fixed = crate::interaction::dom_geometry::find_first_fragment_path_for_box_id(
      prepared.fragment_tree(),
      principal_box_id,
    )
    .and_then(|(root_kind, path)| {
      crate::interaction::dom_geometry::resolve_fragment_path(
        prepared.fragment_tree(),
        root_kind,
        &path,
      )
    })
    .is_some_and(|(fragment, _origin, _has_fixed_cb_ancestor)| {
      fragment
        .style
        .as_deref()
        .is_some_and(|style| matches!(style.position, crate::style::position::Position::Fixed))
    });

    let offset_parent = if is_fixed || self.is_root_scrolling_element(node) {
      None
    } else {
      let mut candidate = None;
      let mut current = self.dom().parent_node(node);
      while let Some(parent) = current {
        if self.is_root_scrolling_element(parent) {
          break;
        }

        let Some(parent_styled_id) = mapping.preorder_for_node_id(parent) else {
          current = self.dom().parent_node(parent);
          continue;
        };
        let parent_box_ids = crate::interaction::dom_geometry::collect_box_ids_for_styled_node(
          prepared.box_tree(),
          parent_styled_id,
        );
        let Some(&parent_principal_box_id) = parent_box_ids.first() else {
          current = self.dom().parent_node(parent);
          continue;
        };

        let qualifies = find_box_node_by_id(&prepared.box_tree.root, parent_principal_box_id)
          .is_some_and(|node| node.style.establishes_abs_containing_block());
        if qualifies {
          candidate = Some(parent);
          break;
        }

        current = self.dom().parent_node(parent);
      }

      candidate.or(self.dom().body())
    };

    let sanitize_nonneg = |value: f32| if value.is_finite() { value.max(0.0) } else { 0.0 };

    let reference = if let Some(offset_parent) = offset_parent {
      match mapping.preorder_for_node_id(offset_parent) {
        Some(offset_parent_styled_id) => {
          let parent_box_ids = crate::interaction::dom_geometry::collect_box_ids_for_styled_node(
            prepared.box_tree(),
            offset_parent_styled_id,
          );
          match parent_box_ids.first() {
            Some(&parent_principal_box_id) => crate::interaction::dom_geometry::find_first_fragment_path_for_box_id(
              prepared.fragment_tree(),
              parent_principal_box_id,
            )
            .and_then(|(root_kind, path)| {
              crate::interaction::dom_geometry::resolve_fragment_path(
                prepared.fragment_tree(),
                root_kind,
                &path,
              )
            })
            .map(|(fragment, origin, _has_fixed_cb_ancestor)| {
              let (border_left, border_top) = fragment
                .style
                .as_deref()
                .map(|style| {
                  (
                    sanitize_nonneg(style.used_border_left_width().to_px()),
                    sanitize_nonneg(style.used_border_top_width().to_px()),
                  )
                })
                .unwrap_or((0.0, 0.0));
              Point::new(origin.x + border_left, origin.y + border_top)
            })
            .unwrap_or(Point::ZERO),
            None => Point::ZERO,
          }
        }
        None => Point::ZERO,
      }
    } else {
      Point::ZERO
    };

    Some(Rect::from_xywh(
      rect_page.x() - reference.x,
      rect_page.y() - reference.y,
      rect_page.width(),
      rect_page.height(),
    ))
  }

  pub fn client_size(&mut self, node: crate::dom2::NodeId) -> Option<Size> {
    let _ = self.ensure_layout();
    let prepared = self.prepared.as_ref()?;
    let fragment_tree = prepared.fragment_tree();

    let sanitize_nonneg = |value: f32| if value.is_finite() { value.max(0.0) } else { 0.0 };

    if self.is_root_scrolling_element(node) {
      let viewport = fragment_tree.viewport_size();
      let (border_left, border_right, border_top, border_bottom) = fragment_tree
        .root
        .style
        .as_deref()
        .map(|style| {
          (
            sanitize_nonneg(style.used_border_left_width().to_px()),
            sanitize_nonneg(style.used_border_right_width().to_px()),
            sanitize_nonneg(style.used_border_top_width().to_px()),
            sanitize_nonneg(style.used_border_bottom_width().to_px()),
          )
        })
        .unwrap_or((0.0, 0.0, 0.0, 0.0));
      let reservation = fragment_tree.root.scrollbar_reservation;
      let width = (sanitize_nonneg(viewport.width)
        - border_left
        - border_right
        - sanitize_nonneg(reservation.left)
        - sanitize_nonneg(reservation.right))
        .max(0.0);
      let height = (sanitize_nonneg(viewport.height)
        - border_top
        - border_bottom
        - sanitize_nonneg(reservation.top)
        - sanitize_nonneg(reservation.bottom))
        .max(0.0);
      return Some(Size::new(width, height));
    }

    let mapping = self.last_dom_mapping.as_ref()?;
    let styled_id = mapping.preorder_for_node_id(node)?;

    let box_ids =
      crate::interaction::dom_geometry::collect_box_ids_for_styled_node(prepared.box_tree(), styled_id);
    let principal_box_id = *box_ids.first()?;

    let (fragment, _origin, _has_fixed_cb_ancestor) =
      crate::interaction::dom_geometry::find_first_fragment_path_for_box_id(
        prepared.fragment_tree(),
        principal_box_id,
      )
      .and_then(|(root_kind, path)| {
        crate::interaction::dom_geometry::resolve_fragment_path(
          prepared.fragment_tree(),
          root_kind,
          &path,
        )
      })?;

    Some(crate::interaction::dom_geometry::client_size_for_fragment(fragment))
  }

  pub fn scroll_size(&mut self, node: crate::dom2::NodeId) -> Option<Size> {
    let _ = self.ensure_layout();
    let prepared = self.prepared.as_ref()?;
    let fragment_tree = prepared.fragment_tree();

    let sanitize_nonneg = |value: f32| if value.is_finite() { value.max(0.0) } else { 0.0 };

    if self.is_root_scrolling_element(node) {
      let viewport = fragment_tree.viewport_size();
      let client = {
        let (border_left, border_right, border_top, border_bottom) = fragment_tree
          .root
          .style
          .as_deref()
          .map(|style| {
            (
              sanitize_nonneg(style.used_border_left_width().to_px()),
              sanitize_nonneg(style.used_border_right_width().to_px()),
              sanitize_nonneg(style.used_border_top_width().to_px()),
              sanitize_nonneg(style.used_border_bottom_width().to_px()),
            )
          })
          .unwrap_or((0.0, 0.0, 0.0, 0.0));
        let reservation = fragment_tree.root.scrollbar_reservation;
        let width = (viewport.width
          - border_left
          - border_right
          - sanitize_nonneg(reservation.left)
          - sanitize_nonneg(reservation.right))
          .max(0.0);
        let height = (viewport.height
          - border_top
          - border_bottom
          - sanitize_nonneg(reservation.top)
          - sanitize_nonneg(reservation.bottom))
          .max(0.0);
        Size::new(width, height)
      };

      let bounds = crate::scroll::scroll_bounds_for_fragment(
        &fragment_tree.root,
        Point::new(fragment_tree.root.bounds.x(), fragment_tree.root.bounds.y()),
        viewport,
        viewport,
        true,
        false,
      );
      return Some(Size::new(
        sanitize_nonneg(client.width + sanitize_nonneg(bounds.max_x)),
        sanitize_nonneg(client.height + sanitize_nonneg(bounds.max_y)),
      ));
    }

    let mapping = self.last_dom_mapping.as_ref()?;
    let styled_id = mapping.preorder_for_node_id(node)?;
    let box_ids =
      crate::interaction::dom_geometry::collect_box_ids_for_styled_node(prepared.box_tree(), styled_id);
    let principal_box_id = *box_ids.first()?;

    let (fragment, origin, has_fixed_cb_ancestor) =
      crate::interaction::dom_geometry::find_first_fragment_path_for_box_id(
        fragment_tree,
        principal_box_id,
      )
      .and_then(|(root_kind, path)| {
        crate::interaction::dom_geometry::resolve_fragment_path(fragment_tree, root_kind, &path)
      })?;

    let client = crate::interaction::dom_geometry::client_size_for_fragment(fragment);
    let viewport_for_units = fragment_tree.viewport_size();
    let bounds = crate::scroll::scroll_bounds_for_fragment(
      fragment,
      origin,
      fragment.bounds.size,
      viewport_for_units,
      false,
      has_fixed_cb_ancestor,
    );

    Some(Size::new(
      sanitize_nonneg(client.width + sanitize_nonneg(bounds.max_x)),
      sanitize_nonneg(client.height + sanitize_nonneg(bounds.max_y)),
    ))
  }

  pub fn scroll_offset(&mut self, node: crate::dom2::NodeId) -> Point {
    if self.is_root_scrolling_element(node) {
      return Point::new(self.options.scroll_x, self.options.scroll_y);
    }

    let _ = self.ensure_layout();
    let Some(prepared) = self.prepared.as_ref() else {
      return Point::ZERO;
    };
    let Some(mapping) = self.last_dom_mapping.as_ref() else {
      return Point::ZERO;
    };
    let Some(styled_id) = mapping.preorder_for_node_id(node) else {
      return Point::ZERO;
    };

    let box_ids = crate::interaction::dom_geometry::collect_box_ids_for_styled_node(prepared.box_tree(), styled_id);
    let Some(&principal_box_id) = box_ids.first() else {
      return Point::ZERO;
    };
    let scroll = self
      .options
      .element_scroll_offsets
      .get(&principal_box_id)
      .copied()
      .unwrap_or(Point::ZERO);
    Point::new(
      if scroll.x.is_finite() { scroll.x } else { 0.0 },
      if scroll.y.is_finite() { scroll.y } else { 0.0 },
    )
  }

  pub fn set_scroll_offset(&mut self, node: crate::dom2::NodeId, offset: Point) -> Result<()> {
    let offset = Point::new(
      if offset.x.is_finite() { offset.x } else { 0.0 },
      if offset.y.is_finite() { offset.y } else { 0.0 },
    );

    let _ = self.ensure_layout();
    let sanitize_nonneg = |value: f32| if value.is_finite() { value.max(0.0) } else { 0.0 };
    let desired = Point::new(sanitize_nonneg(offset.x), sanitize_nonneg(offset.y));

    if self.is_root_scrolling_element(node) {
      let current = Point::new(self.options.scroll_x, self.options.scroll_y);
      let clamped = if let Some(prepared) = self.prepared.as_ref() {
        let tree = prepared.fragment_tree();
        let viewport = tree.viewport_size();
        let bounds = crate::scroll::scroll_bounds_for_fragment(
          &tree.root,
          Point::new(tree.root.bounds.x(), tree.root.bounds.y()),
          viewport,
          viewport,
          true,
          false,
        );
        bounds.clamp(desired)
      } else {
        desired
      };

      if clamped != current {
        self.options.scroll_delta = Point::new(clamped.x - current.x, clamped.y - current.y);
        self.options.scroll_x = clamped.x;
        self.options.scroll_y = clamped.y;
        self.paint_dirty = true;
      }
      return Ok(());
    }

    let Some(prepared) = self.prepared.as_ref() else {
      return Ok(());
    };
    let Some(mapping) = self.last_dom_mapping.as_ref() else {
      return Ok(());
    };
    let Some(styled_id) = mapping.preorder_for_node_id(node) else {
      return Ok(());
    };

    let box_ids = crate::interaction::dom_geometry::collect_box_ids_for_styled_node(prepared.box_tree(), styled_id);
    let Some(&principal_box_id) = box_ids.first() else {
      return Ok(());
    };

    let viewport_for_units = prepared.fragment_tree().viewport_size();

    let clamped = crate::interaction::dom_geometry::find_first_fragment_path_for_box_id(
      prepared.fragment_tree(),
      principal_box_id,
    )
    .and_then(|(root_kind, path)| {
      crate::interaction::dom_geometry::resolve_fragment_path(
        prepared.fragment_tree(),
        root_kind,
        &path,
      )
    })
    .map(|(fragment, origin, has_fixed_cb_ancestor)| {
      let bounds = crate::scroll::scroll_bounds_for_fragment(
        fragment,
        origin,
        fragment.bounds.size,
        viewport_for_units,
        false,
        has_fixed_cb_ancestor,
      );
      bounds.clamp(desired)
    })
    .unwrap_or(Point::ZERO);

    let prev = self
      .options
      .element_scroll_offsets
      .get(&principal_box_id)
      .copied()
      .unwrap_or(Point::ZERO);
    if clamped != prev {
      if clamped == Point::ZERO {
        self.options.element_scroll_offsets.remove(&principal_box_id);
      } else {
        self.options.element_scroll_offsets.insert(principal_box_id, clamped);
      }

      let delta = Point::new(clamped.x - prev.x, clamped.y - prev.y);
      if delta == Point::ZERO {
        self.options.element_scroll_deltas.remove(&principal_box_id);
      } else {
        self
          .options
          .element_scroll_deltas
          .insert(principal_box_id, delta);
      }

      self.paint_dirty = true;
    }

    Ok(())
  }

  /// Updates the animation/transition sampling timestamp in milliseconds since document load.
  ///
  /// When the value changes, this marks paint dirty (but does not invalidate style/layout).
  pub fn set_animation_time(&mut self, time_ms: Option<f32>) {
    let sanitized = super::sanitize_animation_time_ms(time_ms);
    if sanitized != self.options.animation_time {
      self.options.animation_time = sanitized;
      self.paint_dirty = true;
    }
  }

  /// Convenience wrapper for [`BrowserDocumentDom2::set_animation_time`] with a concrete timestamp.
  pub fn set_animation_time_ms(&mut self, time_ms: f32) {
    self.set_animation_time(Some(time_ms));
  }

  /// Returns the mapping produced for the most recently prepared renderer DOM snapshot, if
  /// available.
  pub fn last_dom_mapping(&self) -> Option<&crate::dom2::RendererDomMapping> {
    self.last_dom_mapping.as_ref()
  }

  /// Returns the cached prepared document, if available.
  ///
  /// The returned [`PreparedDocument`] contains a renderer DOM snapshot, computed styles, box tree,
  /// and fragment tree. It is updated when this document is rendered (see [`Self::render_frame`]).
  pub fn prepared(&self) -> Option<&PreparedDocument> {
    self.prepared.as_ref()
  }

  /// Returns a mutable reference to the cached prepared document, if available.
  pub fn prepared_mut(&mut self) -> Option<&mut PreparedDocument> {
    self.prepared.as_mut()
  }

  /// Returns the renderer computed style for a `dom2` node using the latest prepared layout.
  ///
  /// This is intended for DOM query APIs like `getComputedStyle()`. It prefers the style attached
  /// to the node's **principal box** (so used-value adjustments like blockification are observed).
  /// If the node does not generate a principal box (e.g. `display: contents`), it falls back to
  /// scanning the styled tree.
  pub fn computed_style_for_dom_node(
    &self,
    node_id: crate::dom2::NodeId,
  ) -> Option<Arc<ComputedStyle>> {
    let prepared = self.prepared.as_ref()?;
    let mapping = self.last_dom_mapping.as_ref()?;
    let preorder = mapping.preorder_for_node_id(node_id)?;

    principal_box_style_for_styled_node_id(&prepared.box_tree.root, preorder)
      .or_else(|| styled_tree_style_for_preorder_id(&prepared.styled_tree, preorder))
  }

  /// Builds a scroll- and sticky-aware geometry context for the most recently prepared layout.
  ///
  /// Callers can use the returned [`super::Dom2GeometryContext`] to compute border/padding/content
  /// boxes and scrollport (client) rects in viewport coordinates for `dom2` nodes.
  pub fn geometry_context(&self) -> Result<super::Dom2GeometryContext<'_>> {
    let prepared = self.prepared.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no cached layout; call render_frame() first".to_string(),
      })
    })?;
    let mapping = self.last_dom_mapping.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no DOM mapping; call render_frame() first".to_string(),
      })
    })?;

    let scroll_state = ScrollState::from_parts_with_deltas(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
      self.options.scroll_delta,
      self.options.element_scroll_deltas.clone(),
    );

    Ok(super::Dom2GeometryContext::new(
      &self.renderer,
      prepared,
      mapping,
      scroll_state,
    ))
  }

  /// Returns counters describing how invalidations have been satisfied over this document's
  /// lifetime.
  pub fn invalidation_counters(&self) -> BrowserDocumentDom2InvalidationCounters {
    self.invalidation_counters
  }

  /// Translate a renderer/cascade 1-based preorder id (see `crate::dom::enumerate_dom_ids`) back to
  /// a stable `dom2` node id.
  pub fn dom2_node_for_renderer_preorder(&self, preorder_id: usize) -> Option<crate::dom2::NodeId> {
    self.last_dom_mapping()?.node_id_for_preorder(preorder_id)
  }

  /// Translate a hit-test result back to a stable `dom2` node id.
  pub fn dom2_node_for_hit_test(
    &self,
    hit: &crate::interaction::HitTestResult,
  ) -> Option<crate::dom2::NodeId> {
    self.dom2_node_for_renderer_preorder(hit.dom_node_id)
  }

  /// Perform a viewport-coordinate hit test and return the hit element's stable `dom2::NodeId`.
  ///
  /// This mirrors `Document.elementFromPoint()` semantics at the host layer:
  /// - `x`/`y` are viewport-relative CSS px coordinates (before applying scroll offsets).
  /// - The returned id is stable across renderer snapshots (unlike renderer preorder ids).
  /// - The returned node is guaranteed to be an element (walking up the ancestor chain if needed).
  ///
  /// This ensures style/layout are up to date but does **not** require a paint.
  pub fn element_from_point(
    &mut self,
    x: f32,
    y: f32,
  ) -> Result<Option<crate::dom2::NodeId>> {
    let Some(mut node_id) = self
      .hit_test_viewport_point(x, y)?
      .map(|result| result.node)
    else {
      return Ok(None);
    };

    // The hit-test layer should already resolve semantic targets to elements, but keep this
    // defensive so callers (JS) never accidentally receive Text/comment nodes.
    loop {
      match &self.dom().node(node_id).kind {
        crate::dom2::NodeKind::Element { .. } => return Ok(Some(node_id)),
        _ => match self.dom().parent_node(node_id) {
          Some(parent) => node_id = parent,
          None => return Ok(None),
        },
      }
    }
  }

  /// Perform a viewport-coordinate hit test and return both hit metadata and a stable `dom2` node id.
  ///
  /// - `x`/`y` are viewport-relative CSS px coordinates (before applying scroll offsets).
  /// - The returned [`Dom2HitTestResult::node`] is stable across renderer snapshots (unlike renderer
  ///   preorder ids) and corresponds to [`Dom2HitTestResult::hit::dom_node_id`].
  /// - The returned node is guaranteed to be an element (walking up the ancestor chain if needed).
  ///
  /// This ensures style/layout are up to date but does **not** require a paint.
  pub fn hit_test_viewport_point(&mut self, x: f32, y: f32) -> Result<Option<Dom2HitTestResult>> {
    if !x.is_finite() || !y.is_finite() {
      return Ok(None);
    }
 
    self.ensure_layout_for_hit_testing()?;
    let Some(prepared) = self.prepared.as_ref() else {
      return Ok(None);
    };
 
    let viewport = prepared.fragment_tree().viewport_size();
    if x < 0.0 || y < 0.0 || x >= viewport.width || y >= viewport.height {
      return Ok(None);
    }
 
    let scroll_state = ScrollState::from_parts_with_deltas(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
      self.options.scroll_delta,
      self.options.element_scroll_deltas.clone(),
    );
 
    let Some(hit) = crate::interaction::hit_testing::hit_test_dom_viewport_point(
      prepared,
      &scroll_state,
      Point::new(x, y),
    ) else {
      return Ok(None);
    };
 
    let Some(mut node) = self.dom2_node_for_hit_test(&hit) else {
      return Ok(None);
    };

    // The hit-test layer should already resolve semantic targets to elements, but keep this
    // defensive so callers (UI) never accidentally receive Text/comment nodes.
    loop {
      match &self.dom().node(node).kind {
        crate::dom2::NodeKind::Element { .. } => break,
        _ => match self.dom().parent_node(node) {
          Some(parent) => node = parent,
          None => return Ok(None),
        },
      }
    }

    Ok(Some(Dom2HitTestResult { node, hit }))
  }

  /// Like [`BrowserDocumentDom2::hit_test_viewport_point`], but returns all hits (topmost first).
  ///
  /// This mirrors [`BrowserDocumentDom2::elements_from_point`] but includes hit-test metadata.
  pub fn hit_test_viewport_point_all(&mut self, x: f32, y: f32) -> Result<Vec<Dom2HitTestResult>> {
    if !x.is_finite() || !y.is_finite() {
      return Ok(Vec::new());
    }

    self.ensure_layout_for_hit_testing()?;
    let Some(prepared) = self.prepared.as_ref() else {
      return Ok(Vec::new());
    };

    let viewport = prepared.fragment_tree().viewport_size();
    if x < 0.0 || y < 0.0 || x >= viewport.width || y >= viewport.height {
      return Ok(Vec::new());
    }

    let scroll_state = ScrollState::from_parts_with_deltas(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
      self.options.scroll_delta,
      self.options.element_scroll_deltas.clone(),
    );

    let hits = crate::interaction::hit_testing::hit_test_dom_viewport_point_all(
      prepared,
      &scroll_state,
      Point::new(x, y),
    );

    let mut out: Vec<Dom2HitTestResult> = Vec::new();
    for hit in hits {
      let Some(mut node) = self.dom2_node_for_hit_test(&hit) else {
        continue;
      };

      loop {
        match &self.dom().node(node).kind {
          crate::dom2::NodeKind::Element { .. } => break,
          _ => match self.dom().parent_node(node) {
            Some(parent) => node = parent,
            None => break,
          },
        }
      }

      if matches!(&self.dom().node(node).kind, crate::dom2::NodeKind::Element { .. }) {
        out.push(Dom2HitTestResult { node, hit });
      }
    }

    Ok(out)
  }

  /// Like [`BrowserDocumentDom2::element_from_point`], but returns all hit elements (topmost first).
  ///
  /// This backs `Document.elementsFromPoint()` when needed.
  pub fn elements_from_point(
    &mut self,
    x: f32,
    y: f32,
  ) -> Result<Vec<crate::dom2::NodeId>> {
    let hits = self.hit_test_viewport_point_all(x, y)?;
    let mut out: Vec<crate::dom2::NodeId> = Vec::new();
    for hit in hits {
      let mut node_id = hit.node;

      loop {
        match &self.dom().node(node_id).kind {
          crate::dom2::NodeKind::Element { .. } => break,
          _ => match self.dom().parent_node(node_id) {
            Some(parent) => node_id = parent,
            None => break,
          },
        }
      }

      if matches!(&self.dom().node(node_id).kind, crate::dom2::NodeKind::Element { .. }) {
        // `hit_test_dom_*` already de-dupes within the renderer preorder space, but ensure we don't
        // return duplicates after walking to element ancestors.
        if !out.contains(&node_id) {
          out.push(node_id);
        }
      }
    }

    Ok(out)
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame.
  ///
  /// Returns `Ok(None)` when no dirty flags are set.
  pub fn render_if_needed(&mut self) -> Result<Option<super::Pixmap>> {
    Ok(
      self
        .render_if_needed_with_deadlines(None)?
        .map(|frame| frame.pixmap),
    )
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame,
  /// applying an optional deadline to the *paint* phase.
  ///
  /// This mirrors [`super::BrowserDocument::render_if_needed_with_deadlines`] but operates on the
  /// `dom2`-backed document.
  pub fn render_if_needed_with_deadlines(
    &mut self,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<Option<super::PaintedFrame>> {
    let interaction_css_hash =
      interaction_state_css_fingerprint(self.interaction_state.as_ref());
    let interaction_paint_hash =
      interaction_state_paint_fingerprint(self.interaction_state.as_ref());
    if !self.is_dirty()
      && self.prepared.is_some()
      && interaction_css_hash == self.interaction_css_hash
      && interaction_paint_hash == self.interaction_paint_hash
      && !self.needs_animation_frame()
    {
      return Ok(None);
    }
    let frame = self.render_frame_with_deadlines(paint_deadline)?;
    Ok(Some(frame))
  }

  /// Renders one frame.
  ///
  /// If the document is dirty, this triggers a full pipeline run. Otherwise, it repaints from
  /// cached layout artifacts.
  pub fn render_frame(&mut self) -> Result<super::Pixmap> {
    Ok(self.render_frame_with_deadlines(None)?.pixmap)
  }

  /// Renders one frame, applying an optional deadline to the *paint* phase.
  ///
  /// When layout is required, prepare/layout is executed using the currently configured
  /// `RenderOptions::{timeout,cancel_callback}`, then painting proceeds under `paint_deadline`.
  pub fn render_frame_with_deadlines(
    &mut self,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::PaintedFrame> {
    let interaction_css_hash =
      interaction_state_css_fingerprint(self.interaction_state.as_ref());
    let interaction_paint_hash =
      interaction_state_paint_fingerprint(self.interaction_state.as_ref());
    if interaction_css_hash != self.interaction_css_hash {
      // CSS-affecting interaction state affects selector matching / cascade, so we must re-run
      // style/layout when it changes.
      self.invalidate_all();
    } else if interaction_paint_hash != self.interaction_paint_hash {
      // Paint-only interaction state can be applied during cached paint.
      self.paint_dirty = true;
    }

    // Rendering requires up-to-date layout caches. Reuse the same host-side layout flush used by
    // DOM/layout query APIs (e.g. `getBoundingClientRect`) so JS-driven `scrollTo` can clamp against
    // current scroll bounds without forcing a paint.
    self.flush_layout(BrowserDocumentDom2LayoutFlushRequest::FOR_RENDER_OR_DOM_QUERIES)?;

    let frame = self.paint_from_cache_frame_with_deadline(paint_deadline)?;

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }

    Ok(frame)
  }

  /// Ensure this document has up-to-date style + layout artifacts for DOM geometry queries.
  ///
  /// This mirrors the style/layout portion of [`BrowserDocumentDom2::render_frame_with_deadlines`],
  /// but intentionally does **not** paint.
  ///
  /// Callers can use this to satisfy CSSOM View properties (e.g. `Element.clientTop/clientLeft`)
  /// that need computed border widths while avoiding the cost/side-effects of a paint.
  pub(crate) fn ensure_layout_for_dom_query(&mut self) -> Result<()> {
    self.flush_layout(BrowserDocumentDom2LayoutFlushRequest::FOR_RENDER_OR_DOM_QUERIES)?;
    Ok(())
  }

  /// Compute CSSOM View `clientTop`/`clientLeft` values for a DOM element.
  ///
  /// These values are defined as the computed border widths (plus any scrollbars between padding
  /// and border edges). Scrollbar contributions are currently approximated as 0.
  ///
  /// The return values follow WebIDL `long` semantics (clamped/truncated to i32).
  pub(crate) fn element_client_border_widths(&mut self, node_id: crate::dom2::NodeId) -> (i32, i32) {
    // Ensure style/layout is fresh before reading computed style.
    if self.ensure_layout_for_dom_query().is_err() {
      return (0, 0);
    }

    let Some(prepared) = self.prepared.as_ref() else {
      return (0, 0);
    };
    let Some(mapping) = self.last_dom_mapping.as_ref() else {
      return (0, 0);
    };

    let Some(styled_node_id) = mapping.preorder_for_node_id(node_id) else {
      // Detached nodes have no associated CSS box.
      return (0, 0);
    };

    fn find_principal_box<'a>(root: &'a BoxNode, styled_node_id: usize) -> Option<&'a BoxNode> {
      let mut stack: Vec<&BoxNode> = vec![root];
      while let Some(node) = stack.pop() {
        if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
          return Some(node);
        }
        if let Some(body) = node.footnote_body.as_deref() {
          stack.push(body);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      None
    }

    let Some(principal_box) = find_principal_box(&prepared.box_tree.root, styled_node_id) else {
      // The element is present in the DOM snapshot but did not generate a layout box (e.g. inert
      // template contents, `display: contents`, etc).
      return (0, 0);
    };

    // CSSOM View: return 0 for inline boxes. Treat *non-atomic* inline boxes as inline; atomic
    // inline-level boxes (inline-block/flex/grid/etc) should still expose border widths.
    let is_inline = matches!(
      &principal_box.box_type,
      BoxType::Inline(inline) if inline.formatting_context.is_none()
    );
    if is_inline {
      return (0, 0);
    }

    let top = principal_box.style.used_border_top_width().to_px();
    let left = principal_box.style.used_border_left_width().to_px();
    (top as i32, left as i32)
  }

  /// Paints the most recently laid-out document without re-running style/layout.
  ///
  /// This mirrors [`super::BrowserDocument::paint_from_cache_frame_with_deadline`] but operates on
  /// the `dom2`-backed document. Callers should check [`BrowserDocumentDom2::needs_layout`] first
  /// and fall back to [`BrowserDocumentDom2::render_frame`] when layout is required.
  pub fn paint_from_cache_frame_with_deadline(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::PaintedFrame> {
    let animation_time = self.animation_time_for_paint();
    if self.prepared.is_none() {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no cached layout; call render_frame() first".to_string(),
      }));
    }

    // Ensure the cached fragment tree's selection metadata reflects the latest stable selection
    // state before painting.
    self.apply_document_selection_overlay();
    let prepared = self
      .prepared
      .as_ref()
      .expect("checked prepared is Some above"); // fastrender-allow-unwrap

    // Prefer an explicitly provided deadline; otherwise fall back to the currently installed
    // deadline (if any) or this document's configured `RenderOptions::{timeout,cancel_callback}`.
    //
    // If an embedding already installed a deadline (e.g. to share a single `RenderDeadline` across
    // JS + render), avoid installing a fresh deadline here. A fresh deadline would reset the start
    // time and effectively grant extra time for repaint.
    let _deadline_guard = if let Some(deadline) = deadline {
      Some(crate::render_control::DeadlineGuard::install(Some(
        deadline,
      )))
    } else if crate::render_control::active_deadline().is_some() {
      None
    } else {
      let deadline_enabled =
        self.options.timeout.is_some() || self.options.cancel_callback.is_some();
      deadline_enabled.then(|| {
        let options_deadline = crate::render_control::RenderDeadline::new(
          self.options.timeout,
          self.options.cancel_callback.clone(),
        );
        crate::render_control::DeadlineGuard::install(Some(&options_deadline))
      })
    };
    // Perform an early cancellation check so callers can deterministically abort repaints without
    // relying on deep paint loops to periodically poll deadlines.
    crate::render_control::check_active(RenderStage::Paint).map_err(Error::Render)?;

    let scroll_state = ScrollState::from_parts_with_deltas(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
      self.options.scroll_delta,
      self.options.element_scroll_deltas.clone(),
    );

    // Clone and patch the fragment tree so paint-only interaction overlays do not mutate the cached
    // `PreparedDocument` layout artifacts.
    let mut fragment_tree = prepared.fragment_tree.clone();
    crate::interaction::paint_overlays::apply_form_control_paint_overlays_to_fragment_tree(
      prepared.box_tree(),
      &mut fragment_tree,
      self.interaction_state.as_ref(),
    );

    let frame =
      prepared.paint_with_options_frame_with_animation_state_store_and_fragment_tree(
        fragment_tree,
        PreparedPaintOptions {
          scroll: Some(scroll_state),
          viewport: None,
          background: None,
          animation_time,
          media_provider: self.media_provider.clone(),
        },
        &mut self.animation_state_store,
      )?;

    // Keep our internal scroll model synchronized with any adjustments made during painting (e.g.
    // scroll snap/clamp). This must not mark the document dirty because the frame we just painted
    // already reflects this state.
    self.options.scroll_x = frame.scroll_state.viewport.x;
    self.options.scroll_y = frame.scroll_state.viewport.y;
    self.options.element_scroll_offsets = frame.scroll_state.elements.clone();
    self.options.scroll_delta = frame.scroll_state.viewport_delta;
    self.options.element_scroll_deltas = frame.scroll_state.elements_delta.clone();

    // A successful paint always satisfies any outstanding paint invalidation, but must not clear
    // pending style/layout dirtiness.
    self.paint_dirty = false;
    // Commit the interaction paint hash after a successful paint so callers can update interaction
    // state and use cached paints without forcing repeated renders.
    self.interaction_paint_hash =
      interaction_state_paint_fingerprint(self.interaction_state.as_ref());
    if self.realtime_animations_enabled && self.options.animation_time.is_none() {
      self.last_painted_animation_clock = Some(self.animation_clock.now());
    } else {
      self.last_painted_animation_clock = None;
    }
    self.last_painted_animation_time = animation_time;
    Ok(frame)
  }

  fn apply_document_selection_overlay(&mut self) {
    let Some(prepared) = self.prepared.as_mut() else {
      return;
    };
    let Some(mapping) = self.last_dom_mapping.as_ref() else {
      return;
    };

    // Prune detached selection endpoints so callers do not keep references to removed subtrees.
    if let Some(selection) = &mut self.document_selection_dom2 {
      if !selection.prune_detached(mapping) {
        self.document_selection_dom2 = None;
      }
    }

    // We may need to project the dom2 selection into renderer-preorder space, so keep the owned
    // projected selection alive for the duration of this call and pass a reference downstream.
    let projected_dom2 = self
      .document_selection_dom2
      .as_ref()
      .map(|selection| selection.project_to_preorder(mapping));
    let selection_preorder = projected_dom2.as_ref().or_else(|| {
      self
        .interaction_state
        .as_ref()
        .and_then(|state| state.document_selection.as_ref())
    });

    crate::interaction::document_selection::apply_document_selection_to_fragment_tree_with_index(
      &mut prepared.fragment_tree,
      prepared.document_selection_index.as_ref(),
      selection_preorder,
    );
  }

  pub fn paint_from_cache_with_deadline(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::Pixmap> {
    Ok(self.paint_from_cache_frame_with_deadline(deadline)?.pixmap)
  }

  fn animation_time_for_paint(&mut self) -> Option<f32> {
    if self.options.animation_time.is_some() {
      return self.options.animation_time;
    }

    if !self.realtime_animations_enabled {
      return None;
    }

    let now = self.animation_clock.now();
    let Some(origin) = self.animation_timeline_origin else {
      self.animation_timeline_origin = Some(now);
      return Some(0.0);
    };
    let elapsed = now.checked_sub(origin).unwrap_or(Duration::ZERO);
    let time_ms = elapsed.as_secs_f64() * 1000.0;
    Some((time_ms.min(f32::MAX as f64)) as f32)
  }

  fn prepare_dom_with_options(
    &mut self,
    prev_prepared: Option<&PreparedDocument>,
    prev_mapping: Option<&crate::dom2::RendererDomMapping>,
  ) -> Result<(PreparedDocument, bool)> {
    let options = self.options.clone();
    let dom_generation = self.dom.mutation_generation();
    let snapshot = self.dom.as_ref().to_renderer_dom_with_mapping();
    let mut renderer_dom = snapshot.dom;
    let mapping = snapshot.mapping;
    self
      .dom
      .as_ref()
      .project_form_control_state_into_renderer_dom_snapshot(&mut renderer_dom, &mapping);
    let renderer_dom_ref = &renderer_dom;
    let interaction_state = self.interaction_state.as_ref();

    let toggles = self.renderer.resolve_runtime_toggles(&options);
    let incremental_restyle_enabled =
      toggles.truthy_with_default("FASTR_DOM2_INCREMENTAL_RESTYLE_REUSE", true);

    // `:has()` selectors introduce reverse dependencies (ancestors and previous siblings) that our
    // current dom2 dirty sets do not model. Until we have selector dependency tracking, disable
    // incremental restyle reuse whenever the author stylesheet contains `:has()`.
    // `reuse_map` uses renderer preorder ids as stable keys. Those ids are derived from a DOM
    // preorder traversal and are only stable across snapshots when the preorder → dom2-node mapping
    // is identical, including entries for synthetic nodes (e.g. the implicit ZWSP child for
    // `<wbr>`). When the mapping differs we must disable reuse to avoid reusing a previous
    // `StyledNode` subtree for the wrong DOM nodes.
    let can_attempt_incremental_restyle = incremental_restyle_enabled
      && !self.author_stylesheet_has_has_selectors
      && prev_prepared.is_some()
      && prev_mapping.is_some()
      // Incremental restyle reuse is only meaningful when we have at least one style-affecting
      // mutation classified (attribute changes or form-state changes that affect selectors like
      // `:checked`).
      && (!self.dirty_style_nodes.is_empty() || !self.dirty_form_state_style_nodes.is_empty())
      // Only attempt reuse when dirty nodes are elements/slots. (Text nodes inside `<style>` are
      // marked as dirty_style_nodes; those require a full restyle because the stylesheet changes.)
      && self
        .dirty_style_nodes
        .iter()
        .chain(self.dirty_form_state_style_nodes.iter())
        .all(|&node_id| {
          matches!(
            self.dom.node(node_id).kind,
            crate::dom2::NodeKind::Element { .. } | crate::dom2::NodeKind::Slot { .. }
          )
        })
      // Reuse-based invalidation is currently conservative and does not support container query
      // fixpoint iteration or `:has()` reverse dependencies.
      && prev_prepared.is_some_and(|prepared| !prepared.stylesheet().has_container_rules())
      && prev_mapping.is_some_and(|prev| prev.preorder_to_node_id() == mapping.preorder_to_node_id());

    // Build optional reuse inputs for the cascade pass.
    let cascade_reuse = if can_attempt_incremental_restyle
      && (!self.dirty_style_nodes.is_empty() || !self.dirty_form_state_style_nodes.is_empty())
    {
      let dom = self.dom.as_ref();
      let mut dirty_style_nodes: FxHashSet<crate::dom2::NodeId> = FxHashSet::default();
      dirty_style_nodes.extend(self.dirty_style_nodes.iter().copied());
      dirty_style_nodes.extend(self.dirty_form_state_style_nodes.iter().copied());

      build_incremental_restyle_scope(renderer_dom_ref, &mapping, dom, &dirty_style_nodes).map(
        |restyle_scope| {
          fn collect_reuse_map(
            node: &StyledNode,
            out: &mut std::collections::HashMap<usize, *const StyledNode>,
          ) {
            out.insert(node.node_id, node as *const _);
            for child in node.children.iter() {
              collect_reuse_map(child, out);
            }
          }

          let mut reuse_map: std::collections::HashMap<usize, *const StyledNode> =
            std::collections::HashMap::new();
          if let Some(prev) = prev_prepared {
            collect_reuse_map(prev.styled_tree(), &mut reuse_map);
          }

          super::CascadeReuse {
            restyle_scope,
            reuse_map,
          }
        },
      )
    } else {
      None
    };
    let did_incremental_restyle = cascade_reuse.is_some();

    let prepared = {
      let renderer = &mut self.renderer;

      let _toggles_guard =
        super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
      crate::debug::runtime::with_runtime_toggles(toggles, || {
        let trace = super::TraceSession::from_options(Some(&options));
        let trace_handle = trace.handle();
        let _root_span = trace_handle.span("browser_document_dom2_prepare", "pipeline");

        let shared_diagnostics =
          renderer
            .diagnostics
            .as_ref()
            .map(|diag| super::SharedRenderDiagnostics {
              inner: std::sync::Arc::clone(diag),
            });
        let context = Some(renderer.build_resource_context(
          renderer.document_url_hint(),
          shared_diagnostics,
          ReferrerPolicy::default(),
        ));
        let (prev_self, prev_image, prev_layout_image, prev_font) =
          renderer.push_resource_context(context);
        let result = prepare_dom_inner(
          renderer,
          renderer_dom_ref,
          options.clone(),
          trace_handle,
          interaction_state,
          cascade_reuse,
        );
        renderer.pop_resource_context(prev_self, prev_image, prev_layout_image, prev_font);
        drop(_root_span);
        trace.finalize(result)
      })?
    };

    self.last_dom_mapping = Some(mapping);
    // The cached layout artifacts produced by `prepare_dom_inner` correspond to the DOM snapshot we
    // just took. Update the "seen" generation so future calls can detect out-of-band DOM mutations
    // (e.g. via JS shims using raw pointers) without forcing a re-layout when only paint is
    // outstanding.
    self.last_seen_dom_mutation_generation = dom_generation;
    // `prepare_dom_with_options` runs a full pipeline against a fresh DOM snapshot, so any
    // accumulated mutation log entries (for example, from callers that mutated the DOM via
    // `dom_mut()` and bypassed `mutate_dom`) are now stale. Leaving them around would cause the next
    // `mutate_dom` call to see the old entries in `take_mutations()` and potentially force an
    // unnecessary full restyle.
    self.dom.clear_mutations();
    self.author_stylesheet_has_has_selectors = prepared.stylesheet().contains_has_selectors();
    Ok((prepared, did_incremental_restyle))
  }

  /// Ensure `self.prepared` + `self.last_dom_mapping` reflect the current live DOM.
  ///
  /// This is a "layout flush" that runs at most cascade+layout. It intentionally does **not** paint.
  fn ensure_layout_for_hit_testing(&mut self) -> Result<()> {
    self.flush_layout(BrowserDocumentDom2LayoutFlushRequest::FOR_HIT_TESTING)?;
    Ok(())
  }

  #[inline]
  fn invalidate_all(&mut self) {
    self.style_dirty = true;
    self.layout_dirty = true;
    self.paint_dirty = true;
    self.dirty_style_nodes.clear();
    self.dirty_form_state_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
    self.form_state_dirty = false;
  }

  #[inline]
  fn clear_dirty(&mut self) {
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = false;
    self.dirty_style_nodes.clear();
    self.dirty_form_state_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
    self.form_state_dirty = false;
  }

  #[inline]
  pub fn is_dirty(&self) -> bool {
    self.style_dirty
      || self.layout_dirty
      || self.paint_dirty
      || self.dom.mutation_generation() != self.last_seen_dom_mutation_generation
  }

  /// Consume any pending structured DOM mutations recorded by `dom2` when we notice the live
  /// document's mutation generation has advanced.
  ///
  /// Many embedding layers (notably some VM-JS/WebIDL native handlers) can mutate the live
  /// `dom2::Document` through raw pointers. In those cases `dom2` still records a `MutationLog`, but
  /// unless we drain it the host layer will only observe a `mutation_generation` mismatch and may
  /// fall back to a full pipeline run (missing incremental relayout opportunities).
  fn sync_dirty_from_pending_dom_mutations(&mut self) {
    if self.dom.mutation_generation() == self.last_seen_dom_mutation_generation {
      return;
    }

    let mutations = self.dom.take_mutations();
    if mutations.is_empty() {
      // We observed a generation mismatch but have no structured mutation log. This can happen for
      // truly out-of-band edits (e.g. `Document::node_mut`) *or* when mutations were already drained
      // by `mutate_dom`/`DomHost::mutate_dom` and the generation mismatch is already reflected in our
      // existing dirty flags.
      //
      // Only fall back to a full invalidation when we have no other dirtiness recorded; otherwise
      // we'd accidentally downgrade incremental invalidation state already captured by
      // `apply_mutation_log`.
      let already_dirty = self.style_dirty
        || self.layout_dirty
        || self.form_state_dirty
        || !self.dirty_style_nodes.is_empty()
        || !self.dirty_form_state_style_nodes.is_empty()
        || !self.dirty_text_nodes.is_empty()
        || !self.dirty_structure_nodes.is_empty();
      if !already_dirty {
        self.invalidate_all();
      }
      return;
    }

    let render_affecting = self.apply_mutation_log(mutations);
    if !render_affecting {
      // `dom2::Document` bumps `mutation_generation` for all mutations, including changes in
      // disconnected/inert subtrees. If none of the drained mutations can affect rendering, record
      // that we've now "seen" this generation so future layout flushes do not repeatedly treat the
      // document as dirty.
      self.last_seen_dom_mutation_generation = self.dom.mutation_generation();
    }
  }

  fn apply_mutation_log(&mut self, mutations: crate::dom2::MutationLog) -> bool {
    if mutations.unclassified {
      self.invalidate_all();
      // Treat unclassified changes as render-affecting so generation-based dirtiness is preserved.
      return true;
    }

    let mut render_affecting = false;
    // Treat changes in disconnected/inert subtrees as non-render-affecting.
    for (node, attrs) in mutations.attribute_changed {
      if !self.dom.is_connected_for_scripting(node) {
        continue;
      }
      if self.attribute_mutation_affects_render(node, &attrs) {
        render_affecting = true;
        self.dirty_style_nodes.insert(node);
      }
    }
    for node in mutations.text_changed {
      if !self.dom.is_connected_for_scripting(node) {
        continue;
      }
      render_affecting = true;
      // Text changes inside <style> elements affect the stylesheet and require a full restyle.
      if self.text_node_affects_stylesheet(node) {
        self.dirty_style_nodes.insert(node);
      } else {
        self.dirty_text_nodes.insert(node);
      }
    }

    for parent in mutations.child_list_changed {
      if self.dom.is_connected_for_scripting(parent) {
        render_affecting = true;
        self.dirty_structure_nodes.insert(parent);
      }
    }

    // Track node-level structural changes in addition to parent-level child-list mutations. Hosts
    // can use these node ids to compute more precise paint damage for inserted/removed/moved
    // subtrees (e.g. absolutely-positioned descendants that can paint outside their parent's
    // border box).
    for node in mutations.nodes_inserted {
      if self.dom.is_connected_for_scripting(node) {
        render_affecting = true;
        self.dirty_structure_nodes.insert(node);
      }
    }

    // Removed nodes are recorded before they become disconnected, so treat them as structure-
    // affecting even though they are no longer connected in the current DOM snapshot.
    for node in mutations.nodes_removed {
      render_affecting = true;
      self.dirty_structure_nodes.insert(node);
    }

    // Convenience: moved nodes are already covered by inserted+removed, but keep them here so
    // future damage tracking can treat moves distinctly without re-deriving the intersection.
    for node in mutations.nodes_moved {
      render_affecting = true;
      self.dirty_structure_nodes.insert(node);
    }

    // Form control state changes (e.g. `.value`, `.checked`) are render-affecting but should not be
    // treated as style-affecting attribute mutations. Conservatively mark layout+paint dirty so the
    // next render takes a fresh renderer DOM snapshot and projects live form state into it.
    //
    // Note: `dom2::MutationLog` models these separately so hosts can add future incremental paths
    // (e.g. repainting replaced form controls) without forcing a full restyle.
    for &node in &mutations.form_state_changed {
      if !self.dom.is_connected_for_scripting(node) {
        continue;
      }

      // Only treat form-state mutations as render-affecting when they can influence the rendered
      // output. Some form controls never paint (`type=hidden`), and some state is tracked out of DOM
      // (`type=file` uses `InteractionState`).
      let affects_render = match &self.dom.node(node).kind {
        crate::dom2::NodeKind::Element {
          tag_name,
          namespace,
          attributes,
          ..
        } if self.dom.is_html_case_insensitive_namespace(namespace) => {
          if tag_name.eq_ignore_ascii_case("input") {
            let ty = attributes
              .iter()
              .find(|attr| {
                attr.namespace == crate::dom2::NULL_NAMESPACE
                  && attr.local_name.eq_ignore_ascii_case("type")
              })
              .map(|attr| attr.value.as_str())
              .unwrap_or("text");
            !(ty.eq_ignore_ascii_case("hidden")
              || ty.eq_ignore_ascii_case("image")
              || ty.eq_ignore_ascii_case("file"))
          } else if tag_name.eq_ignore_ascii_case("textarea") || tag_name.eq_ignore_ascii_case("select") {
            true
          } else if tag_name.eq_ignore_ascii_case("option") {
            // Option selectedness only affects rendering when it participates in a <select>.
            let mut current = node;
            let mut in_select = false;
            while let Some(parent) = self.dom.parent_node(current) {
              match &self.dom.node(parent).kind {
                crate::dom2::NodeKind::Element {
                  tag_name,
                  namespace,
                  ..
                } if self.dom.is_html_case_insensitive_namespace(namespace)
                  && tag_name.eq_ignore_ascii_case("select") =>
                {
                  in_select = true;
                  break;
                }
                _ => {}
              }
              current = parent;
            }
            in_select
          } else {
            false
          }
        }
        // Conservatively treat unknown cases as render-affecting.
        _ => true,
      };

      if affects_render {
        render_affecting = true;
        self.form_state_dirty = true;
        self.layout_dirty = true;
        self.paint_dirty = true;
        self.dirty_form_state_style_nodes.insert(node);
      }
    }

    for node in mutations.composed_tree_changed {
      if self.dom.is_connected_for_scripting(node) {
        render_affecting = true;
        self.dirty_structure_nodes.insert(node);
      }
    }
    // Upgrade to the minimal set of coarse invalidation flags we can currently satisfy.
    if !self.dirty_structure_nodes.is_empty() || !self.dirty_style_nodes.is_empty() {
      self.style_dirty = true;
      self.layout_dirty = true;
      self.paint_dirty = true;
      return render_affecting;
    }

    if !self.dirty_text_nodes.is_empty() {
      self.layout_dirty = true;
      self.paint_dirty = true;
    }

    render_affecting
  }

  fn attribute_mutation_affects_render(
    &self,
    node: crate::dom2::NodeId,
    attrs: &FxHashSet<String>,
  ) -> bool {
    let node_ref = self.dom.node(node);
    match &node_ref.kind {
      crate::dom2::NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        let is_html = namespace.is_empty() || namespace == crate::dom::HTML_NAMESPACE;
        if !is_html {
          return true;
        }

        // `<link>`, `<style>`, `<base>`, and `<meta>` do not generate layout boxes. Mutations on
        // unrelated attributes should not force a full restyle/layout pass.
        if tag_name.eq_ignore_ascii_case("link") {
          return attrs.iter().any(|name| {
            matches!(
              name.as_str(),
              "href"
                | "rel"
                | "as"
                | "type"
                | "media"
                | "disabled"
                | "crossorigin"
                | "referrerpolicy"
            )
          });
        }

        if tag_name.eq_ignore_ascii_case("style") {
          return attrs.iter().any(|name| {
            matches!(name.as_str(), "media" | "type" | "nonce" | "disabled")
          });
        }

        if tag_name.eq_ignore_ascii_case("base") {
          return attrs.iter().any(|name| name == "href");
        }

        if tag_name.eq_ignore_ascii_case("meta") {
          // Viewport depends on `<meta name="viewport" content="...">`.
          if attrs.iter().any(|name| name == "name") {
            // A name change could add/remove the viewport meta; conservatively invalidate.
            return true;
          }
          if attrs.iter().any(|name| name == "content") {
            let meta_name = self
              .dom
              .get_attribute(node, "name")
              .ok()
              .flatten()
              .unwrap_or("");
            return meta_name.eq_ignore_ascii_case("viewport");
          }
          return false;
        }

        true
      }
      crate::dom2::NodeKind::Slot { .. } => true,
      _ => true,
    }
  }

  /// Returns `Ok(true)` if this document is eligible to satisfy the current style invalidation via
  /// incremental restyle (instead of a full restyle).
  ///
  /// Note: incremental restyle is conservative and must not be used when style changes can trigger
  /// top-layer derived attribute propagation (`open` toggles, `data-fastr-inert` propagation). In
  /// that case, the restyle scope would need to expand beyond the mutated subtree, so we force a
  /// full restyle instead.
  #[allow(dead_code)]
  fn incremental_restyle_is_eligible(&self) -> Result<bool> {
    if self.dirty_style_nodes.is_empty() {
      return Ok(false);
    }

    // Conservative safety check: avoid incremental restyle when mutations might affect dialog /
    // popover open state or modal inert propagation. These states are applied to the *renderer DOM*
    // snapshot via `dom::apply_top_layer_state_with_deadline`, so reusing styled subtrees outside
    // the dirty scope can produce stale `StyledNode.node` snapshots and computed styles.
    if self.dirty_style_nodes_may_affect_top_layer_state() {
      return Ok(false);
    }

    Ok(true)
  }

  #[allow(dead_code)]
  fn dirty_style_nodes_may_affect_top_layer_state(&self) -> bool {
    let dom = self.dom();
    for &node_id in &self.dirty_style_nodes {
      let node = dom.node(node_id);
      match &node.kind {
        crate::dom2::NodeKind::Element {
          tag_name,
          namespace,
          attributes,
          ..
        } => {
          let is_html = dom.is_html_case_insensitive_namespace(namespace);
          if is_html && tag_name.eq_ignore_ascii_case("dialog") {
            return true;
          }

          for attr in attributes {
            if attr.namespace == crate::dom2::NULL_NAMESPACE
              && (attr.qualified_name_matches("popover", is_html)
                || attr.qualified_name_matches("data-fastr-open", is_html)
                || attr.qualified_name_matches("data-fastr-modal", is_html))
            {
              return true;
            }
          }
        }
        crate::dom2::NodeKind::Slot {
          namespace,
          attributes,
          ..
        } => {
          let is_html = dom.is_html_case_insensitive_namespace(namespace);
          for attr in attributes {
            if attr.namespace == crate::dom2::NULL_NAMESPACE
              && (attr.qualified_name_matches("popover", is_html)
                || attr.qualified_name_matches("data-fastr-open", is_html)
                || attr.qualified_name_matches("data-fastr-modal", is_html))
            {
              return true;
            }
          }
        }
        _ => {}
      }
    }

    false
  }

  fn text_node_affects_stylesheet(&self, node: crate::dom2::NodeId) -> bool {
    let parent = self.dom.parent_node(node);
    let Some(parent) = parent else {
      return false;
    };
    let parent_node = self.dom.node(parent);
    match &parent_node.kind {
      crate::dom2::NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => namespace.is_empty() && tag_name.eq_ignore_ascii_case("style"),
      _ => false,
    }
  }

  fn incremental_relayout_for_text_changes(
    &mut self,
    prepared: &mut PreparedDocument,
  ) -> Result<bool> {
    let Some(mapping) = self.last_dom_mapping.as_ref() else {
      // Missing mapping implies we can't reliably map dom2 nodes to box-tree styled ids.
      return Ok(false);
    };

    // Map dom2 text node ids to renderer preorder ids (styled_node_id) for box lookup.
    let mut updates: FxHashMap<usize, String> = FxHashMap::default();
    let mut updated_styled_node_ids: FxHashSet<usize> = FxHashSet::default();
    for &node in &self.dirty_text_nodes {
      let Some(preorder) = mapping.preorder_for_node_id(node) else {
        // Mapping mismatch: fall back to a full pipeline run.
        return Ok(false);
      };
      let text = match &self.dom.node(node).kind {
        crate::dom2::NodeKind::Text { content } => content.clone(),
        _ => return Ok(false),
      };
      updates.insert(preorder, text);
      updated_styled_node_ids.insert(preorder);
    }

    if !updates.is_empty() {
      // Only take the incremental path when every dirty text node corresponds to a concrete
      // `BoxType::Text` entry in the box tree. Text nodes under replaced form controls like
      // `<textarea>`/`<option>` do not have a corresponding text box, so we must return `false` so
      // callers can fall back (or try another incremental path).
      let original_text =
        capture_text_for_styled_node_ids(&prepared.box_tree.root, &updated_styled_node_ids);
      let applied = apply_text_updates_to_box_tree(&mut prepared.box_tree.root, &updates);
      if applied.len() != updates.len() {
        // Restore any partial mutations so callers that fall back to a full pipeline run still see
        // the previously committed box tree.
        restore_text_for_box_ids(&mut prepared.box_tree.root, &original_text);
        return Ok(false);
      }
    }

    // Snapshot animation timing once so the layout/transition update is consistent within the call.
    let now_ms = super::sanitize_animation_time_ms(self.animation_time_for_paint());

    let options = self.options.clone();
    let toggles = self.renderer.resolve_runtime_toggles(&options);
    let _toggles_guard =
      super::RuntimeTogglesSwap::new(&mut self.renderer.runtime_toggles, toggles.clone());

    crate::debug::runtime::with_runtime_toggles(toggles, || {
      let trace = super::TraceSession::from_options(Some(&options));
      let trace_handle = trace.handle();
      let _root_span = trace_handle.span("browser_document_dom2_incremental_relayout", "pipeline");

      let shared_diagnostics =
        self
          .renderer
          .diagnostics
          .as_ref()
          .map(|diag| super::SharedRenderDiagnostics {
            inner: std::sync::Arc::clone(diag),
          });
      let context = Some(self.renderer.build_resource_context(
        self.renderer.document_url_hint(),
        shared_diagnostics,
        ReferrerPolicy::default(),
      ));
      let (prev_self, prev_image, prev_layout_image, prev_font) =
        self.renderer.push_resource_context(context);

      let result = (|| -> Result<()> {
        let deadline = crate::render_control::RenderDeadline::new(
          options.timeout,
          options.cancel_callback.clone(),
        );
        let _deadline_guard = crate::render_control::DeadlineGuard::install(Some(&deadline));
        crate::render_control::check_active(RenderStage::Layout).map_err(Error::Render)?;

        let memory_sampling_enabled = options.stage_mem_budget_bytes.is_some();
        let layout_rss_start = memory_sampling_enabled
          .then(crate::memory::current_rss_bytes)
          .flatten();
        super::check_stage_mem_budget(
          RenderStage::Layout,
          layout_rss_start,
          options.stage_mem_budget_bytes,
        )?;

        // Preserve document-level metadata that is not produced by layout itself.
        //
        // Full pipeline runs populate these fields from CSS parsing / DOM scanning (e.g. `@keyframes`,
        // SVG `<defs>` registries). `LayoutEngine::layout_tree_*` returns a fresh `FragmentTree`
        // without them, so incremental relayout must carry them forward.
        let preserved_keyframes = std::mem::take(&mut prepared.fragment_tree.keyframes);
        let preserved_svg_filter_defs = prepared.fragment_tree.svg_filter_defs.take();
        let preserved_svg_id_defs = prepared.fragment_tree.svg_id_defs.take();
        let preserved_svg_id_defs_raw = prepared.fragment_tree.svg_id_defs_raw.take();

        crate::render_control::record_stage(crate::render_control::StageHeartbeat::Layout);
        let _layout_span = trace_handle.span("layout_tree", "layout");
        let layout_result = self
          .renderer
          .layout_engine
          .layout_tree_with_trace(&prepared.box_tree, trace_handle);
        let mut fragment_tree = match layout_result {
          Ok(tree) => tree,
          Err(err) => {
            // Restore preserved metadata when layout fails so callers can retry without losing
            // non-layout caches.
            prepared.fragment_tree.keyframes = preserved_keyframes;
            prepared.fragment_tree.svg_filter_defs = preserved_svg_filter_defs;
            prepared.fragment_tree.svg_id_defs = preserved_svg_id_defs;
            prepared.fragment_tree.svg_id_defs_raw = preserved_svg_id_defs_raw;
            return Err(super::map_formatting_layout_error(err));
          }
        };
        drop(_layout_span);

        fragment_tree.keyframes = preserved_keyframes;
        fragment_tree.svg_filter_defs = preserved_svg_filter_defs;
        fragment_tree.svg_id_defs = preserved_svg_id_defs;
        fragment_tree.svg_id_defs_raw = preserved_svg_id_defs_raw;

        // Full pipeline runs reattach starting-style snapshots after layout (fragmentation can clear
        // them while translating column/fragmentainer coordinates). Mirror that behavior so
        // transition sampling continues to see starting-style values after incremental relayout.
        fragment_tree.attach_starting_styles_from_boxes(&prepared.box_tree);

        // Preserve (and refresh) transition state across incremental relayouts.
        match now_ms {
          None => {
            fragment_tree.transition_state = None;
          }
          Some(_now_ms) => {
            if let Some(prev) = prepared.fragment_tree.transition_state.as_deref() {
              let mut next = prev.clone();
              next.capture_layout_from_fragment_tree(&fragment_tree);
              fragment_tree.transition_state = Some(Arc::new(next));
            } else {
              fragment_tree.transition_state = None;
            }
          }
        }

        prepared.fragment_tree = fragment_tree;
        Ok(())
      })();

      self
        .renderer
        .pop_resource_context(prev_self, prev_image, prev_layout_image, prev_font);
      drop(_root_span);
      trace.finalize(result)
    })?;

    Ok(true)
  }

  fn incremental_relayout_for_form_control_text_changes(
    &mut self,
    prepared: &mut PreparedDocument,
  ) -> Result<bool> {
    let Some(mapping) = self.last_dom_mapping.as_ref() else {
      return Ok(false);
    };

    fn attrs_get_ci<'a>(attrs: &'a [crate::dom2::Attribute], name: &str) -> Option<&'a str> {
      attrs
        .iter()
        .find(|attr| attr.namespace == crate::dom2::NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case(name))
        .map(|attr| attr.value.as_str())
    }

    fn collect_descendant_text(doc: &crate::dom2::Document, root: crate::dom2::NodeId) -> String {
      let mut out = String::new();
      let mut stack: Vec<crate::dom2::NodeId> = vec![root];
      while let Some(node_id) = stack.pop() {
        match &doc.node(node_id).kind {
          crate::dom2::NodeKind::Text { content } => out.push_str(content),
          _ => {}
        }
        for &child in doc.node(node_id).children.iter().rev() {
          if doc.node(child).parent == Some(node_id) {
            stack.push(child);
          }
        }
      }
      out
    }

    fn option_label_text(doc: &crate::dom2::Document, option: crate::dom2::NodeId) -> Option<String> {
      let crate::dom2::NodeKind::Element {
        tag_name,
        namespace,
        attributes,
        ..
      } = &doc.node(option).kind
      else {
        return None;
      };

      if !doc.is_html_case_insensitive_namespace(namespace) || !tag_name.eq_ignore_ascii_case("option") {
        return None;
      }

      if let Some(label) = attrs_get_ci(attributes, "label").filter(|label| !label.is_empty()) {
        return Some(label.to_string());
      }

      let mut text = String::new();
      let mut stack: Vec<crate::dom2::NodeId> = vec![option];
      while let Some(node_id) = stack.pop() {
        match &doc.node(node_id).kind {
          crate::dom2::NodeKind::Text { content } => text.push_str(content),
          crate::dom2::NodeKind::Element { tag_name, namespace, .. } => {
            if tag_name.eq_ignore_ascii_case("script")
              && (namespace.is_empty()
                || namespace == crate::dom::HTML_NAMESPACE
                || namespace == crate::dom::SVG_NAMESPACE)
            {
              continue;
            }
          }
          _ => {}
        }
        for &child in doc.node(node_id).children.iter().rev() {
          if doc.node(child).parent == Some(node_id) {
            stack.push(child);
          }
        }
      }

      Some(crate::dom::strip_and_collapse_ascii_whitespace(&text))
    }

    let dom = self.dom();

    let mut textarea_nodes: FxHashSet<crate::dom2::NodeId> = FxHashSet::default();
    let mut select_option_nodes: FxHashMap<crate::dom2::NodeId, FxHashSet<crate::dom2::NodeId>> =
      FxHashMap::default();

    for &node_id in &self.dirty_text_nodes {
      if mapping.preorder_for_node_id(node_id).is_none() {
        return Ok(false);
      }

      if !matches!(dom.node(node_id).kind, crate::dom2::NodeKind::Text { .. }) {
        return Ok(false);
      }

      let mut current = node_id;
      let mut found_textarea: Option<crate::dom2::NodeId> = None;
      let mut found_option: Option<crate::dom2::NodeId> = None;

      while let Some(parent) = dom.parent_node(current) {
        match &dom.node(parent).kind {
          crate::dom2::NodeKind::Element { tag_name, namespace, .. }
            if dom.is_html_case_insensitive_namespace(namespace) =>
          {
            if tag_name.eq_ignore_ascii_case("textarea") {
              found_textarea = Some(parent);
              break;
            }
            if tag_name.eq_ignore_ascii_case("option") {
              found_option = Some(parent);
              break;
            }
          }
          _ => {}
        }
        current = parent;
      }

      if let Some(textarea) = found_textarea {
        textarea_nodes.insert(textarea);
        continue;
      }

      let Some(option) = found_option else {
        return Ok(false);
      };

      let mut select: Option<crate::dom2::NodeId> = None;
      let mut ancestor = option;
      while let Some(parent) = dom.parent_node(ancestor) {
        if let crate::dom2::NodeKind::Element { tag_name, namespace, .. } = &dom.node(parent).kind {
          if dom.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case("select") {
            select = Some(parent);
            break;
          }
        }
        ancestor = parent;
      }

      let Some(select) = select else {
        return Ok(false);
      };

      select_option_nodes
        .entry(select)
        .or_default()
        .insert(option);
    }

    let mut textarea_updates: FxHashMap<usize, String> = FxHashMap::default();
    for textarea in textarea_nodes {
      let dirty = dom.textarea_value_is_dirty(textarea).map_err(|_| {
        Error::Render(RenderError::InvalidParameters {
          message: "textarea_value_is_dirty failed".to_string(),
        })
      })?;
      if dirty {
        continue;
      }

      let Some(textarea_preorder) = mapping.preorder_for_node_id(textarea) else {
        return Ok(false);
      };
      // Default value: concatenate descendant text node data in tree order.
      let value = collect_descendant_text(dom, textarea);
      textarea_updates.insert(textarea_preorder, value);
    }

    let mut select_updates: FxHashMap<usize, FxHashMap<usize, String>> = FxHashMap::default();
    for (select, option_nodes) in select_option_nodes {
      let Some(select_preorder) = mapping.preorder_for_node_id(select) else {
        return Ok(false);
      };
      let mut option_updates: FxHashMap<usize, String> = FxHashMap::default();
      for option in option_nodes {
        let Some(option_preorder) = mapping.preorder_for_node_id(option) else {
          return Ok(false);
        };
        let Some(label) = option_label_text(dom, option) else {
          return Ok(false);
        };
        option_updates.insert(option_preorder, label);
      }
      select_updates.insert(select_preorder, option_updates);
    }

    // Ensure every form control we intend to update actually exists in the current box tree. This
    // keeps `Ok(false)` "transactional": if we decide to fall back to a full pipeline run, we must
    // not have partially mutated cached layout artifacts.
    {
      let mut found_textareas: FxHashSet<usize> = FxHashSet::default();
      let mut found_selects: FxHashSet<usize> = FxHashSet::default();
      let mut stack: Vec<&BoxNode> = vec![&prepared.box_tree.root];
      while let Some(node) = stack.pop() {
        if let Some(styled_id) = node.styled_node_id {
          if let BoxType::Replaced(replaced) = &node.box_type {
            if let ReplacedType::FormControl(control) = &replaced.replaced_type {
              match &control.control {
                FormControlKind::TextArea { .. } => {
                  found_textareas.insert(styled_id);
                }
                FormControlKind::Select(_) => {
                  found_selects.insert(styled_id);
                }
                _ => {}
              }
            }
          }
          if let Some(control) = node.form_control.as_ref() {
            match &control.control {
              FormControlKind::TextArea { .. } => {
                found_textareas.insert(styled_id);
              }
              FormControlKind::Select(_) => {
                found_selects.insert(styled_id);
              }
              _ => {}
            }
          }
        }

        if let Some(body) = node.footnote_body.as_deref() {
          stack.push(body);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }

      if textarea_updates
        .keys()
        .any(|id| !found_textareas.contains(id))
      {
        return Ok(false);
      }
      if select_updates.keys().any(|id| !found_selects.contains(id)) {
        return Ok(false);
      }
    }

    enum UndoOp {
      TextareaReplaced {
        node_ptr: *mut BoxNode,
        value: String,
        caret: usize,
        selection: Option<(usize, usize)>,
      },
      TextareaFormControlArc {
        node_ptr: *mut BoxNode,
        form_control: Arc<crate::tree::box_tree::FormControl>,
      },
      SelectReplaced {
        node_ptr: *mut BoxNode,
        items: Arc<Vec<SelectItem>>,
      },
      SelectFormControlArc {
        node_ptr: *mut BoxNode,
        form_control: Arc<crate::tree::box_tree::FormControl>,
      },
    }

    let mut undo_ops: Vec<UndoOp> = Vec::new();
    let mut undo_all = |undo_ops: &mut Vec<UndoOp>| {
      for op in undo_ops.drain(..).rev() {
        // Safety: undo pointers refer to nodes owned by `prepared.box_tree.root`, which remain valid
        // because we never move nodes during traversal.
        unsafe {
          match op {
            UndoOp::TextareaReplaced {
              node_ptr,
              value,
              caret,
              selection,
            } => {
              let node = &mut *node_ptr;
              if let BoxType::Replaced(replaced) = &mut node.box_type {
                if let ReplacedType::FormControl(control) = &mut replaced.replaced_type {
                  if let FormControlKind::TextArea {
                    value: current,
                    caret: current_caret,
                    selection: current_sel,
                    ..
                  } = &mut control.control
                  {
                    *current = value;
                    *current_caret = caret;
                    *current_sel = selection;
                  }
                }
              }
            }
            UndoOp::TextareaFormControlArc {
              node_ptr,
              form_control,
            } => {
              let node = &mut *node_ptr;
              node.form_control = Some(form_control);
            }
            UndoOp::SelectReplaced {
              node_ptr,
              items,
            } => {
              let node = &mut *node_ptr;
              if let BoxType::Replaced(replaced) = &mut node.box_type {
                if let ReplacedType::FormControl(control) = &mut replaced.replaced_type {
                  if let FormControlKind::Select(select) = &mut control.control {
                    select.items = items;
                  }
                }
              }
            }
            UndoOp::SelectFormControlArc {
              node_ptr,
              form_control,
            } => {
              let node = &mut *node_ptr;
              node.form_control = Some(form_control);
            }
          }
        }
      }
    };

    let mut processed_textareas: FxHashSet<usize> = FxHashSet::default();
    let mut processed_selects: FxHashSet<usize> = FxHashSet::default();

    let mut stack: Vec<*mut BoxNode> = vec![&mut prepared.box_tree.root as *mut _];
    while let Some(node_ptr) = stack.pop() {
      // Safety: stack contains pointers to nodes owned by `root` and we never move nodes during the
      // traversal.
      unsafe {
        let node = &mut *node_ptr;
        let styled_id = node.styled_node_id;

        if let Some(styled_id) = styled_id {
          if let Some(new_value) = textarea_updates.get(&styled_id) {
            let mut handled = false;

            if let BoxType::Replaced(replaced) = &mut node.box_type {
              if let ReplacedType::FormControl(control) = &mut replaced.replaced_type {
                if let FormControlKind::TextArea {
                  value,
                  caret,
                  selection,
                  ..
                } = &mut control.control
                {
                  let old_len = value.chars().count();
                  let new_len = new_value.chars().count();
                  let old_caret = *caret;
                  undo_ops.push(UndoOp::TextareaReplaced {
                    node_ptr,
                    value: value.clone(),
                    caret: *caret,
                    selection: *selection,
                  });

                  value.clear();
                  value.push_str(new_value);

                  if old_caret == old_len {
                    *caret = new_len;
                  } else {
                    *caret = old_caret.min(new_len);
                  }
                  if let Some((start, end)) = selection {
                    let start = (*start).min(new_len);
                    let end = (*end).min(new_len);
                    *selection = if start == end {
                      None
                    } else if start < end {
                      Some((start, end))
                    } else {
                      Some((end, start))
                    };
                  }

                  handled = true;
                }
              }
            }

            if let Some(form_control) = node.form_control.as_mut() {
              if matches!(&form_control.control, FormControlKind::TextArea { .. }) {
                undo_ops.push(UndoOp::TextareaFormControlArc {
                  node_ptr,
                  form_control: Arc::clone(form_control),
                });
                let form_control = Arc::make_mut(form_control);
                if let FormControlKind::TextArea {
                  value,
                  caret,
                  selection,
                  ..
                } = &mut form_control.control
                {
                  let old_len = value.chars().count();
                  let new_len = new_value.chars().count();
                  let old_caret = *caret;

                  value.clear();
                  value.push_str(new_value);

                  if old_caret == old_len {
                    *caret = new_len;
                  } else {
                    *caret = old_caret.min(new_len);
                  }
                  if let Some((start, end)) = selection {
                    let start = (*start).min(new_len);
                    let end = (*end).min(new_len);
                    *selection = if start == end {
                      None
                    } else if start < end {
                      Some((start, end))
                    } else {
                      Some((end, start))
                    };
                  }

                  handled = true;
                }
              }
            }

            if handled {
              processed_textareas.insert(styled_id);
            }
          }

          if let Some(option_updates) = select_updates.get(&styled_id) {
            let mut any_patched = false;
            let mut all_patches_complete = true;

            let patch_select_control =
              |select: &mut crate::tree::box_tree::SelectControl| -> bool {
                let mut items = select.items.as_ref().clone();
                let mut any_label_change = false;
                let mut found_options: FxHashSet<usize> = FxHashSet::default();

                for item in items.iter_mut() {
                  let SelectItem::Option { node_id, label, .. } = item else {
                    continue;
                  };
                  if let Some(new_label) = option_updates.get(node_id) {
                    found_options.insert(*node_id);
                    if label != new_label {
                      *label = new_label.clone();
                      any_label_change = true;
                    }
                  }
                }

                let all_found = option_updates.keys().all(|id| found_options.contains(id));
                if any_label_change {
                  select.items = Arc::new(items);
                }
                all_found
              };

            if let BoxType::Replaced(replaced) = &mut node.box_type {
              if let ReplacedType::FormControl(control) = &mut replaced.replaced_type {
                if let FormControlKind::Select(select) = &mut control.control {
                  any_patched = true;
                  undo_ops.push(UndoOp::SelectReplaced {
                    node_ptr,
                    items: Arc::clone(&select.items),
                  });
                  if !patch_select_control(select) {
                    all_patches_complete = false;
                  }
                }
              }
            }

            if let Some(form_control) = node.form_control.as_mut() {
              if matches!(&form_control.control, FormControlKind::Select(_)) {
                any_patched = true;
                undo_ops.push(UndoOp::SelectFormControlArc {
                  node_ptr,
                  form_control: Arc::clone(form_control),
                });
                let form_control = Arc::make_mut(form_control);
                if let FormControlKind::Select(select) = &mut form_control.control {
                  if !patch_select_control(select) {
                    all_patches_complete = false;
                  }
                }
              }
            }

            if any_patched && all_patches_complete {
              processed_selects.insert(styled_id);
            }
          }
        }

        if let Some(body) = node.footnote_body.as_deref_mut() {
          stack.push(body as *mut _);
        }
        for child in node.children.iter_mut().rev() {
          stack.push(child as *mut _);
        }
      }
    }

    if textarea_updates
      .keys()
      .any(|id| !processed_textareas.contains(id))
    {
      undo_all(&mut undo_ops);
      return Ok(false);
    }

    if select_updates.keys().any(|id| !processed_selects.contains(id)) {
      undo_all(&mut undo_ops);
      return Ok(false);
    }

    // Snapshot animation timing once so the layout/transition update is consistent within the call.
    let now_ms = super::sanitize_animation_time_ms(self.animation_time_for_paint());

    let options = self.options.clone();
    let toggles = self.renderer.resolve_runtime_toggles(&options);
    let _toggles_guard =
      super::RuntimeTogglesSwap::new(&mut self.renderer.runtime_toggles, toggles.clone());

    let layout_result = crate::debug::runtime::with_runtime_toggles(toggles, || {
      let trace = super::TraceSession::from_options(Some(&options));
      let trace_handle = trace.handle();
      let _root_span =
        trace_handle.span("browser_document_dom2_incremental_relayout_form_controls", "pipeline");

      let shared_diagnostics =
        self
          .renderer
          .diagnostics
          .as_ref()
          .map(|diag| super::SharedRenderDiagnostics {
            inner: std::sync::Arc::clone(diag),
          });
      let context = Some(self.renderer.build_resource_context(
        self.renderer.document_url_hint(),
        shared_diagnostics,
        ReferrerPolicy::default(),
      ));
      let (prev_self, prev_image, prev_layout_image, prev_font) =
        self.renderer.push_resource_context(context);

      let result = (|| -> Result<()> {
        let deadline = crate::render_control::RenderDeadline::new(
          options.timeout,
          options.cancel_callback.clone(),
        );
        let _deadline_guard = crate::render_control::DeadlineGuard::install(Some(&deadline));
        crate::render_control::check_active(RenderStage::Layout).map_err(Error::Render)?;

        let memory_sampling_enabled = options.stage_mem_budget_bytes.is_some();
        let layout_rss_start = memory_sampling_enabled
          .then(crate::memory::current_rss_bytes)
          .flatten();
        super::check_stage_mem_budget(
          RenderStage::Layout,
          layout_rss_start,
          options.stage_mem_budget_bytes,
        )?;

        // Preserve document-level metadata that is not produced by layout itself.
        //
        // Full pipeline runs populate these fields from CSS parsing / DOM scanning (e.g. `@keyframes`,
        // SVG `<defs>` registries). `LayoutEngine::layout_tree_*` returns a fresh `FragmentTree`
        // without them, so incremental relayout must carry them forward.
        let preserved_keyframes = std::mem::take(&mut prepared.fragment_tree.keyframes);
        let preserved_svg_filter_defs = prepared.fragment_tree.svg_filter_defs.take();
        let preserved_svg_id_defs = prepared.fragment_tree.svg_id_defs.take();
        let preserved_svg_id_defs_raw = prepared.fragment_tree.svg_id_defs_raw.take();

        crate::render_control::record_stage(crate::render_control::StageHeartbeat::Layout);
        let _layout_span = trace_handle.span("layout_tree", "layout");
        let layout_result = self
          .renderer
          .layout_engine
          .layout_tree_with_trace(&prepared.box_tree, trace_handle);
        let mut fragment_tree = match layout_result {
          Ok(tree) => tree,
          Err(err) => {
            // Restore preserved metadata when layout fails so callers can retry without losing
            // non-layout caches.
            prepared.fragment_tree.keyframes = preserved_keyframes;
            prepared.fragment_tree.svg_filter_defs = preserved_svg_filter_defs;
            prepared.fragment_tree.svg_id_defs = preserved_svg_id_defs;
            prepared.fragment_tree.svg_id_defs_raw = preserved_svg_id_defs_raw;
            return Err(super::map_formatting_layout_error(err));
          }
        };
        drop(_layout_span);

        fragment_tree.keyframes = preserved_keyframes;
        fragment_tree.svg_filter_defs = preserved_svg_filter_defs;
        fragment_tree.svg_id_defs = preserved_svg_id_defs;
        fragment_tree.svg_id_defs_raw = preserved_svg_id_defs_raw;

        // Full pipeline runs reattach starting-style snapshots after layout (fragmentation can clear
        // them while translating column/fragmentainer coordinates). Mirror that behavior so
        // transition sampling continues to see starting-style values after incremental relayout.
        fragment_tree.attach_starting_styles_from_boxes(&prepared.box_tree);

        // Preserve (and refresh) transition state across incremental relayouts.
        match now_ms {
          None => {
            fragment_tree.transition_state = None;
          }
          Some(_now_ms) => {
            if let Some(prev) = prepared.fragment_tree.transition_state.as_deref() {
              let mut next = prev.clone();
              next.capture_layout_from_fragment_tree(&fragment_tree);
              fragment_tree.transition_state = Some(Arc::new(next));
            } else {
              fragment_tree.transition_state = None;
            }
          }
        }

        prepared.fragment_tree = fragment_tree;
        Ok(())
      })();

      self
        .renderer
        .pop_resource_context(prev_self, prev_image, prev_layout_image, prev_font);
      drop(_root_span);
      trace.finalize(result)
    });

    if let Err(err) = layout_result {
      undo_all(&mut undo_ops);
      return Err(err);
    }

    Ok(true)
  }
}

fn apply_text_updates_to_box_tree(
  root: &mut BoxNode,
  updates: &FxHashMap<usize, String>,
) -> FxHashSet<usize> {
  let mut applied: FxHashSet<usize> = FxHashSet::default();
  let mut stack: Vec<*mut BoxNode> = vec![root as *mut _];
  while let Some(node_ptr) = stack.pop() {
    // Safety: stack contains pointers to nodes owned by `root` and we never move nodes during the
    // traversal.
    unsafe {
      let node = &mut *node_ptr;
      if let Some(styled_id) = node.styled_node_id {
        if let Some(new_text) = updates.get(&styled_id) {
          if let BoxType::Text(text_box) = &mut node.box_type {
            text_box.text.clear();
            text_box.text.push_str(new_text);
            applied.insert(styled_id);
          }
        }
      }

      if let Some(body) = node.footnote_body.as_deref_mut() {
        stack.push(body as *mut _);
      }
      for child in node.children.iter_mut().rev() {
        stack.push(child as *mut _);
      }
    }
  }

  applied
}

fn principal_box_style_for_styled_node_id(
  root: &BoxNode,
  styled_node_id: usize,
) -> Option<Arc<ComputedStyle>> {
  let mut stack: Vec<&BoxNode> = vec![root];
  while let Some(node) = stack.pop() {
    // A "principal box" is the first non-pseudo box generated for this element in a pre-order walk
    // of the box tree.
    if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
      return Some(Arc::clone(&node.style));
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn styled_tree_style_for_preorder_id(root: &StyledNode, preorder_id: usize) -> Option<Arc<ComputedStyle>> {
  let mut stack: Vec<&StyledNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.node_id == preorder_id {
      return Some(Arc::clone(&node.styles));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_box_node_by_id<'a>(root: &'a BoxNode, box_id: usize) -> Option<&'a BoxNode> {
  let mut stack: Vec<&BoxNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.id == box_id {
      return Some(node);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn capture_text_for_styled_node_ids(
  root: &BoxNode,
  styled_node_ids: &FxHashSet<usize>,
) -> FxHashMap<usize, String> {
  let mut out: FxHashMap<usize, String> = FxHashMap::default();
  let mut stack: Vec<&BoxNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node
      .styled_node_id
      .is_some_and(|id| styled_node_ids.contains(&id))
    {
      if let BoxType::Text(text_box) = &node.box_type {
        out.insert(node.id, text_box.text.clone());
      }
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  out
}

fn restore_text_for_box_ids(root: &mut BoxNode, text_by_box_id: &FxHashMap<usize, String>) {
  let mut stack: Vec<*mut BoxNode> = vec![root as *mut _];
  while let Some(node_ptr) = stack.pop() {
    // Safety: stack contains pointers to nodes owned by `root` and we never move nodes during the
    // traversal.
    unsafe {
      let node = &mut *node_ptr;
      if let Some(text) = text_by_box_id.get(&node.id) {
        if let BoxType::Text(text_box) = &mut node.box_type {
          text_box.text.clear();
          text_box.text.push_str(text);
        }
      }

      if let Some(body) = node.footnote_body.as_deref_mut() {
        stack.push(body as *mut _);
      }
      for child in node.children.iter_mut().rev() {
        stack.push(child as *mut _);
      }
    }
  }
}

fn build_incremental_restyle_scope(
  renderer_dom: &crate::dom::DomNode,
  mapping: &crate::dom2::RendererDomMapping,
  dom2: &crate::dom2::Document,
  dirty_style_nodes: &FxHashSet<crate::dom2::NodeId>,
) -> Option<std::collections::HashSet<usize>> {
  let dom2_root = dom2.root();
  let mut restyle_roots: std::collections::HashSet<usize> = std::collections::HashSet::new();
  let mut ancestors: std::collections::HashSet<usize> = std::collections::HashSet::new();

  for &dirty in dirty_style_nodes {
    let mut restyle_root = dom2.parent_node(dirty).unwrap_or(dom2_root);
    // `<slot>` nodes live inside shadow roots, but their attributes affect the composed/rendered
    // tree by changing which light-DOM children are assigned. When a slot mutates we must restyle
    // from the shadow host so host descendants do not reuse stale slot-assignment metadata.
    if matches!(
      &dom2.node(dirty).kind,
      crate::dom2::NodeKind::Slot { .. }
    ) {
      if let Some(shadow_root) = dom2.shadow_root_ancestor(dirty) {
        if let Some(host) = dom2.parent_node(shadow_root) {
          restyle_root = host;
        }
      }
    }
    let restyle_root_preorder = mapping.preorder_for_node_id(restyle_root)?;
    restyle_roots.insert(restyle_root_preorder);

    let mut current = restyle_root;
    loop {
      let preorder = mapping.preorder_for_node_id(current)?;
      ancestors.insert(preorder);
      if current == dom2_root {
        break;
      }
      current = dom2.parent_node(current)?;
    }
  }

  enum Frame<'a> {
    Enter(&'a crate::dom::DomNode),
    Exit(usize),
  }

  let mut out: std::collections::HashSet<usize> = std::collections::HashSet::new();
  let mut stack: Vec<Frame<'_>> = vec![Frame::Enter(renderer_dom)];
  let mut next_id: usize = 1;
  let mut active_restyle_roots: usize = 0;

  while let Some(frame) = stack.pop() {
    match frame {
      Frame::Enter(node) => {
        let node_id = next_id;
        next_id = next_id.saturating_add(1);

        let mut decrement_on_exit = 0;
        if restyle_roots.contains(&node_id) {
          active_restyle_roots = active_restyle_roots.saturating_add(1);
          decrement_on_exit = 1;
        }

        if active_restyle_roots > 0 || ancestors.contains(&node_id) {
          out.insert(node_id);
        }

        if decrement_on_exit > 0 {
          stack.push(Frame::Exit(decrement_on_exit));
        }
        for child in node.children.iter().rev() {
          stack.push(Frame::Enter(child));
        }
      }
      Frame::Exit(dec) => {
        active_restyle_roots = active_restyle_roots.saturating_sub(dec);
      }
    }
  }

  Some(out)
}

impl crate::js::DomHost for BrowserDocumentDom2 {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&crate::dom2::Document) -> R,
  {
    f(self.dom())
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut crate::dom2::Document) -> (R, bool),
  {
    let generation_before = self.dom.mutation_generation();
    let generation_in_sync_before =
      generation_before == self.last_seen_dom_mutation_generation;
    let (result, changed) = f(self.dom.as_mut());
    if changed {
      let mutations = self.dom.take_mutations();
      if mutations.is_empty() {
        self.invalidate_all();
      } else {
        let render_affecting = self.apply_mutation_log(mutations);
        if !render_affecting && generation_in_sync_before {
          self.last_seen_dom_mutation_generation = self.dom.mutation_generation();
        }
      }
    } else {
      self.dom.clear_mutations();
    }
    result
  }
}

// NOTE: `BrowserDocumentDom2` intentionally does not implement `WebIdlBindingsHost` here; WebIDL
// host dispatch lives in `src/js/webidl/vmjs_host_dispatch.rs`.
#[cfg(test)]
mod tests {
  use super::*;
  use crate::interaction::selection_serialize::{DocumentSelectionPointDom2, DocumentSelectionRangeDom2};
  use crate::interaction::state::{DocumentSelectionRangesDom2, DocumentSelectionStateDom2};
  use crate::tree::box_tree::BoxTree;
  use selectors::context::QuirksMode;

  fn renderer_for_tests() -> super::super::FastRender {
    super::super::FastRender::builder()
      .font_sources(crate::text::font_db::FontConfig::bundled_only())
      .build()
      .expect("renderer")
  }

  fn count_document_selection_pixels(pixmap: &tiny_skia::Pixmap) -> usize {
    let mut count = 0usize;
    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        let p = pixmap.pixel(x, y).expect("pixel in bounds");
        // Selection highlight is drawn as a light blue tint (e.g. rgba(0,120,215,0.35) over white
        // -> ~rgb(166,208,241)). Use a loose heuristic to avoid coupling tightly to exact color
        // constants.
        if p.blue() > 220 && p.green() > 180 && p.red() < 220 {
          count += 1;
        }
      }
    }
    count
  }

  fn caret_red_x_range(pixmap: &tiny_skia::Pixmap) -> Option<(u32, u32)> {
    let mut min_x: Option<u32> = None;
    let mut max_x: Option<u32> = None;
    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        let p = pixmap.pixel(x, y).expect("pixel in bounds");
        // Caret pixels are rendered with `caret-color` (we set it to red). Allow some tolerance for
        // antialiasing.
        if p.red() > 200 && p.green() < 80 && p.blue() < 80 {
          min_x = Some(min_x.map_or(x, |m| m.min(x)));
          max_x = Some(max_x.map_or(x, |m| m.max(x)));
        }
      }
    }
    min_x.zip(max_x)
  }

  fn assert_channel_close(actual: u8, expected: u8, tolerance: u8) {
    let diff = actual.abs_diff(expected);
    assert!(
      diff <= tolerance,
      "expected channel {expected}±{tolerance}, got {actual} (diff={diff})"
    );
  }

  fn assert_rgb_close(color: tiny_skia::PremultipliedColorU8, expected: u8, tolerance: u8) {
    assert_channel_close(color.red(), expected, tolerance);
    assert_channel_close(color.green(), expected, tolerance);
    assert_channel_close(color.blue(), expected, tolerance);
    assert_eq!(color.alpha(), 255);
  }

  fn first_text_node_id(doc: &crate::dom2::Document) -> Option<crate::dom2::NodeId> {
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
      let node = doc.node(id);
      if matches!(node.kind, crate::dom2::NodeKind::Text { .. }) {
        return Some(id);
      }
      for &child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn first_text_node_in_inert_template(doc: &crate::dom2::Document) -> Option<crate::dom2::NodeId> {
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
      let node = doc.node(id);
      if matches!(node.kind, crate::dom2::NodeKind::Text { .. })
        && doc.is_descendant_of_inert_template(id)
      {
        return Some(id);
      }
      for &child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn first_text_child(
    doc: &crate::dom2::Document,
    parent: crate::dom2::NodeId,
  ) -> Option<crate::dom2::NodeId> {
    doc
      .node(parent)
      .children
      .iter()
      .copied()
      .find(|&child| {
        doc.node(child).parent == Some(parent)
          && matches!(doc.node(child).kind, crate::dom2::NodeKind::Text { .. })
      })
  }

  fn find_first_textarea_control_value(root: &BoxNode) -> Option<String> {
    let mut stack: Vec<&BoxNode> = vec![root];
    while let Some(node) = stack.pop() {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let ReplacedType::FormControl(control) = &replaced.replaced_type {
          if let FormControlKind::TextArea { value, .. } = &control.control {
            return Some(value.clone());
          }
        }
      }
      if let Some(control) = node.form_control.as_ref() {
        if let FormControlKind::TextArea { value, .. } = &control.control {
          return Some(value.clone());
        }
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn find_first_text_input_value(root: &BoxNode) -> Option<String> {
    let mut stack: Vec<&BoxNode> = vec![root];
    while let Some(node) = stack.pop() {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let ReplacedType::FormControl(control) = &replaced.replaced_type {
          if let FormControlKind::Text { value, .. } = &control.control {
            return Some(value.clone());
          }
        }
      }
      if let Some(control) = node.form_control.as_ref() {
        if let FormControlKind::Text { value, .. } = &control.control {
          return Some(value.clone());
        }
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn find_first_checkbox_checked(root: &BoxNode) -> Option<bool> {
    let mut stack: Vec<&BoxNode> = vec![root];
    while let Some(node) = stack.pop() {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let ReplacedType::FormControl(control) = &replaced.replaced_type {
          if let FormControlKind::Checkbox { checked, .. } = &control.control {
            return Some(*checked);
          }
        }
      }
      if let Some(control) = node.form_control.as_ref() {
        if let FormControlKind::Checkbox { checked, .. } = &control.control {
          return Some(*checked);
        }
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn find_first_select_control(root: &BoxNode) -> Option<crate::tree::box_tree::SelectControl> {
    let mut stack: Vec<&BoxNode> = vec![root];
    while let Some(node) = stack.pop() {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let ReplacedType::FormControl(control) = &replaced.replaced_type {
          if let FormControlKind::Select(select) = &control.control {
            return Some(select.clone());
          }
        }
      }
      if let Some(control) = node.form_control.as_ref() {
        if let FormControlKind::Select(select) = &control.control {
          return Some(select.clone());
        }
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn find_first_html_script_by_id(
    doc: &crate::dom2::Document,
    id_value: &str,
  ) -> Option<crate::dom2::NodeId> {
    doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        crate::dom2::NodeKind::Element {
          tag_name,
          namespace,
          attributes,
          ..
        } if tag_name.eq_ignore_ascii_case("script")
          && (namespace.is_empty() || namespace == crate::dom::HTML_NAMESPACE)
          && attributes
            .iter()
            .any(|attr| attr.qualified_name().eq_ignore_ascii_case("id") && attr.value == id_value) =>
        {
          Some(crate::dom2::NodeId::from_index(idx))
        }
        _ => None,
      })
  }

  fn find_renderer_element_by_id<'a>(
    root: &'a crate::dom::DomNode,
    id_value: &str,
  ) -> Option<&'a crate::dom::DomNode> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
      if node.is_element() && node.get_attribute_ref("id") == Some(id_value) {
        return Some(node);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn collect_box_id_to_styled_node_id(box_tree: &BoxTree) -> FxHashMap<usize, usize> {
    let mut mapping: FxHashMap<usize, usize> = FxHashMap::default();
    let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
    while let Some(node) = stack.pop() {
      if let Some(styled_id) = node.styled_node_id {
        mapping.insert(node.id, styled_id);
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    mapping
  }

  fn collect_selected_styled_node_ids(
    fragment_tree: &crate::tree::fragment_tree::FragmentTree,
    box_tree: &BoxTree,
  ) -> FxHashSet<usize> {
    let box_id_to_styled_node_id = collect_box_id_to_styled_node_id(box_tree);
    let mut selected: FxHashSet<usize> = FxHashSet::default();

    let mut stack: Vec<&crate::tree::fragment_tree::FragmentNode> = Vec::new();
    for root in fragment_tree.additional_fragments.iter().rev() {
      stack.push(root);
    }
    stack.push(&fragment_tree.root);

    while let Some(node) = stack.pop() {
      if let crate::tree::fragment_tree::FragmentContent::Text {
        document_selection,
        box_id,
        ..
      } = &node.content
      {
        if document_selection.is_some() {
          if let Some(box_id) = box_id {
            if let Some(styled_id) = box_id_to_styled_node_id.get(box_id).copied() {
              selected.insert(styled_id);
            }
          }
        }
      }

      if matches!(
        node.content,
        crate::tree::fragment_tree::FragmentContent::RunningAnchor { .. }
          | crate::tree::fragment_tree::FragmentContent::FootnoteAnchor { .. }
      ) {
        continue;
      }

      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }

    selected
  }

  #[test]
  fn document_selection_dom2_tracks_dom_mutations() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = "<!doctype html><html><body><div id=a>hello</div></body></html>";
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(200, 40))?;

    let div = doc.dom().get_element_by_id("a").expect("div");
    let text = first_text_child(doc.dom(), div).expect("text node");

    doc.set_document_selection_dom2(Some(DocumentSelectionStateDom2::Ranges(
      DocumentSelectionRangesDom2 {
        ranges: vec![DocumentSelectionRangeDom2 {
          start: DocumentSelectionPointDom2 {
            node_id: text,
            char_offset: 1,
          },
          end: DocumentSelectionPointDom2 {
            node_id: text,
            char_offset: 4,
          },
        }],
        primary: 0,
        anchor: DocumentSelectionPointDom2 {
          node_id: text,
          char_offset: 1,
        },
        focus: DocumentSelectionPointDom2 {
          node_id: text,
          char_offset: 4,
        },
      },
    )));

    doc.render_frame()?;

    let mapping_1 = doc.last_dom_mapping().expect("dom mapping after render");
    let preorder_1 = mapping_1
      .preorder_for_node_id(text)
      .expect("text node should be connected");

    let prepared_1 = doc.prepared().expect("prepared document after render");
    let selected_1 = collect_selected_styled_node_ids(prepared_1.fragment_tree(), prepared_1.box_tree());
    assert!(
      selected_1.contains(&preorder_1),
      "expected selection highlight to reference preorder {preorder_1}"
    );

    // Insert a new earlier sibling before the selected text node, shifting renderer preorder ids.
    let changed = doc.mutate_dom(|dom| {
      let new_text = dom.create_text("X");
      dom.insert_before(div, new_text, Some(text))
        .expect("insert before");
      true
    });
    assert!(changed);

    doc.render_frame()?;

    let mapping_2 = doc.last_dom_mapping().expect("dom mapping after second render");
    let preorder_2 = mapping_2
      .preorder_for_node_id(text)
      .expect("text node should still be connected");
    assert_ne!(preorder_1, preorder_2, "expected preorder ids to shift");

    let prepared_2 = doc.prepared().expect("prepared document after second render");
    let selected_2 = collect_selected_styled_node_ids(prepared_2.fragment_tree(), prepared_2.box_tree());
    assert!(
      selected_2.contains(&preorder_2),
      "expected selection highlight to reference updated preorder {preorder_2}"
    );
    assert!(
      !selected_2.contains(&preorder_1),
      "expected selection highlight to no longer reference old preorder {preorder_1}"
    );
    Ok(())
  }

  #[test]
  fn hit_test_viewport_point_returns_node_and_metadata_for_link() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html>
      <html>
        <body style="margin:0">
          <a id="link" href="https://example.invalid/" style="display:block;width:50px;height:20px">Link</a>
        </body>
      </html>"#;
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(100, 100))?;
 
    let link_id = doc.dom().get_element_by_id("link").expect("link element");
 
    let result = doc.hit_test_viewport_point(5.0, 5.0)?;
    let result = result.expect("hit result");
    assert_eq!(result.node, link_id);
    assert_eq!(result.hit.kind, crate::interaction::HitTestKind::Link);
    assert_eq!(result.hit.href.as_deref(), Some("https://example.invalid/"));
    Ok(())
  }

  #[test]
  fn hit_test_viewport_point_returns_none_outside_viewport() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html><html><body><div>Hi</div></body></html>"#;
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(32, 32))?;
 
    assert!(doc.hit_test_viewport_point(40.0, 10.0)?.is_none());
    assert!(doc.hit_test_viewport_point(10.0, 40.0)?.is_none());
    Ok(())
  }

  #[test]
  fn hit_test_viewport_point_all_returns_hits_in_stack_order() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #a, #b { position: absolute; left: 0; top: 0; width: 50px; height: 50px; }
            #a { background: red; z-index: 1; }
            #b { background: blue; z-index: 2; }
          </style>
        </head>
        <body>
          <div id="a"></div>
          <div id="b"></div>
        </body>
      </html>"#;
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(100, 100))?;

    let a_id = doc.dom().get_element_by_id("a").expect("a element");
    let b_id = doc.dom().get_element_by_id("b").expect("b element");

    let hits = doc.hit_test_viewport_point_all(10.0, 10.0)?;
    assert_eq!(hits.first().map(|hit| hit.node), Some(b_id));
    assert!(hits.iter().any(|hit| hit.node == a_id));
    Ok(())
  }

  #[test]
  fn incremental_restyle_scope_includes_wbr_synthetic_zwsp() {
    // Create a dom2 tree with a `<wbr>` element that does **not** contain a ZWSP text node; the
    // renderer snapshot should inject the ZWSP as a synthetic renderer node.
    let mut dom2 = crate::dom2::Document::new(QuirksMode::NoQuirks);
    let doc_root = dom2.root();

    let html = dom2.create_element("html", "");
    dom2.append_child(doc_root, html).unwrap();
    let head = dom2.create_element("head", "");
    dom2.append_child(html, head).unwrap();
    let body = dom2.create_element("body", "");
    dom2.append_child(html, body).unwrap();

    let scope_div = dom2.create_element("div", "");
    dom2.set_attribute(scope_div, "id", "scope").unwrap();
    dom2.append_child(body, scope_div).unwrap();

    let dirty_node = dom2.create_element("span", "");
    dom2.set_attribute(dirty_node, "id", "dirty").unwrap();
    dom2.append_child(scope_div, dirty_node).unwrap();

    let wbr = dom2.create_element("wbr", "");
    dom2.set_attribute(wbr, "id", "w").unwrap();
    dom2.append_child(scope_div, wbr).unwrap();

    let snapshot = dom2.to_renderer_dom_with_mapping();
    let mut dirty_style_nodes: FxHashSet<crate::dom2::NodeId> = FxHashSet::default();
    dirty_style_nodes.insert(dirty_node);

    let scope = build_incremental_restyle_scope(
      &snapshot.dom,
      &snapshot.mapping,
      &dom2,
      &dirty_style_nodes,
    )
    .expect("incremental restyle scope");

    assert!(
      scope.contains(&1),
      "incremental restyle scope must always include the renderer root id when dirty nodes exist"
    );

    let wbr_id = dom2.get_element_by_id("w").expect("wbr element");
    let wbr_node = find_renderer_element_by_id(&snapshot.dom, "w").expect("wbr node in snapshot");
    assert_eq!(
      wbr_node.children.len(),
      1,
      "<wbr> should have a synthetic ZWSP child in renderer snapshots"
    );
    let zwsp_node = &wbr_node.children[0];
    assert_eq!(zwsp_node.text_content(), Some("\u{200B}"));

    let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);
    let wbr_preorder = *renderer_ids
      .get(&(wbr_node as *const crate::dom::DomNode))
      .expect("preorder for <wbr>");
    let zwsp_preorder = *renderer_ids
      .get(&(zwsp_node as *const crate::dom::DomNode))
      .expect("preorder for ZWSP node");

    assert_eq!(
      snapshot.mapping.preorder_for_node_id(wbr_id),
      Some(wbr_preorder),
      "<wbr> node id should map to its element preorder id"
    );
    assert_eq!(
      snapshot.mapping.node_id_for_preorder(zwsp_preorder),
      Some(wbr_id),
      "ZWSP preorder id must map back to the parent `<wbr>` dom2 node id"
    );

    assert!(
      scope.contains(&zwsp_preorder),
      "restyle scope must include synthetic ZWSP child inside restyle roots"
    );
  }

  #[test]
  fn set_scroll_updates_scroll_delta_and_noops_when_unchanged() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    doc.set_scroll(0.0, 0.0);
    assert_eq!(doc.options.scroll_delta, Point::ZERO);

    doc.set_scroll(10.0, 20.0);
    assert_eq!(doc.options.scroll_delta, Point::new(10.0, 20.0));

    let before = doc.options.scroll_delta;
    doc.set_scroll(10.0, 20.0);
    assert_eq!(doc.options.scroll_delta, before);
  }

  #[test]
  fn wheel_scroll_at_viewport_point_updates_element_scroll_offsets() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #scroller { width: 100px; height: 100px; overflow-y: scroll; }
            #content { height: 1000px; }
          </style>
        </head>
        <body>
          <div id="scroller"><div id="content"></div></div>
        </body>
      </html>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(200, 200))?;
    // Prime the cached layout (required by `wheel_scroll_at_viewport_point`).
    doc.render_frame()?;

    let scroller = doc.dom().get_element_by_id("scroller").expect("#scroller");
    let scroller_box_id = doc
      .principal_box_id_for_node(scroller)?
      .expect("principal box id for #scroller");

    assert!(
      doc.scroll_state().elements.is_empty(),
      "expected no element scroll offsets initially"
    );

    // Scrolling up when already at the top should not change state.
    assert!(
      !doc.wheel_scroll_at_viewport_point(Point::new(10.0, 10.0), (0.0, -10.0))?,
      "expected scroll-up at top to be a no-op"
    );

    // Scrolling down inside the scroll container should update its element scroll offset.
    assert!(
      doc.wheel_scroll_at_viewport_point(Point::new(10.0, 10.0), (0.0, 10.0))?,
      "expected wheel scroll to affect the scroll container"
    );
    let state = doc.scroll_state();
    assert_eq!(
      state.viewport,
      Point::ZERO,
      "expected viewport scroll to remain unchanged"
    );
    let offset = state.element_offset(scroller_box_id);
    assert!(
      offset.y > 0.0,
      "expected element scroll y to be >0, got {}",
      offset.y
    );

    // Zero delta should be a no-op and return false.
    assert!(
      !doc.wheel_scroll_at_viewport_point(Point::new(10.0, 10.0), (0.0, 0.0))?,
      "expected zero wheel delta to return false"
    );

    // Scrolling back up should reset the element scroll state to zero (and remove the entry).
    assert!(
      doc.wheel_scroll_at_viewport_point(Point::new(10.0, 10.0), (0.0, -10.0))?,
      "expected wheel scroll back up to change scroll state"
    );
    let state = doc.scroll_state();
    assert!(
      !state.elements.contains_key(&scroller_box_id),
      "expected element scroll offset to clear back to zero"
    );
    Ok(())
  }

  #[test]
  fn scroll_anchoring_resnaps_snapped_element_scroller_after_layout_change() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html><html><head><style>
html, body { margin: 0; padding: 0; }
#scroller { width: 100px; height: 100px; overflow-y: scroll; scroll-snap-type: y mandatory; }
.item { height: 50px; margin: 0; padding: 0; scroll-snap-align: center; }
</style></head><body><div id="scroller"><div id="item1" class="item"></div><div id="item2" class="item"></div><div id="item3" class="item"></div><div id="item4" class="item"></div></div></body></html>"#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(200, 200))?;
    doc.render_frame()?;

    let scroller_node = doc.dom().get_element_by_id("scroller").expect("scroller node");
    let scroller_box_id = doc
      .principal_box_id_for_node(scroller_node)?
      .expect("scroller box id");

    // Scroll close to the third snap target and paint once so scroll snap commits the snapped state.
    doc.set_element_scroll_offset(scroller_box_id, Point::new(0.0, 70.0));
    doc.paint_from_cache_frame_with_deadline(None)?;
    let snapped_before = doc.element_scroll_offset(scroller_box_id).y;
    assert!(
      (snapped_before - 75.0).abs() < 0.5,
      "expected element scroller to snap to 75px, got {snapped_before}"
    );

    // Mutate layout above the viewport inside the scroller so later snap points shift.
    let item2_node = doc.dom().get_element_by_id("item2").expect("item2 node");
    let changed = doc.mutate_dom(|dom| {
      dom
        .set_attribute(item2_node, "style", "padding-top: 10px")
        .expect("set attribute")
    });
    assert!(changed, "expected style mutation to dirty layout");

    // Flush layout without painting. Scroll anchoring must apply, and snapped scrollers must re-snap.
    doc.ensure_layout()?;
    let snapped_after = doc.element_scroll_offset(scroller_box_id).y;
    assert!(
      (snapped_after - 85.0).abs() < 0.5,
      "expected element scroller to re-snap to 85px, got {snapped_after}"
    );
    Ok(())
  }

  #[test]
  fn scroll_anchoring_resnaps_snapped_viewport_after_layout_change() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html><html><head><style>
html, body { margin: 0; padding: 0; }
html { scroll-snap-type: y mandatory; }
.snap { height: 50px; margin: 0; padding: 0; scroll-snap-align: center; }
</style></head><body><div id="s1" class="snap"></div><div id="s2" class="snap"></div><div id="s3" class="snap"></div><div id="s4" class="snap"></div></body></html>"#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(100, 100))?;
    doc.render_frame()?;

    // Scroll close to the third snap target and paint once so scroll snap commits the snapped state.
    doc.set_scroll(0.0, 70.0);
    doc.paint_from_cache_frame_with_deadline(None)?;
    let snapped_before = doc.viewport_scroll_offset().y;
    assert!(
      (snapped_before - 75.0).abs() < 0.5,
      "expected viewport to snap to 75px, got {snapped_before}"
    );

    // Mutate layout above the viewport so later snap points shift.
    let s2_node = doc.dom().get_element_by_id("s2").expect("s2 node");
    let changed = doc.mutate_dom(|dom| {
      dom
        .set_attribute(s2_node, "style", "padding-top: 10px")
        .expect("set attribute")
    });
    assert!(changed, "expected style mutation to dirty layout");

    // Flush layout without painting. Scroll anchoring must apply, and snapped scrollers must re-snap.
    doc.ensure_layout()?;
    let snapped_after = doc.viewport_scroll_offset().y;
    assert!(
      (snapped_after - 85.0).abs() < 0.5,
      "expected viewport to re-snap to 85px, got {snapped_after}"
    );
    Ok(())
  }

  #[test]
  fn render_if_needed_returns_none_when_clean() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    assert!(doc.render_if_needed().unwrap().is_some());
    assert!(doc.render_if_needed().unwrap().is_none());
  }

  #[test]
  fn invalidate_paint_triggers_repaint_without_layout() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;

    // Prime the layout cache.
    assert!(doc.render_if_needed()?.is_some());
    assert!(
      doc.render_if_needed()?.is_none(),
      "expected render_if_needed to return None when clean"
    );

    let before = doc.invalidation_counters();
    assert!(
      !doc.needs_layout(),
      "expected document to have up-to-date layout after initial render"
    );

    doc.invalidate_paint();
    assert!(
      !doc.needs_layout(),
      "paint-only invalidation should not mark style/layout dirty"
    );

    assert!(
      doc.render_if_needed()?.is_some(),
      "expected paint-only invalidation to trigger a repaint"
    );

    let after = doc.invalidation_counters();
    assert_eq!(
      after, before,
      "paint-only invalidation should reuse cached style/layout artifacts"
    );
    Ok(())
  }

  #[test]
  fn form_state_mutations_trigger_rerender_for_out_of_band_dom2_updates() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = "<!doctype html><html><body><input id=i value=foo></body></html>";
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      html,
      RenderOptions::new().with_viewport(80, 32),
    )?;
    // Prime layout caches.
    doc.render_frame()?;
    assert!(
      doc.render_if_needed()?.is_none(),
      "expected document to be clean after initial render"
    );

    let input = doc.dom().get_element_by_id("i").expect("input element");
    let styled_node_id = doc
      .last_dom_mapping()
      .expect("dom mapping")
      .preorder_for_node_id(input)
      .expect("input preorder id");

    fn input_text_value(root: &BoxNode, styled_node_id: usize) -> Option<String> {
      let mut stack: Vec<&BoxNode> = vec![root];
      while let Some(node) = stack.pop() {
        if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
          if let BoxType::Replaced(replaced) = &node.box_type {
            if let ReplacedType::FormControl(control) = &replaced.replaced_type {
              if let FormControlKind::Text { value, .. } = &control.control {
                return Some(value.clone());
              }
            }
          }
        }
        if let Some(body) = node.footnote_body.as_deref() {
          stack.push(body);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      None
    }

    let value0 = input_text_value(&doc.prepared.as_ref().unwrap().box_tree.root, styled_node_id)
      .expect("missing input form control");
    assert_eq!(value0, "foo");

    // Simulate JS shims mutating the live `dom2::Document` through a raw pointer (bypassing
    // `BrowserDocumentDom2::mutate_dom`).
    let dom_ptr = doc.dom_non_null();
    // SAFETY: `dom_ptr` is valid for the duration of the test and we don't move `doc.dom` while the
    // mutable reference is live.
    unsafe { dom_ptr.as_ptr().as_mut() }
      .expect("dom pointer")
      .set_input_value(input, "bar")
      .expect("set_input_value");

    assert!(
      doc.render_if_needed()?.is_some(),
      "expected out-of-band form state change to invalidate and repaint"
    );
    let value1 = input_text_value(&doc.prepared.as_ref().unwrap().box_tree.root, styled_node_id)
      .expect("missing input form control");
    assert_eq!(value1, "bar");
    Ok(())
  }

  #[test]
  fn hover_interaction_state_triggers_rerender_and_coalesces() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            body { margin: 0; }
            #a { width: 10px; height: 10px; background: rgb(255, 0, 0); }
            #a:hover { background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <div id="a"></div>
        </body>
      </html>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(16, 16))?;

    let pixmap0 = doc.render_frame()?;
    let p0 = pixmap0.pixel(5, 5).expect("pixel 5,5");
    assert_eq!(p0.alpha(), 255);
    assert_eq!(p0.red(), 255);
    assert_eq!(p0.green(), 0);
    assert_eq!(p0.blue(), 0);

    let node_id = doc.dom().get_element_by_id("a").expect("#a element");
    let preorder_id = doc
      .last_dom_mapping()
      .expect("dom mapping")
      .preorder_for_node_id(node_id)
      .expect("preorder id");

    let mut interaction_state = InteractionState::default();
    interaction_state.set_hover_chain(vec![preorder_id]);
    doc.set_interaction_state(Some(interaction_state));

    let pixmap1 = doc
      .render_if_needed()?
      .expect("interaction state should invalidate and repaint");
    let p1 = pixmap1.pixel(5, 5).expect("pixel 5,5");
    assert_eq!(p1.alpha(), 255);
    assert_eq!(p1.red(), 0);
    assert_eq!(p1.green(), 0);
    assert_eq!(p1.blue(), 255);

    assert!(
      doc.render_if_needed()?.is_none(),
      "expected no-op render when interaction state unchanged"
    );

    Ok(())
  }

  #[test]
  fn document_selection_repaints_from_cache_without_layout() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        html, body { margin: 0; background: white; }
        p { margin: 0; font: 20px sans-serif; color: black; }
      </style>
      <p>Hello world</p>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(200, 40))?;

    let base_state = InteractionState::default();
    doc.set_interaction_state(Some(base_state.clone()));
    let pixmap0 = doc.render_frame()?;
    assert_eq!(
      count_document_selection_pixels(&pixmap0),
      0,
      "expected no selection pixels before selection is applied"
    );
    let counters_before = doc.invalidation_counters();

    let mut selected_state = base_state.clone();
    selected_state.set_document_selection(Some(
      crate::interaction::state::DocumentSelectionState::All,
    ));
    doc.set_interaction_state(Some(selected_state));

    let pixmap1 = doc
      .render_if_needed()?
      .expect("expected document selection to invalidate and repaint");
    assert_eq!(
      doc.invalidation_counters(),
      counters_before,
      "expected no style/layout work for document selection repaint"
    );
    assert!(
      count_document_selection_pixels(&pixmap1) > 0,
      "expected selection highlight pixels after selection is applied"
    );
    assert!(
      doc.render_if_needed()?.is_none(),
      "expected no-op render when selection is unchanged"
    );
    Ok(())
  }

  #[test]
  fn form_control_caret_repaints_from_cache_without_layout() -> Result<()> {
    use crate::text::caret::CaretAffinity;

    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        html, body { margin: 0; background: white; }
        input {
          font: 24px monospace;
          caret-color: rgb(255, 0, 0);
          border: 0;
          padding: 0;
          margin: 0;
          background: white;
          color: black;
        }
      </style>
      <input id="a" value="aaaaaaaaaa">
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(320, 40))?;

    // Prime a dom2↔renderer mapping so we can address the input via preorder IDs.
    doc.render_frame()?;
    let input = doc.dom().get_element_by_id("a").expect("#a input");
    let input_preorder = doc
      .last_dom_mapping()
      .expect("dom mapping")
      .preorder_for_node_id(input)
      .expect("preorder id");

    let mut state_end = InteractionState::default();
    state_end.set_focused(Some(input_preorder));
    state_end.set_focus_chain(vec![input_preorder]);
    state_end.set_text_edit(Some(crate::interaction::state::TextEditPaintState {
      node_id: input_preorder,
      caret: 10,
      caret_affinity: CaretAffinity::Downstream,
      selection: None,
    }));
    doc.set_interaction_state(Some(state_end.clone()));

    let pixmap_end = doc
      .render_if_needed()?
      .expect("expected focus/caret state to invalidate and repaint");
    let caret_end = caret_red_x_range(&pixmap_end).expect("expected caret pixels");
    let counters_before = doc.invalidation_counters();

    let mut state_start = state_end.clone();
    if let Some(edit) = state_start.text_edit_mut().as_mut() {
      edit.caret = 0;
    }
    doc.set_interaction_state(Some(state_start));

    let pixmap_start = doc
      .render_if_needed()?
      .expect("expected caret move to invalidate and repaint");
    assert_eq!(
      doc.invalidation_counters(),
      counters_before,
      "expected no style/layout work for caret repaint"
    );

    let caret_start = caret_red_x_range(&pixmap_start).expect("expected caret pixels");
    assert!(
      caret_start.0 + 5 < caret_end.0,
      "expected caret x to move left; start={caret_start:?}, end={caret_end:?}"
    );
    assert!(
      doc.render_if_needed()?.is_none(),
      "expected no-op render when caret is unchanged"
    );
    Ok(())
  }

  #[test]
  fn multiple_dom_mutations_coalesce_into_single_layout() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    // First render clears dirty flags.
    doc.render_frame().unwrap();

    {
      let dom = doc.dom_mut();
      if let Some(text_id) = first_text_node_id(dom) {
        let node = dom.node_mut(text_id);
        if let crate::dom2::NodeKind::Text { content } = &mut node.kind {
          content.clear();
          content.push_str("first");
        }
      }
    }

    {
      let dom = doc.dom_mut();
      if let Some(text_id) = first_text_node_id(dom) {
        let node = dom.node_mut(text_id);
        if let crate::dom2::NodeKind::Text { content } = &mut node.kind {
          content.clear();
          content.push_str("second");
        }
      }
    }

    assert!(doc.render_if_needed().unwrap().is_some());
    assert!(doc.render_if_needed().unwrap().is_none());
  }

  #[test]
  fn text_mutation_uses_incremental_relayout() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;
    doc.render_frame()?;
    let before = doc.invalidation_counters();
    assert_eq!(before.incremental_relayouts, 0);
    assert_eq!(before.full_restyles, 1);
    assert_eq!(before.full_relayouts, 1);

    let text_id = first_text_node_id(doc.dom()).expect("text node");
    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_id, "Updated").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);
    assert_eq!(after.full_restyles, before.full_restyles);
    assert_eq!(after.full_relayouts, before.full_relayouts);
    Ok(())
  }

  #[test]
  fn raw_pointer_text_mutation_uses_incremental_relayout() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;
    doc.render_frame()?;
    let before = doc.invalidation_counters();
    assert_eq!(before.incremental_relayouts, 0);
    assert_eq!(before.full_restyles, 1);
    assert_eq!(before.full_relayouts, 1);

    let text_id = first_text_node_id(doc.dom()).expect("text node");
    let mut dom_ptr = doc.dom_non_null();
    unsafe {
      dom_ptr
        .as_mut()
        .set_text_data(text_id, "Updated")
        .expect("set text via raw pointer");
    }

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);
    assert_eq!(after.full_restyles, before.full_restyles);
    assert_eq!(after.full_relayouts, before.full_relayouts);
    Ok(())
  }

  #[test]
  fn raw_pointer_text_mutation_is_applied_even_when_layout_already_dirty() -> Result<()> {
    let renderer = renderer_for_tests();
    let html =
      "<!doctype html><html><body><div id=\"a\">One</div><div id=\"b\">Two</div></body></html>";
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(64, 64))?;
    doc.render_frame()?;

    let a = doc.dom().get_element_by_id("a").expect("<div id=a>");
    let b = doc.dom().get_element_by_id("b").expect("<div id=b>");
    let text_a = first_child_text_node_id(doc.dom(), a).expect("text node a");
    let text_b = first_child_text_node_id(doc.dom(), b).expect("text node b");

    // First change is host-classified (via mutate_dom).
    assert!(doc.mutate_dom(|dom| dom.set_text_data(text_a, "Uno").expect("set text a")));

    // Second change bypasses BrowserDocumentDom2 invalidation plumbing (raw pointer).
    let mut dom_ptr = doc.dom_non_null();
    unsafe {
      dom_ptr
        .as_mut()
        .set_text_data(text_b, "Dos")
        .expect("set text b via raw pointer");
    }

    // Rendering should incorporate *both* text mutations. If the raw-pointer mutation log is not
    // drained when `layout_dirty` is already set, the prepared box tree would still contain "Two".
    doc.render_frame()?;
    let prepared = doc.prepared().expect("prepared");

    fn collect_box_tree_text(root: &BoxNode) -> Vec<String> {
      let mut out = Vec::new();
      let mut stack: Vec<&BoxNode> = vec![root];
      while let Some(node) = stack.pop() {
        if let BoxType::Text(text_box) = &node.box_type {
          out.push(text_box.text.clone());
        }
        if let Some(body) = node.footnote_body.as_deref() {
          stack.push(body);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      out
    }

    let texts = collect_box_tree_text(&prepared.box_tree().root);
    assert!(
      texts.iter().any(|text| text == "Uno"),
      "expected box tree to include updated text 'Uno', got: {texts:?}"
    );
    assert!(
      texts.iter().any(|text| text == "Dos"),
      "expected box tree to include updated text 'Dos', got: {texts:?}"
    );
    Ok(())
  }

  #[test]
  fn dom_query_and_hit_testing_layout_flushes_stay_in_sync() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = "<!doctype html><html><body style=\"margin:0;padding:0;\"><div id=\"target\" style=\"width:10px;height:10px;background:black;\">Hello</div></body></html>";
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(64, 64))?;

    // Seed a prepared cache + mapping.
    doc.render_frame()?;
    let target = doc.dom().get_element_by_id("target").expect("#target element");
    let text_id = first_text_node_id(doc.dom()).expect("text node");

    let start = doc.invalidation_counters();
    assert_eq!(start.full_restyles, 1);
    assert_eq!(start.full_relayouts, 1);

    // 1) DOM query flush after a text mutation should use incremental relayout, and a subsequent
    // hit-testing flush should observe the same up-to-date layout without re-running pipeline work.
    assert!(doc.mutate_dom(|dom| dom.set_text_data(text_id, "Updated").expect("set text")));
    doc.ensure_layout_for_dom_queries()?;
    let after_dom_flush = doc.invalidation_counters();
    assert_eq!(after_dom_flush.incremental_relayouts, start.incremental_relayouts + 1);
    assert_eq!(after_dom_flush.full_restyles, start.full_restyles);
    assert_eq!(after_dom_flush.full_relayouts, start.full_relayouts);

    let hit = doc.element_from_point(1.0, 1.0)?;
    assert_eq!(hit, Some(target));
    assert_eq!(
      doc.invalidation_counters(),
      after_dom_flush,
      "hit-testing flush should not redo layout when a DOM-query flush already updated caches"
    );

    // 2) Hit-testing flush should also satisfy layout after text mutations, and a subsequent DOM
    // query flush should be a no-op with respect to layout work.
    assert!(doc.mutate_dom(|dom| {
      dom
        .set_text_data(text_id, "Updated again")
        .expect("set text")
    }));
    let before_hit_flush = doc.invalidation_counters();
    let hit2 = doc.element_from_point(1.0, 1.0)?;
    assert_eq!(hit2, Some(target));
    let after_hit_flush = doc.invalidation_counters();
    assert_eq!(
      after_hit_flush.incremental_relayouts,
      before_hit_flush.incremental_relayouts + 1
    );
    assert_eq!(after_hit_flush.full_restyles, before_hit_flush.full_restyles);
    assert_eq!(after_hit_flush.full_relayouts, before_hit_flush.full_relayouts);

    doc.ensure_layout_for_dom_queries()?;
    assert_eq!(
      doc.invalidation_counters(),
      after_hit_flush,
      "DOM-query flush should not redo layout when hit-testing already flushed layout"
    );
    Ok(())
  }

  #[test]
  fn form_state_mutation_triggers_rerender_and_updates_form_controls() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = "<!doctype html><html><body>\
      <input id=\"i\" value=\"initial\">\
      <input id=\"c\" type=\"checkbox\">\
      <textarea id=\"t\">default</textarea>\
      <select id=\"s\">\
        <option value=\"a\">A</option>\
        <option value=\"b\">B</option>\
      </select>\
    </body></html>";
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(128, 64))?;
    doc.render_frame()?;
    assert!(
      doc.render_if_needed()?.is_none(),
      "expected document to be clean after initial render"
    );

    let input = doc.dom().get_element_by_id("i").expect("<input id=i>");
    let checkbox = doc.dom().get_element_by_id("c").expect("<input id=c>");
    let textarea = doc.dom().get_element_by_id("t").expect("<textarea id=t>");
    let select = doc.dom().get_element_by_id("s").expect("<select id=s>");

    let changed = doc.mutate_dom(|dom| {
      dom.set_input_value(input, "updated").expect("set input value");
      dom
        .set_input_checked(checkbox, true)
        .expect("set checkbox checked");
      dom
        .set_textarea_value(textarea, "edited")
        .expect("set textarea value");
      dom
        .set_select_selected_index(select, 1)
        .expect("set select selected index");
      true
    });
    assert!(changed);

    assert!(
      doc.render_if_needed()?.is_some(),
      "expected form state mutation to invalidate the renderer"
    );

    let prepared = doc.prepared().expect("prepared");
    assert_eq!(
      find_first_text_input_value(&prepared.box_tree().root).expect("text input value"),
      "updated"
    );
    assert_eq!(
      find_first_checkbox_checked(&prepared.box_tree().root).expect("checkbox checkedness"),
      true
    );
    assert_eq!(
      find_first_textarea_control_value(&prepared.box_tree().root).expect("textarea value"),
      "edited"
    );

    let select_control =
      find_first_select_control(&prepared.box_tree().root).expect("select control");
    assert_eq!(select_control.selected.len(), 1);
    let selected_idx = select_control.selected[0];
    match select_control.items.get(selected_idx) {
      Some(SelectItem::Option { value, selected, .. }) => {
        assert_eq!(value, "b");
        assert!(*selected);
      }
      other => panic!("expected selected option at index {selected_idx}, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn hidden_input_value_mutation_does_not_invalidate_renderer() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = "<!doctype html><html><body>\
      <input id=\"h\" TYPE=\"HIDDEN\" value=\"initial\">\
    </body></html>";
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(128, 64))?;
    doc.render_frame()?;
    assert!(
      doc.render_if_needed()?.is_none(),
      "expected document to be clean after initial render"
    );

    let hidden = doc.dom().get_element_by_id("h").expect("<input id=h>");
    let changed = doc.mutate_dom(|dom| {
      dom
        .set_input_value(hidden, "updated")
        .expect("set hidden input value");
      true
    });
    assert!(changed);

    assert!(
      doc.render_if_needed()?.is_none(),
      "hidden input value changes should not invalidate rendering"
    );
    Ok(())
  }

  fn first_child_text_node_id(
    doc: &crate::dom2::Document,
    parent: crate::dom2::NodeId,
  ) -> Option<crate::dom2::NodeId> {
    doc
      .node(parent)
      .children
      .iter()
      .copied()
      .find(|&child| matches!(doc.node(child).kind, crate::dom2::NodeKind::Text { .. }))
  }

  fn find_select_control<'a>(
    root: &'a BoxNode,
  ) -> Option<&'a crate::tree::box_tree::SelectControl> {
    let mut stack: Vec<&BoxNode> = vec![root];
    while let Some(node) = stack.pop() {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let crate::tree::box_tree::ReplacedType::FormControl(control) = &replaced.replaced_type {
          if let crate::tree::box_tree::FormControlKind::Select(select) = &control.control {
            return Some(select);
          }
        }
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn find_textarea_value<'a>(root: &'a BoxNode) -> Option<&'a str> {
    let mut stack: Vec<&BoxNode> = vec![root];
    while let Some(node) = stack.pop() {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let crate::tree::box_tree::ReplacedType::FormControl(control) = &replaced.replaced_type {
          if let crate::tree::box_tree::FormControlKind::TextArea { value, .. } = &control.control
          {
            return Some(value.as_str());
          }
        }
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  #[test]
  fn option_text_mutation_falls_back_to_full_pipeline_and_updates_select_model() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = "<!doctype html><html><body>\
      <select id=\"s\">\
        <option id=\"o\">One</option>\
        <option>Two</option>\
      </select>\
    </body></html>";
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(32, 32))?;

    doc.render_frame()?;
    {
      let prepared = doc.prepared().expect("prepared");
      let select = find_select_control(&prepared.box_tree().root).expect("select control");
      let first = select.items.first().expect("select option 0");
      match first {
        crate::tree::box_tree::SelectItem::Option { label, .. } => assert_eq!(label, "One"),
        other => panic!("expected SelectItem::Option, got {other:?}"),
      }
    }

    let before = doc.invalidation_counters();

    let option = doc.dom().get_element_by_id("o").expect("<option id=o>");
    let text_id = first_child_text_node_id(doc.dom(), option).expect("option text node");
    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_id, "Uno").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(
      after.incremental_relayouts, before.incremental_relayouts,
      "option text changes should fall back to full pipeline (no incremental relayout)"
    );
    assert_eq!(after.full_restyles, before.full_restyles + 1);
    assert_eq!(after.full_relayouts, before.full_relayouts + 1);

    let prepared = doc.prepared().expect("prepared");
    let select = find_select_control(&prepared.box_tree().root).expect("select control");
    let first = select.items.first().expect("select option 0");
    match first {
      crate::tree::box_tree::SelectItem::Option { label, .. } => assert_eq!(label, "Uno"),
      other => panic!("expected SelectItem::Option, got {other:?}"),
    }
    Ok(())
  }

  #[test]
  fn textarea_text_mutation_falls_back_to_full_pipeline_and_updates_control_value() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = "<!doctype html><html><body><textarea id=\"t\">hello</textarea></body></html>";
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(32, 32))?;

    doc.render_frame()?;
    assert_eq!(
      find_textarea_value(&doc.prepared().expect("prepared").box_tree().root).expect("textarea"),
      "hello"
    );

    let before = doc.invalidation_counters();

    let textarea = doc.dom().get_element_by_id("t").expect("<textarea id=t>");
    let text_id = first_child_text_node_id(doc.dom(), textarea).expect("textarea text node");
    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_id, "updated").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(
      after.incremental_relayouts, before.incremental_relayouts,
      "textarea text changes should fall back to full pipeline (no incremental relayout)"
    );
    assert_eq!(after.full_restyles, before.full_restyles + 1);
    assert_eq!(after.full_relayouts, before.full_relayouts + 1);
    assert_eq!(
      find_textarea_value(&doc.prepared().expect("prepared").box_tree().root).expect("textarea"),
      "updated"
    );

    Ok(())
  }

  #[test]
  fn incremental_restyle_scope_includes_wbr_synthetic_zwsp_text_nodes() -> Result<()> {
    use crate::style::color::Rgba;

    // Parse directly into dom2 so `<wbr>` remains a void element with no text children. The renderer
    // snapshot taken during layout injects an implicit ZWSP text node, which is then "synthetic"
    // from the dom2 perspective and maps back to the `<wbr>` element `NodeId`.
    let html = concat!(
      "<!doctype html>",
      "<html><head><style>#p { color: rgb(255,0,0); }</style></head>",
      "<body><p id=p>Hi<wbr>There</p></body></html>",
    );

    let renderer = renderer_for_tests();
    let dom = renderer.parse_html_dom2(html)?;

    let options = RenderOptions::new().with_viewport(64, 64);
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><head></head><body></body></html>",
      options.clone(),
    )?;
    doc.reset_with_dom(dom, options.clone());

    doc.render_frame()?;
    let prepared = doc.prepared.as_ref().expect("prepared layout");
    let mapping = doc.last_dom_mapping.as_ref().expect("dom mapping");

    // Find the styled text node injected for `<wbr>` (ZWSP, U+200B).
    let mut zwsp_preorders: Vec<usize> = Vec::new();
    let mut stack: Vec<&StyledNode> = vec![prepared.styled_tree()];
    while let Some(node) = stack.pop() {
      if let crate::dom::DomNodeType::Text { content } = &node.node.node_type {
        if content == "\u{200B}" {
          zwsp_preorders.push(node.node_id);
        }
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    assert_eq!(
      zwsp_preorders.len(),
      1,
      "expected exactly one synthetic ZWSP text node"
    );
    let zwsp_preorder = zwsp_preorders[0];

    let mapped_dom2_id = mapping
      .node_id_for_preorder(zwsp_preorder)
      .expect("ZWSP preorder should map back to a dom2 node");
    match &doc.dom().node(mapped_dom2_id).kind {
      crate::dom2::NodeKind::Element { tag_name, .. } => {
        assert!(tag_name.eq_ignore_ascii_case("wbr"));
      }
      other => panic!("expected ZWSP preorder to map to a <wbr> element, got {other:?}"),
    }
    let wbr_preorder = mapping
      .preorder_for_node_id(mapped_dom2_id)
      .expect("<wbr> should have a renderer preorder id");
    assert_ne!(
      wbr_preorder, zwsp_preorder,
      "expected ZWSP preorder to be synthetic (map to <wbr> but not round-trip)"
    );

    let before_color = styled_tree_style_for_preorder_id(prepared.styled_tree(), zwsp_preorder)
      .expect("ZWSP style")
      .color;
    assert_eq!(before_color, Rgba::rgb(255, 0, 0));

    let p = doc.dom().get_element_by_id("p").expect("<p id=p>");
    let changed = doc.mutate_dom(|dom| {
      dom
        .set_attribute(p, "style", "color: rgb(0,255,0)")
        .expect("set p[style]")
    });
    assert!(changed);

    doc.render_frame()?;
    let prepared_after = doc.prepared.as_ref().expect("prepared layout after mutation");

    let after_color =
      styled_tree_style_for_preorder_id(prepared_after.styled_tree(), zwsp_preorder)
        .expect("ZWSP style after mutation")
        .color;
    assert_eq!(
      after_color,
      Rgba::rgb(0, 255, 0),
      "synthetic ZWSP text node should inherit updated color after incremental restyle"
    );

    Ok(())
  }

  #[test]
  fn textarea_text_mutation_updates_form_control_model_without_full_restyle() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><head><style>\
        @keyframes fade { from { opacity: 0; } to { opacity: 1; } }\
        #box { width: 1px; height: 1px; animation: fade 1s linear infinite; }\
      </style></head><body><div id=box></div><textarea id=t>Hello</textarea></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;
    doc.render_frame()?;
    assert!(
      doc
        .prepared()
        .expect("prepared")
        .fragment_tree
        .keyframes
        .contains_key("fade"),
      "expected @keyframes fade to be preserved on the initial fragment tree"
    );
    let before = doc.invalidation_counters();

    let textarea = doc.dom().get_element_by_id("t").expect("textarea element");
    let text_id = first_text_child(doc.dom(), textarea).expect("textarea text node");
    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_id, "Updated").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.full_restyles, before.full_restyles);
    assert_eq!(after.full_relayouts, before.full_relayouts);
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);

    let prepared = doc.prepared().expect("prepared");
    assert!(
      prepared.fragment_tree.keyframes.contains_key("fade"),
      "incremental relayout should preserve fragment-tree keyframes metadata"
    );
    let value = find_first_textarea_control_value(&prepared.box_tree().root)
      .expect("textarea form control value");
    assert_eq!(value, "Updated");
    Ok(())
  }

  #[test]
  fn select_option_label_text_mutation_updates_select_items_without_full_restyle() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><select id=s><option id=o>One</option></select></body></html>",
      RenderOptions::new().with_viewport(64, 64),
    )?;
    doc.render_frame()?;
    let before = doc.invalidation_counters();

    let option = doc.dom().get_element_by_id("o").expect("option element");
    let option_preorder = doc
      .last_dom_mapping()
      .and_then(|mapping| mapping.preorder_for_node_id(option))
      .expect("option preorder id");
    let text_id = first_text_child(doc.dom(), option).expect("option text node");
    let changed =
      doc.mutate_dom(|dom| dom.set_text_data(text_id, "Updated").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.full_restyles, before.full_restyles);
    assert_eq!(after.full_relayouts, before.full_relayouts);
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);

    let prepared = doc.prepared().expect("prepared");
    let select =
      find_first_select_control(&prepared.box_tree().root).expect("select form control");
    let updated_label = select.items.iter().find_map(|item| match item {
      SelectItem::Option { node_id, label, .. } if *node_id == option_preorder => Some(label.as_str()),
      _ => None,
    });
    assert_eq!(updated_label, Some("Updated"));
    Ok(())
  }

  #[test]
  fn slot_assign_records_composed_tree_mutation_invalidation() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body>\
        <div id=\"host\">\
          <span id=\"a\">A</span>\
          <span id=\"b\">B</span>\
        </div>\
      </body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    let (shadow_root, slot, a, b) = {
      let dom = doc.dom_mut();
      let host = dom.get_element_by_id("host").expect("host");
      let a = dom.get_element_by_id("a").expect("slottable a");
      let b = dom.get_element_by_id("b").expect("slottable b");
      let shadow_root = dom
        .attach_shadow_root(
          host,
          crate::dom::ShadowRootMode::Open,
          /* clonable */ false,
          /* serializable */ false,
          /* delegates_focus */ false,
          crate::dom2::SlotAssignmentMode::Manual,
        )
        .expect("attach shadow root");
      let slot = dom.create_element("slot", "");
      dom.append_child(shadow_root, slot).expect("append slot");
      (shadow_root, slot, a, b)
    };

    // Clear dirtiness from `dom_mut` setup.
    doc.render_frame().expect("render");

    let changed = doc.mutate_dom(|dom| {
      dom.slot_assign(slot, &[a, b]).expect("slot assign");
      true
    });
    assert!(changed);

    // Without structured mutation logging, `BrowserDocumentDom2::mutate_dom` would fall back to
    // `invalidate_all()`, clearing per-node dirty sets. We expect a structured invalidation marker.
    assert!(
      doc.dirty_structure_nodes.contains(&shadow_root),
      "slot_assign should mark the shadow root as structurally dirty via composed-tree mutation log"
    );
    assert!(
      !doc.dirty_structure_nodes.is_empty(),
      "slot_assign should not fall back to unstructured invalidate_all()"
    );
    assert!(doc.dirty_style_nodes.is_empty());
    assert!(doc.dirty_text_nodes.is_empty());
  }

  #[test]
  fn dom_mut_structural_changes_do_not_poison_future_incremental_invalidation() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;
    doc.render_frame()?;

    let text_id = first_text_node_id(doc.dom()).expect("text node");

    // Mutate via `dom_mut()` so the recorded `MutationLog` entries are not consumed by
    // `BrowserDocumentDom2::apply_mutation_log`. A subsequent full pipeline run should clear the
    // stale mutation log so later incremental invalidation isn't forced into a full restyle.
    {
      let dom = doc.dom_mut();
      let body = dom.body().expect("body element");
      let child = dom.create_element("span", "");
      dom.append_child(body, child).expect("append child");
    }

    // Structural changes force a full pipeline.
    doc.render_frame()?;
    let after_structural = doc.invalidation_counters();

    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_id, "Updated").expect("set text"));
    assert!(changed);

    // Pure text changes should use incremental relayout and must not trigger another full restyle.
    doc.render_frame()?;
    let after_text = doc.invalidation_counters();
    assert_eq!(
      after_text.incremental_relayouts,
      after_structural.incremental_relayouts + 1
    );
    assert_eq!(after_text.full_restyles, after_structural.full_restyles);
    assert_eq!(after_text.full_relayouts, after_structural.full_relayouts);

    Ok(())
  }

  #[test]
  fn child_insertion_disables_incremental_restyle_reuse() -> Result<()> {
    use crate::debug::runtime::RuntimeToggles;
    use std::collections::HashMap;

    let renderer = renderer_for_tests();
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_DOM2_INCREMENTAL_RESTYLE_REUSE".to_string(),
      "1".to_string(),
    )]));

    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            #target { color: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="target">Hi</div>
        </body>
      </html>
    "#;

    let mut doc = BrowserDocumentDom2::new(
      renderer,
      html,
      RenderOptions::new()
        .with_viewport(32, 32)
        .with_runtime_toggles(toggles),
    )?;

    doc.render_frame()?;
    let before = doc.invalidation_counters();
    assert_eq!(before.full_restyles, 1);
    assert_eq!(before.incremental_restyles, 0);

    let target = doc.dom().get_element_by_id("target").expect("#target");
    let changed = doc.mutate_dom(|dom| dom.set_attribute(target, "class", "changed").expect("set attribute"));
    assert!(changed);

    doc.render_frame()?;
    let after_attr = doc.invalidation_counters();
    assert_eq!(after_attr.full_restyles, before.full_restyles);
    assert_eq!(after_attr.incremental_restyles, before.incremental_restyles + 1);

    // Structural mutation (child insertion) shifts renderer preorder ids; incremental restyle reuse
    // must be disabled to avoid misaligned node-id reuse.
    let body = doc.dom().body().expect("body");
    let changed = doc.mutate_dom(|dom| {
      let child = dom.create_element("div", "");
      dom.append_child(body, child).expect("append child")
    });
    assert!(changed);

    doc.render_frame()?;
    let after_insert = doc.invalidation_counters();
    assert_eq!(after_insert.full_restyles, after_attr.full_restyles + 1);
    assert_eq!(after_insert.incremental_restyles, after_attr.incremental_restyles);

    Ok(())
  }

  #[test]
  fn slot_name_change_updates_slot_distribution_with_incremental_restyle_reuse() -> Result<()> {
    use crate::debug::runtime::RuntimeToggles;
    use std::collections::HashMap;

    let renderer = renderer_for_tests();
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_DOM2_INCREMENTAL_RESTYLE_REUSE".to_string(),
      "1".to_string(),
    )]));

    let html = r#"<!doctype html>
      <html>
        <body>
          <div id=host>
            <template shadowroot=open>
              <slot id=slot name=x></slot>
            </template>
            <span id=slotted slot=x>Hi</span>
          </div>
        </body>
      </html>
    "#;

    let mut doc = BrowserDocumentDom2::new(
      renderer,
      html,
      RenderOptions::new()
        .with_viewport(32, 32)
        .with_runtime_toggles(toggles),
    )?;

    doc.render_frame()?;
    let before = doc.invalidation_counters();
    assert_eq!(before.full_restyles, 1);
    assert_eq!(before.incremental_restyles, 0);

    let (slot, slotted) = {
      let dom = doc.dom();
      let host = dom.get_element_by_id("host").expect("#host");
      let shadow_root = dom.shadow_root_for_host(host).expect("shadow root");
      let slot = dom
        .get_element_by_id_from(shadow_root, "slot")
        .expect("slot element");
      let slotted = dom.get_element_by_id("slotted").expect("#slotted");
      (slot, slotted)
    };

    assert!(
      doc.principal_box_id_for_node(slotted)?.is_some(),
      "slotted node should be rendered before slot name change"
    );

    let changed =
      doc.mutate_dom(|dom| dom.set_attribute(slot, "name", "y").expect("set attribute"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.full_restyles, before.full_restyles);
    assert_eq!(after.incremental_restyles, before.incremental_restyles + 1);
    assert!(
      doc.principal_box_id_for_node(slotted)?.is_none(),
      "slotted node should not be rendered after slot name change"
    );

    Ok(())
  }

  #[test]
  fn slot_name_change_can_cause_slotted_node_to_start_rendering_with_incremental_restyle_reuse() -> Result<()> {
    use crate::debug::runtime::RuntimeToggles;
    use std::collections::HashMap;

    let renderer = renderer_for_tests();
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_DOM2_INCREMENTAL_RESTYLE_REUSE".to_string(),
      "1".to_string(),
    )]));

    let html = r#"<!doctype html>
      <html>
        <body>
          <div id=host>
            <template shadowroot=open>
              <style>
                ::slotted(#slotted) { display: block !important; }
              </style>
              <slot id=slot name=x></slot>
            </template>
            <span id=slotted slot=y style="display:none">Hi</span>
          </div>
        </body>
      </html>
    "#;

    let mut doc = BrowserDocumentDom2::new(
      renderer,
      html,
      RenderOptions::new()
        .with_viewport(32, 32)
        .with_runtime_toggles(toggles),
    )?;

    doc.render_frame()?;
    let before = doc.invalidation_counters();
    assert_eq!(before.full_restyles, 1);
    assert_eq!(before.incremental_restyles, 0);

    let (slot, slotted) = {
      let dom = doc.dom();
      let host = dom.get_element_by_id("host").expect("#host");
      let shadow_root = dom.shadow_root_for_host(host).expect("shadow root");
      let slot = dom
        .get_element_by_id_from(shadow_root, "slot")
        .expect("slot element");
      let slotted = dom.get_element_by_id("slotted").expect("#slotted");
      (slot, slotted)
    };

    assert!(
      doc.principal_box_id_for_node(slotted)?.is_none(),
      "slotted node should not be rendered when unassigned"
    );

    let changed =
      doc.mutate_dom(|dom| dom.set_attribute(slot, "name", "y").expect("set attribute"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.full_restyles, before.full_restyles);
    assert_eq!(after.incremental_restyles, before.incremental_restyles + 1);
    assert!(
      doc.principal_box_id_for_node(slotted)?.is_some(),
      "slotted node should be rendered after slot name change assigns it"
    );

    Ok(())
  }

  #[test]
  fn has_selectors_disable_incremental_restyle_reuse() -> Result<()> {
    use crate::debug::runtime::RuntimeToggles;
    use std::collections::HashMap;

    let renderer = renderer_for_tests();
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_DOM2_INCREMENTAL_RESTYLE_REUSE".to_string(),
      "1".to_string(),
    )]));

    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            #container { background: rgb(255, 255, 255); }
            /* Reverse dependency: ancestor style depends on a descendant. */
            #container:has(#child[data-state="on"]) { background: rgb(0, 0, 0); }
          </style>
        </head>
        <body>
          <div id="container">
            <div id="child"></div>
          </div>
        </body>
      </html>
    "#;

    let mut doc = BrowserDocumentDom2::new(
      renderer,
      html,
      RenderOptions::new()
        .with_viewport(32, 32)
        .with_runtime_toggles(toggles),
    )?;

    doc.render_frame()?;
    let before = doc.invalidation_counters();
    assert_eq!(before.full_restyles, 1);
    assert_eq!(before.incremental_restyles, 0);

    let child = doc.dom().get_element_by_id("child").expect("#child");
    let changed =
      doc.mutate_dom(|dom| dom.set_attribute(child, "data-state", "on").expect("set attribute"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(
      after.full_restyles,
      before.full_restyles + 1,
      "attribute mutation must trigger a full restyle when :has() is present"
    );
    assert_eq!(after.incremental_restyles, before.incremental_restyles);

    Ok(())
  }

  #[test]
  fn form_state_mutation_uses_incremental_restyle_reuse_when_enabled() -> Result<()> {
    use crate::debug::runtime::RuntimeToggles;
    use std::collections::HashMap;

    let renderer = renderer_for_tests();
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_DOM2_INCREMENTAL_RESTYLE_REUSE".to_string(),
      "1".to_string(),
    )]));

    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; background: rgb(255, 255, 255); }
            /* Keep the checkbox out of layout so the colored box is anchored at (0,0). */
            input { display: none; }
            #box { width: 10px; height: 10px; background: rgb(255, 0, 0); }
            input:checked + #box { background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <input id="i" type="checkbox">
          <div id="box"></div>
        </body>
      </html>
    "#;

    let mut doc = BrowserDocumentDom2::new(
      renderer,
      html,
      RenderOptions::new()
        .with_viewport(16, 16)
        .with_runtime_toggles(toggles),
    )?;

    let pixmap0 = doc.render_frame()?;
    let p0 = pixmap0.pixel(5, 5).expect("pixel 5,5");
    assert_eq!(p0.alpha(), 255);
    assert_eq!(p0.red(), 255);
    assert_eq!(p0.green(), 0);
    assert_eq!(p0.blue(), 0);

    let before = doc.invalidation_counters();
    assert_eq!(before.full_restyles, 1);
    assert_eq!(before.incremental_restyles, 0);

    let input = doc.dom().get_element_by_id("i").expect("input element");
    let changed = doc.mutate_dom(|dom| {
      dom
        .set_input_checked(input, true)
        .expect("set_input_checked");
      true
    });
    assert!(changed);

    let pixmap1 = doc.render_frame()?;
    let p1 = pixmap1.pixel(5, 5).expect("pixel 5,5 after checked");
    assert_eq!(p1.alpha(), 255);
    assert_eq!(p1.red(), 0);
    assert_eq!(p1.green(), 0);
    assert_eq!(p1.blue(), 255);

    let after = doc.invalidation_counters();
    assert_eq!(
      after.full_restyles, before.full_restyles,
      "form-state-only changes should avoid full-document restyle when reuse is enabled"
    );
    assert_eq!(after.incremental_restyles, before.incremental_restyles + 1);
    Ok(())
  }

  #[test]
  fn form_state_mutation_invalidates_layout_without_full_restyle() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><input id=i value=foo></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;
    doc.render_frame()?;

    let input = doc.dom().get_element_by_id("i").expect("input element");
    let changed = doc.mutate_dom(|dom| {
      dom.set_input_value(input, "bar").expect("set input value");
      true
    });
    assert!(changed);

    // Form state changes should invalidate at least layout+paint so the next render can rebuild
    // form control models, but must not force a full restyle (no stylesheet-affecting mutation).
    assert!(!doc.style_dirty);
    assert!(doc.layout_dirty);
    assert!(doc.paint_dirty);
    Ok(())
  }

  #[test]
  fn form_state_mutation_coalesces_with_text_changes_correctly() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = "<!doctype html><html><body><input id=i value=foo><div id=d>Hello</div></body></html>";
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      html,
      RenderOptions::new().with_viewport(80, 32),
    )?;
    doc.render_frame()?;

    let input = doc.dom().get_element_by_id("i").expect("input element");
    let div = doc.dom().get_element_by_id("d").expect("div element");
    let text_id = first_text_child(doc.dom(), div).expect("div text node");

    let changed = doc.mutate_dom(|dom| {
      dom.set_input_value(input, "bar").expect("set_input_value");
      dom.set_text_data(text_id, "World").expect("set_text_data");
      true
    });
    assert!(changed);

    doc.render_frame()?;
    let mapping = doc.last_dom_mapping().expect("dom mapping");
    let styled_node_id = mapping.preorder_for_node_id(input).expect("input preorder id");

    fn input_text_value(root: &BoxNode, styled_node_id: usize) -> Option<String> {
      let mut stack: Vec<&BoxNode> = vec![root];
      while let Some(node) = stack.pop() {
        if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
          if let BoxType::Replaced(replaced) = &node.box_type {
            if let ReplacedType::FormControl(control) = &replaced.replaced_type {
              if let FormControlKind::Text { value, .. } = &control.control {
                return Some(value.clone());
              }
            }
          }
        }
        if let Some(body) = node.footnote_body.as_deref() {
          stack.push(body);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      None
    }

    let prepared = doc.prepared.as_ref().expect("prepared");
    let value =
      input_text_value(&prepared.box_tree.root, styled_node_id).expect("input control value");
    assert_eq!(value, "bar");
    Ok(())
  }

  #[test]
  fn attribute_mutation_uses_incremental_restyle() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #target { width: 10px; height: 10px; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="target"></div>
        </body>
      </html>
    "#;
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(20, 20))?;

    let pixmap0 = doc.render_frame()?;
    let before = doc.invalidation_counters();
    assert_eq!(before.incremental_restyles, 0);
    assert_eq!(before.full_restyles, 1);
    assert_eq!(before.full_relayouts, 1);
    let c0 = pixmap0.pixel(5, 5).expect("pixel 5,5");
    assert_eq!((c0.red(), c0.green(), c0.blue(), c0.alpha()), (255, 0, 0, 255));

    let target = doc.dom().get_element_by_id("target").expect("#target");
    let changed = doc.mutate_dom(|dom| {
      dom
        .set_attribute(target, "style", "background: rgb(0, 255, 0);")
        .expect("set attribute")
    });
    assert!(changed);

    let pixmap1 = doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_restyles, before.incremental_restyles + 1);
    assert_eq!(after.full_restyles, before.full_restyles);
    assert_eq!(after.full_relayouts, before.full_relayouts + 1);
    let c1 = pixmap1.pixel(5, 5).expect("pixel 5,5");
    assert_eq!((c1.red(), c1.green(), c1.blue(), c1.alpha()), (0, 255, 0, 255));
    Ok(())
  }

  #[test]
  fn incremental_restyle_disabled_for_has() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            div:has(span) { outline: 1px solid red; }
            #target { width: 10px; height: 10px; background: rgb(255, 0, 0); }
            #target.blue { background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <div id="target"></div>
        </body>
      </html>
    "#;
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(20, 20))?;
    doc.render_frame()?;
    let before = doc.invalidation_counters();
    assert_eq!(before.incremental_restyles, 0);
    assert_eq!(before.full_restyles, 1);

    let target = doc.dom().get_element_by_id("target").expect("#target");
    let changed =
      doc.mutate_dom(|dom| dom.set_attribute(target, "class", "blue").expect("set attribute"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_restyles, before.incremental_restyles);
    assert_eq!(after.full_restyles, before.full_restyles + 1);
    assert_eq!(after.full_relayouts, before.full_relayouts + 1);
    Ok(())
  }

  #[test]
  fn incremental_relayout_preserves_keyframes_metadata() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        html, body { margin: 0; background: white; }
        #box {
          width: 10px;
          height: 10px;
          background: black;
          animation: fade 1000ms linear infinite;
        }
        @keyframes fade {
          from { opacity: 0; }
          to { opacity: 1; }
        }
      </style>
      <div id="box"></div>
      <p id="text">Hello</p>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(32, 32))?;
    doc.render_frame()?;

    let prepared = doc.prepared.as_ref().expect("prepared layout");
    assert!(
      prepared.fragment_tree.keyframes.contains_key("fade"),
      "expected @keyframes fade to be stored on the fragment tree"
    );

    let before = doc.invalidation_counters();
    let p = doc.dom().get_element_by_id("text").expect("p#text element");
    let text_node = doc
      .dom()
      .node(p)
      .children
      .iter()
      .copied()
      .find(|child| matches!(doc.dom().node(*child).kind, crate::dom2::NodeKind::Text { .. }))
      .expect("text child node");

    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_node, "Updated").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);

    let prepared = doc.prepared.as_ref().expect("prepared layout");
    assert!(
      prepared.fragment_tree.keyframes.contains_key("fade"),
      "incremental relayout should preserve fragment-tree keyframes metadata"
    );
    Ok(())
  }

  #[test]
  fn incremental_relayout_form_control_text_preserves_keyframes_metadata() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        html, body { margin: 0; background: white; }
        #box {
          width: 10px;
          height: 10px;
          background: black;
          animation: fade 1000ms linear infinite;
        }
        @keyframes fade {
          from { opacity: 0; }
          to { opacity: 1; }
        }
      </style>
      <div id="box"></div>
      <textarea id="ta">Hello</textarea>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(32, 32))?;
    doc.render_frame()?;

    let prepared = doc.prepared().expect("prepared");
    assert!(
      prepared.fragment_tree.keyframes.contains_key("fade"),
      "expected @keyframes fade to be stored on the fragment tree"
    );

    let before = doc.invalidation_counters();
    let textarea = doc
      .dom()
      .get_element_by_id("ta")
      .expect("textarea#ta element");
    let text_node = first_text_child(doc.dom(), textarea).expect("text child node");
    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_node, "Updated").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);

    let prepared = doc.prepared().expect("prepared");
    assert!(
      prepared.fragment_tree.keyframes.contains_key("fade"),
      "incremental relayout should preserve fragment-tree keyframes metadata for form controls"
    );
    Ok(())
  }

  #[test]
  fn incremental_relayout_preserves_svg_filter_defs_metadata() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        html, body { margin: 0; }
      </style>
      <svg width="0" height="0" style="position:absolute">
        <defs>
          <filter id="blur">
            <feGaussianBlur in="SourceGraphic" stdDeviation="1" />
          </filter>
        </defs>
      </svg>
      <p id="text">Hello</p>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(32, 32))?;
    doc.render_frame()?;

    let prepared = doc.prepared().expect("prepared");
    let defs = prepared
      .fragment_tree
      .svg_filter_defs
      .as_deref()
      .expect("svg_filter_defs should be collected on initial layout");
    assert!(
      defs.contains_key("blur"),
      "expected svg filter defs to contain blur filter"
    );

    let before = doc.invalidation_counters();
    let p = doc.dom().get_element_by_id("text").expect("p#text element");
    let text_node = first_text_child(doc.dom(), p).expect("text child node");
    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_node, "Updated").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);

    let prepared = doc.prepared().expect("prepared");
    let defs = prepared
      .fragment_tree
      .svg_filter_defs
      .as_deref()
      .expect("svg_filter_defs should be preserved after incremental relayout");
    assert!(
      defs.contains_key("blur"),
      "incremental relayout should preserve fragment-tree svg_filter_defs metadata"
    );
    Ok(())
  }

  #[test]
  fn incremental_relayout_preserves_svg_id_defs_raw_metadata() -> Result<()> {
    let renderer = renderer_for_tests();
    // NOTE: this HTML contains `href="#icon"`, so it must use a raw string delimiter with at least
    // two `#` characters to avoid terminating the literal early.
    let html = r##"
      <svg width="0" height="0" style="position:absolute">
        <symbol id="icon" viewBox="0 0 10 10">
          <rect width="10" height="10" fill="currentColor" />
        </symbol>
      </svg>
      <svg width="10" height="10">
        <use href="#icon"></use>
      </svg>
      <p id="text">Hello</p>
    "##;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(32, 32))?;
    doc.render_frame()?;

    let prepared = doc.prepared().expect("prepared");
    let defs = prepared
      .fragment_tree
      .svg_id_defs
      .as_deref()
      .expect("svg_id_defs should be collected on initial layout");
    assert!(defs.contains_key("icon"), "expected svg_id_defs to contain icon");

    let raw = prepared
      .fragment_tree
      .svg_id_defs_raw
      .as_deref()
      .expect("svg_id_defs_raw should be collected on initial layout");
    assert!(
      raw.contains_key("icon"),
      "expected svg_id_defs_raw to contain icon"
    );

    let before = doc.invalidation_counters();
    let p = doc.dom().get_element_by_id("text").expect("p#text element");
    let text_node = first_text_child(doc.dom(), p).expect("text child node");
    let changed = doc.mutate_dom(|dom| dom.set_text_data(text_node, "Updated").expect("set text"));
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);

    let prepared = doc.prepared().expect("prepared");
    let defs = prepared
      .fragment_tree
      .svg_id_defs
      .as_deref()
      .expect("svg_id_defs should be preserved after incremental relayout");
    assert!(
      defs.contains_key("icon"),
      "incremental relayout should preserve fragment-tree svg_id_defs metadata"
    );
    let raw = prepared
      .fragment_tree
      .svg_id_defs_raw
      .as_deref()
      .expect("svg_id_defs_raw should be preserved after incremental relayout");
    assert!(
      raw.contains_key("icon"),
      "incremental relayout should preserve fragment-tree svg_id_defs_raw metadata"
    );
    Ok(())
  }

  #[test]
  fn incremental_relayout_preserves_starting_style_snapshots() -> Result<()> {
    let renderer = renderer_for_tests();
    // Multi-column layout uses translation helpers that clear `FragmentNode.starting_style` on the
    // translated fragment roots. Full pipeline runs reattach the snapshots afterward; incremental
    // relayout must do the same.
    let html = r#"
      <style>
        #columns { column-count: 2; column-gap: 0; width: 20px; }
        #box { width: 10px; height: 10px; background: black; opacity: 1; transition: opacity 1000ms linear; }
        @starting-style { #box { opacity: 0; } }
      </style>
      <div id="columns"><div id="box"></div></div>
      <p id="text">Hello</p>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(64, 64))?;
    doc.render_frame()?;

    fn principal_box_id_for_styled_node_id(root: &BoxNode, styled_node_id: usize) -> Option<usize> {
      let mut stack: Vec<&BoxNode> = vec![root];
      while let Some(node) = stack.pop() {
        if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
          return Some(node.id);
        }
        if let Some(body) = node.footnote_body.as_deref() {
          stack.push(body);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      None
    }

    let box_node = doc.dom().get_element_by_id("box").expect("#box element");
    let box_preorder = doc
      .last_dom_mapping()
      .and_then(|mapping| mapping.preorder_for_node_id(box_node))
      .expect("#box preorder id");

    let prepared = doc.prepared().expect("prepared");
    let box_id = principal_box_id_for_styled_node_id(&prepared.box_tree().root, box_preorder)
      .expect("principal box id");
    let fragment = prepared
      .fragment_tree
      .iter_fragments()
      .find(|frag| frag.box_id() == Some(box_id))
      .expect("box fragment");
    assert!(
      fragment.starting_style.is_some(),
      "expected #box fragment to have a starting-style snapshot after initial layout"
    );

    let before = doc.invalidation_counters();
    let text_parent = doc.dom().get_element_by_id("text").expect("p#text element");
    let text_node = first_text_child(doc.dom(), text_parent).expect("text child node");
    assert!(doc.mutate_dom(|dom| dom.set_text_data(text_node, "Updated").expect("set text")));
    doc.render_frame()?;
    let after = doc.invalidation_counters();
    assert_eq!(after.incremental_relayouts, before.incremental_relayouts + 1);

    let prepared = doc.prepared().expect("prepared");
    let fragment = prepared
      .fragment_tree
      .iter_fragments()
      .find(|frag| frag.box_id() == Some(box_id))
      .expect("box fragment");
    assert!(
      fragment.starting_style.is_some(),
      "incremental relayout should preserve starting-style snapshots for transition sampling"
    );
    Ok(())
  }

  #[test]
  fn dialog_open_state_inert_propagation_forces_full_restyle() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        #outside { background: rgb(0,0,0); }
        #outside[data-fastr-inert] { background: rgb(255,0,0); }
      </style>
      <div id=wrap>
        <dialog id=dlg data-fastr-open=false></dialog>
      </div>
      <div id=outside>Outside</div>
    "#;

    let mut doc = BrowserDocumentDom2::new(
      renderer,
      html,
      RenderOptions::new().with_viewport(64, 64),
    )?;
    let dlg = doc.dom().get_element_by_id("dlg").expect("dlg");
    let outside = doc.dom().get_element_by_id("outside").expect("outside");

    doc.render_frame()?;
    let before = doc.invalidation_counters();

    let style0 = doc
      .computed_style_for_dom_node(outside)
      .expect("computed style");
    assert_eq!(style0.background_color, crate::Rgba::BLACK);

    let changed = doc.mutate_dom(|dom| {
      dom
        .set_attribute(dlg, "data-fastr-open", "modal")
        .expect("set data-fastr-open")
    });
    assert!(changed);

    doc.render_frame()?;
    let after = doc.invalidation_counters();

    let style1 = doc
      .computed_style_for_dom_node(outside)
      .expect("computed style");
    assert_eq!(style1.background_color, crate::Rgba::RED);

    assert_eq!(
      after.incremental_restyles, before.incremental_restyles,
      "incremental restyle must be disabled for top-layer affecting mutations"
    );
    assert_eq!(after.full_restyles, before.full_restyles + 1);

    Ok(())
  }

  #[test]
  fn mutate_dom_false_does_not_invalidate() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    doc.render_frame().unwrap();

    let changed = doc.mutate_dom(|_dom| false);
    assert!(!changed);
    assert!(doc.render_if_needed().unwrap().is_none());
  }

  #[test]
  fn script_internal_slot_updates_do_not_invalidate_layout() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><script id=s></script><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;
    doc.render_frame()?;

    let script = doc.dom().get_element_by_id("s").expect("script element");
    assert!(
      doc.dom().node(script).script_parser_document,
      "expected scripts imported from renderer DOM to start as parser-inserted"
    );
    assert!(
      !doc.dom().node(script).script_force_async,
      "expected parser-inserted scripts to start with force_async=false"
    );

    let before_generation = doc.dom_mutation_generation();

    // Running "prepare the script element" mutates per-script internal slots that do not affect
    // rendering. These updates must not bump the dom2 mutation generation, otherwise
    // `BrowserDocumentDom2` will treat the document as dirty and force a full layout flush.
    let changed = doc.mutate_dom(|dom| {
      let spec = crate::js::dom_integration::build_dynamic_script_element_spec(dom, script, None);
      let _should_run = crate::js::prepare_script_element_dom2(dom, script, &spec);
      false
    });
    assert!(!changed);

    assert_eq!(
      doc.dom_mutation_generation(),
      before_generation,
      "updating script internal slots must not bump mutation generation"
    );
    assert!(
      !doc.dom().node(script).script_parser_document,
      "expected prepare_script_element_dom2 to clear script_parser_document"
    );
    assert!(
      doc.dom().node(script).script_force_async,
      "expected prepare_script_element_dom2 to set script_force_async when parser-inserted and not async"
    );
    assert!(
      doc.render_if_needed()?.is_none(),
      "expected no paint/layout invalidation solely due to script internal slot updates"
    );
    Ok(())
  }

  #[test]
  fn mutate_dom_noop_append_child_does_not_invalidate() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");
    doc.render_frame().unwrap();

    let body = doc.dom().body().expect("body element");

    // Create a detached node. (Using `dom_mut()` here invalidates unconditionally; clear it before
    // exercising `mutate_dom` invalidation behaviour.)
    let child = doc.dom_mut().create_element("div", "");
    doc.render_frame().unwrap();

    // First append should change the tree and dirty the document.
    let changed = doc.mutate_dom(|dom| dom.append_child(body, child).expect("append child"));
    assert!(changed);
    assert!(doc.render_if_needed().unwrap().is_some());
    assert!(doc.render_if_needed().unwrap().is_none());

    // Appending the same (already-last) child again is a no-op in dom2 and must not dirty the host.
    let changed = doc.mutate_dom(|dom| dom.append_child(body, child).expect("append child"));
    assert!(!changed);
    assert!(doc.render_if_needed().unwrap().is_none());
  }

  #[test]
  fn mutate_dom_detached_attribute_change_does_not_invalidate() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;

    // Create a detached element up front (using dom_mut() for convenience).
    let detached = doc.dom_mut().create_element("div", "");
    assert!(!doc.dom().is_connected_for_scripting(detached));

    // First render commits a clean layout.
    doc.render_frame()?;
    let before = doc.invalidation_counters();

    // Mutating attributes on a detached node bumps `dom2::Document`'s mutation generation, but must
    // not force a renderer layout/paint.
    let changed = doc.mutate_dom(|dom| dom.set_attribute(detached, "class", "changed").expect("set attribute"));
    assert!(changed);
    assert!(doc.render_if_needed()?.is_none());
    assert_eq!(doc.invalidation_counters(), before);
    Ok(())
  }

  #[test]
  fn mutate_dom_template_text_change_does_not_invalidate() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><template>hello</template><div>visible</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;

    doc.render_frame()?;
    let before = doc.invalidation_counters();

    let template_text = first_text_node_in_inert_template(doc.dom()).expect("template text node");
    assert!(!doc.dom().is_connected_for_scripting(template_text));

    let changed = doc
      .mutate_dom(|dom| dom.set_text_data(template_text, "updated").expect("set text"));
    assert!(changed);

    assert!(doc.render_if_needed()?.is_none());
    assert_eq!(doc.invalidation_counters(), before);
    Ok(())
  }

  #[test]
  fn unclassified_dom2_mutation_forces_full_invalidation() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><template>hello</template><div id=\"visible\">visible</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;
    doc.render_frame()?;
    let before = doc.invalidation_counters();

    // Mutating the inert template subtree should not invalidate by itself (see
    // `mutate_dom_template_text_change_does_not_invalidate`).
    let template_text = first_text_node_in_inert_template(doc.dom()).expect("template text node");
    assert!(!doc.dom().is_connected_for_scripting(template_text));

    // Perform an unclassified render-affecting mutation via `node_mut()`. This must prevent the
    // "disconnected/inert-only" generation sync optimization from incorrectly treating the document
    // as clean.
    let visible = doc.dom().get_element_by_id("visible").expect("visible div");
    let visible_text = first_text_child(doc.dom(), visible).expect("visible text node");

    let changed = doc.mutate_dom(|dom| {
      dom
        .set_text_data(template_text, "updated")
        .expect("set template text");

      let node = dom.node_mut(visible_text);
      let crate::dom2::NodeKind::Text { content } = &mut node.kind else {
        panic!("expected visible text node");
      };
      content.clear();
      content.push_str("changed");
      true
    });
    assert!(changed);

    assert!(
      doc.render_if_needed()?.is_some(),
      "unclassified mutations must invalidate even when only inert-only structured mutations are present"
    );

    let after = doc.invalidation_counters();
    assert_eq!(
      after.incremental_relayouts, before.incremental_relayouts,
      "unclassified mutations should fall back to a full pipeline run (no incremental relayout)"
    );
    assert_eq!(after.full_restyles, before.full_restyles + 1);
    assert_eq!(after.full_relayouts, before.full_relayouts + 1);
    Ok(())
  }

  #[test]
  fn unclassified_dom2_mutation_forces_full_invalidation_via_dom_host() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><template>hello</template><div id=\"visible\">visible</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;
    doc.render_frame()?;
    let before = doc.invalidation_counters();

    let template_text = first_text_node_in_inert_template(doc.dom()).expect("template text node");
    let visible = doc.dom().get_element_by_id("visible").expect("visible div");
    let visible_text = first_text_child(doc.dom(), visible).expect("visible text node");

    <BrowserDocumentDom2 as crate::js::DomHost>::mutate_dom(&mut doc, |dom| {
      dom
        .set_text_data(template_text, "updated")
        .expect("set template text");

      let node = dom.node_mut(visible_text);
      let crate::dom2::NodeKind::Text { content } = &mut node.kind else {
        panic!("expected visible text node");
      };
      content.clear();
      content.push_str("changed");
      ((), true)
    });

    assert!(
      doc.render_if_needed()?.is_some(),
      "DomHost::mutate_dom should treat unclassified mutations as full invalidations"
    );

    let after = doc.invalidation_counters();
    assert_eq!(
      after.incremental_relayouts, before.incremental_relayouts,
      "unclassified mutations should fall back to a full pipeline run (no incremental relayout)"
    );
    assert_eq!(after.full_restyles, before.full_restyles + 1);
    assert_eq!(after.full_relayouts, before.full_relayouts + 1);
    Ok(())
  }

  #[test]
  fn mutation_observer_bookkeeping_does_not_invalidate_renderer() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");
    doc.render_frame().expect("render");

    let mut realm = crate::js::window_realm::WindowRealm::new(
      crate::js::window_realm::WindowRealmConfig::new("https://example.invalid/"),
    )
    .expect("WindowRealm");
    let mut hooks = vm_js::MicrotaskQueue::new();

    realm
      .exec_script_with_host_and_hooks(
        &mut doc,
        &mut hooks,
        "const mo = new MutationObserver(() => {});\n\
         mo.observe(document.body, { childList: true });\n\
         mo.disconnect();",
      )
      .expect("execute MutationObserver script");

    assert!(
      doc.render_if_needed().unwrap().is_none(),
      "MutationObserver.observe/disconnect should not invalidate style/layout/paint"
    );
  }

  #[test]
  fn intersection_observer_bookkeeping_does_not_invalidate_renderer() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");
    doc.render_frame().expect("render");

    let mut realm = crate::js::window_realm::WindowRealm::new(
      crate::js::window_realm::WindowRealmConfig::new("https://example.invalid/"),
    )
    .expect("WindowRealm");
    let mut hooks = vm_js::MicrotaskQueue::new();

    realm
      .exec_script_with_host_and_hooks(
        &mut doc,
        &mut hooks,
        "const io = new IntersectionObserver(() => {});\n\
         io.observe(document.body);\n\
         io.unobserve(document.body);\n\
         io.disconnect();\n\
         io.takeRecords();",
      )
      .expect("execute IntersectionObserver script");

    assert!(
      doc.render_if_needed().unwrap().is_none(),
      "IntersectionObserver observe/unobserve/disconnect/takeRecords should not invalidate style/layout/paint"
    );
  }

  #[test]
  fn resize_observer_bookkeeping_does_not_invalidate_renderer() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");
    doc.render_frame().expect("render");

    let mut realm = crate::js::window_realm::WindowRealm::new(
      crate::js::window_realm::WindowRealmConfig::new("https://example.invalid/"),
    )
    .expect("WindowRealm");
    let mut hooks = vm_js::MicrotaskQueue::new();

    realm
      .exec_script_with_host_and_hooks(
        &mut doc,
        &mut hooks,
        "const ro = new ResizeObserver(() => {});\n\
         ro.observe(document.body);\n\
         ro.unobserve(document.body);\n\
         ro.disconnect();\n\
         ro.takeRecords();",
      )
      .expect("execute ResizeObserver script");

    assert!(
      doc.render_if_needed().unwrap().is_none(),
      "ResizeObserver observe/unobserve/disconnect/takeRecords should not invalidate style/layout/paint"
    );
  }

  #[test]
  fn detached_node_creation_does_not_invalidate_renderer() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");
    doc.render_frame().expect("render");

    let mut realm = crate::js::window_realm::WindowRealm::new(
      crate::js::window_realm::WindowRealmConfig::new("https://example.invalid/"),
    )
    .expect("WindowRealm");
    let mut hooks = vm_js::MicrotaskQueue::new();

    realm
      .exec_script_with_host_and_hooks(
        &mut doc,
        &mut hooks,
        "document.createElement('div');\n\
         document.createElementNS('http://www.w3.org/1999/xhtml', 'div');\n\
         document.createAttribute('data-x');\n\
         document.createAttributeNS(null, 'data-y');\n\
         document.createTextNode('x');\n\
         document.createComment('x');\n\
         document.createDocumentFragment();\n\
         document.body.cloneNode(true);\n\
         document.importNode(document.body, true);",
      )
      .expect("execute detached node creation script");

    assert!(
      doc.render_if_needed().unwrap().is_none(),
      "Detached node/attribute creation should not invalidate style/layout/paint"
    );
  }

  #[test]
  fn script_internal_slot_updates_do_not_invalidate_renderer() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><head><script id=s></script></head><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");
    doc.render_frame().expect("render");

    let script = find_first_html_script_by_id(doc.dom(), "s").expect("expected <script id=s>");
    assert!(
      doc.dom().node(script).script_parser_document && !doc.dom().node(script).script_force_async,
      "expected parser-inserted script defaults"
    );

    let generation_before = doc.dom_mutation_generation();
    let changed = doc.mutate_dom(|dom| {
      let base = crate::html::base_url_tracker::BaseUrlTracker::new(None);
      let spec = crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2(
        &*dom, script, &base,
      );
      let _should_run = crate::js::prepare_script_element_dom2(dom, script, &spec);
      false
    });
    assert!(!changed, "script internal-slot updates are not render-affecting");
    assert!(
      !doc.dom().node(script).script_parser_document && doc.dom().node(script).script_force_async,
      "expected prepare_script_element_dom2 to clear parser_document and set force_async"
    );
    assert_eq!(
      doc.dom_mutation_generation(),
      generation_before,
      "script internal-slot updates must not bump dom mutation generation"
    );
    assert!(
      doc.render_if_needed().unwrap().is_none(),
      "script internal-slot updates must not invalidate style/layout/paint"
    );
  }

  #[test]
  fn dom2_document_address_is_stable_across_moves_and_changes_on_reset() -> Result<()> {
    let renderer = renderer_for_tests();
    let doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;

    // The underlying `dom2::Document` must not move when the host is moved.
    let ptr0 = doc.dom() as *const crate::dom2::Document;
    let mut doc = doc;
    let ptr1 = doc.dom() as *const crate::dom2::Document;
    assert_eq!(ptr0, ptr1);

    // Rendering must not relocate the DOM.
    doc.render_frame()?;
    let ptr2 = doc.dom() as *const crate::dom2::Document;
    assert_eq!(ptr1, ptr2);

    let before_reset = doc.dom() as *const crate::dom2::Document;
    doc.reset_with_dom(
      crate::dom2::Document::new(QuirksMode::NoQuirks),
      RenderOptions::new().with_viewport(32, 32),
    );
    let after_reset = doc.dom() as *const crate::dom2::Document;
    assert_ne!(before_reset, after_reset);

    Ok(())
  }

  #[test]
  fn realtime_animations_sample_document_timeline_when_enabled() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        html, body { margin: 0; background: white; }
        #box {
          width: 10px;
          height: 10px;
          background: black;
          animation: fade 1000ms linear forwards;
        }
        @keyframes fade {
          from { opacity: 0; }
          to { opacity: 1; }
        }
      </style>
      <div id="box"></div>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(20, 20))?;

    let clock = Arc::new(crate::clock::VirtualClock::new());
    doc.set_animation_clock(clock.clone());
    doc.set_realtime_animations_enabled(true);

    let pixmap0 = doc.render_frame()?;
    let c0 = pixmap0.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c0, 255, 0);

    clock.advance(Duration::from_millis(500));
    let sampled = doc.animation_time_for_paint().expect("animation time");
    assert!(
      (sampled - 500.0).abs() <= 0.1,
      "expected document timeline ~500ms, got {sampled}"
    );

    let pixmap1 = doc.render_frame()?;
    let c1 = pixmap1.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c1, 128, 8);

    // Explicit per-render timestamps always override the real-time document timeline.
    doc.set_animation_time_ms(1000.0);
    let pixmap_override = doc.render_frame()?;
    let c2 = pixmap_override.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c2, 0, 0);

    Ok(())
  }

  fn pixel_gray(pixmap: &super::super::Pixmap) -> u8 {
    let px = pixmap.pixel(0, 0).expect("pixel in bounds");
    assert_eq!(px.alpha(), 255, "expected opaque pixel");
    assert_eq!(px.red(), px.green(), "expected grayscale pixel");
    assert_eq!(px.red(), px.blue(), "expected grayscale pixel");
    px.red()
  }

  fn assert_pixel_gray_approx(pixmap: &super::super::Pixmap, expected: u8, tolerance: u8) {
    let actual = pixel_gray(pixmap);
    let delta = actual.abs_diff(expected);
    assert!(
      delta <= tolerance,
      "expected gray ≈{expected}±{tolerance}, got {actual} (Δ={delta})"
    );
  }

  #[test]
  fn realtime_animations_pause_and_resume_across_frames() -> Result<()> {
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: white; }
            #a { width: 1px; height: 1px; background: black; animation: fade 1s linear forwards; }
            @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
          </style>
        </head>
        <body>
          <div id="a"></div>
        </body>
      </html>
    "#;
    let renderer = renderer_for_tests();
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(2, 2))?;

    let clock = Arc::new(crate::clock::VirtualClock::new());
    doc.set_animation_clock(clock.clone());
    doc.set_realtime_animations_enabled(true);

    // t=0ms: opacity=0 => background shines through.
    let frame0 = doc.render_frame()?;
    assert_eq!(pixel_gray(&frame0), 255);

    // t=600ms: opacity=0.6 => ~40% white.
    clock.advance(Duration::from_millis(600));
    let frame600 = doc.render_frame()?;
    assert_pixel_gray_approx(&frame600, 102, 4);

    // Pause at t=600ms.
    let node = doc.dom().get_element_by_id("a").expect("#a element");
    doc
      .dom_mut()
      .set_attribute(node, "style", "animation-play-state: paused;")
      .expect("set_attribute");
    let paused600 = doc.render_frame()?;
    assert_pixel_gray_approx(&paused600, 102, 4);

    // Advance time while paused; output should remain frozen.
    clock.advance(Duration::from_millis(300));
    let paused900 = doc.render_frame()?;
    assert_pixel_gray_approx(&paused900, 102, 4);

    // Resume at t=900ms (without advancing time).
    let node = doc.dom().get_element_by_id("a").expect("#a element");
    doc
      .dom_mut()
      .set_attribute(node, "style", "animation-play-state: running;")
      .expect("set_attribute");
    let resumed900 = doc.render_frame()?;
    assert_pixel_gray_approx(&resumed900, 102, 4);

    // t=1000ms: animation should have progressed to 700ms of active time (0.7 opacity).
    clock.advance(Duration::from_millis(100));
    let frame1000 = doc.render_frame()?;
    assert_pixel_gray_approx(&frame1000, 77, 5);

    Ok(())
  }

  #[test]
  fn dom_mapping_translates_renderer_preorder_to_dom2_node_id() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div id=target>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    doc.render_frame().expect("render");
    assert!(
      doc.last_dom_mapping().is_some(),
      "expected dom2↔renderer mapping"
    );

    let prepared = doc.prepared.as_ref().expect("prepared layout");
    let ids = crate::dom::enumerate_dom_ids(prepared.dom());
    let target = find_renderer_element_by_id(prepared.dom(), "target")
      .expect("target element in renderer DOM");
    let preorder_id = *ids
      .get(&(target as *const crate::dom::DomNode))
      .expect("renderer preorder id for target");

    let dom2_id = doc
      .dom2_node_for_renderer_preorder(preorder_id)
      .expect("dom2 id for preorder");
    match &doc.dom().node(dom2_id).kind {
      crate::dom2::NodeKind::Element {
        tag_name,
        attributes,
        ..
      } => {
        assert!(tag_name.eq_ignore_ascii_case("div"));
        let id_attr = attributes
          .iter()
          .find(|attr| attr.qualified_name().eq_ignore_ascii_case("id"))
          .map(|attr| attr.value.as_str());
        assert_eq!(id_attr, Some("target"));
      }
      other => panic!("expected mapped dom2 node to be an element, got {other:?}"),
    }
  }

  #[test]
  fn dom_mapping_handles_template_contents_without_shifting_following_nodes() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><template><span>in</span></template><div id=after>After</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    doc.render_frame().expect("render");
    let prepared = doc.prepared.as_ref().expect("prepared layout");
    let ids = crate::dom::enumerate_dom_ids(prepared.dom());
    let after =
      find_renderer_element_by_id(prepared.dom(), "after").expect("after element in renderer DOM");
    let preorder_id = *ids
      .get(&(after as *const crate::dom::DomNode))
      .expect("renderer preorder id for after element");

    let dom2_id = doc
      .dom2_node_for_renderer_preorder(preorder_id)
      .expect("dom2 id for after preorder");
    match &doc.dom().node(dom2_id).kind {
      crate::dom2::NodeKind::Element {
        tag_name,
        attributes,
        ..
      } => {
        assert!(tag_name.eq_ignore_ascii_case("div"));
        let id_attr = attributes
          .iter()
          .find(|attr| attr.qualified_name().eq_ignore_ascii_case("id"))
          .map(|attr| attr.value.as_str());
        assert_eq!(id_attr, Some("after"));
      }
      other => panic!("expected mapped dom2 node to be an element, got {other:?}"),
    }
  }

  #[test]
  fn dom_pointer_is_stable_across_moves_and_changes_on_reset_paths() -> Result<()> {
    let renderer = renderer_for_tests();
    let options = RenderOptions::new().with_viewport(16, 16);
    let doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>hi</div></body></html>",
      options.clone(),
    )?;
    let ptr0 = doc.dom_ptr().as_ptr();

    // Moving the host must not change the address of the underlying `dom2::Document`, since JS
    // shims store the document pointer in a registry.
    let mut doc = doc;
    assert_eq!(ptr0, doc.dom_ptr().as_ptr());

    // Reset paths replace the underlying document allocation, so the pointer must change.
    doc.reset_with_html(
      "<!doctype html><html><body><span>reset</span></body></html>",
      options.clone(),
    )?;
    let ptr1 = doc.dom_ptr().as_ptr();
    assert_ne!(ptr0, ptr1);

    let (prepared, _) = doc.prepare_dom_with_options(None, None)?;
    doc.reset_with_prepared(prepared, options);
    let ptr2 = doc.dom_ptr().as_ptr();
    assert_ne!(ptr1, ptr2);

    Ok(())
  }

  #[test]
  fn ensure_layout_for_dom_queries_populates_prepared_and_mapping() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div id=target style=\"width: 10px; height: 15px;\"></div></body></html>",
      RenderOptions::new().with_viewport(64, 64),
    )?;

    doc.ensure_layout_for_dom_queries()?;
    assert!(doc.prepared.is_some(), "expected prepared layout artifacts");
    assert!(
      doc.last_dom_mapping.is_some(),
      "expected dom2↔renderer mapping"
    );

    Ok(())
  }

  #[test]
  fn dom2_mutation_log_is_cleared_after_successful_render() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div id=a></div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )?;

    let node = doc.dom().get_element_by_id("a").expect("#a element");
    doc
      .dom_mut()
      .set_attribute(node, "data-x", "1")
      .expect("set_attribute");

    doc.render_frame()?;

    assert!(
      doc.dom.take_mutations().is_empty(),
      "expected dom2 mutation log to be cleared after render"
    );
    Ok(())
  }

  #[test]
  fn layout_query_helpers_expose_box_geometry() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #target { width: 10px; height: 15px; }
          </style>
        </head>
        <body>
          <div id="target"></div>
        </body>
      </html>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(64, 64))?;

    let target = doc.dom().get_element_by_id("target").expect("#target");

    let box_id = doc
      .principal_box_id_for_node(target)?
      .expect("principal box id for #target");
    assert!(box_id > 0);

    let rect = doc
      .border_box_rect_viewport(target)?
      .expect("border box rect");
    assert!(
      (rect.width() - 10.0).abs() <= 0.01,
      "expected width≈10, got {}",
      rect.width()
    );
    assert!(
      (rect.height() - 15.0).abs() <= 0.01,
      "expected height≈15, got {}",
      rect.height()
    );

    Ok(())
  }

  #[test]
  fn clamp_element_scroll_offset_clamps_to_bounds() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #scroller { width: 50px; height: 50px; overflow: scroll; }
            #content { height: 200px; }
          </style>
        </head>
        <body>
          <div id="scroller"><div id="content"></div></div>
        </body>
      </html>
    "#;
    let mut doc =
      BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(128, 128))?;
    let scroller = doc.dom().get_element_by_id("scroller").expect("#scroller");
    let box_id = doc
      .principal_box_id_for_node(scroller)?
      .expect("principal box id for #scroller");

    let clamped = doc.clamp_element_scroll_offset(box_id, Point::new(0.0, 1000.0))?;
    assert!(
      (clamped.y - 150.0).abs() <= 0.01,
      "expected max scroll y≈150, got {}",
      clamped.y
    );
    Ok(())
  }
}
