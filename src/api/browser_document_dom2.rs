use crate::animation::TransitionState;
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::{Point, Rect};
use crate::js::clock::{Clock, RealClock};
use crate::js::host_document::{ActiveEventGuard, ActiveEventStack};
use crate::js::CurrentScriptStateHandle;
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;
use crate::style::cascade::StyledNode;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxType};
use crate::web::dom::DocumentVisibilityState;
use rustc_hash::{FxHashMap, FxHashSet};
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
  /// Incremental style recomputations (not yet implemented; reserved for future work).
  pub incremental_restyles: u64,
  /// Full layout recomputations that included a restyle.
  pub full_relayouts: u64,
  /// Layout recomputations performed without rerunning cascade.
  pub incremental_relayouts: u64,
}

/// Mutable, multi-frame renderer backed by a live `dom2` document.
///
/// `BrowserDocumentDom2` mirrors [`super::BrowserDocument`] but stores a spec-ish mutable
/// [`crate::dom2::Document`] as the authoritative DOM (e.g. for JavaScript). The renderer only
/// snapshots the `dom2` document into the renderer's immutable [`crate::dom::DomNode`] form when a
/// layout recomputation is needed.
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
  /// Optional WebIDL binding dispatch host used when the document itself is installed as the
  /// `webidl_vm_js::WebIdlBindingsHost` (for example via `WebIdlBindingsHostSlot`).
  ///
  /// In FastRender's primary execution pipelines, generated vm-js WebIDL bindings dispatch through
  /// a `VmJsWebIdlBindingsHostDispatch<Host>` owned by the surrounding window/tab host.
  /// `BrowserDocumentDom2` stores a non-owning pointer to that dispatcher so that if the document is
  /// used as the WebIDL host, it can still forward to the real dispatcher.
  webidl_bindings_host: Option<NonNull<dyn webidl_vm_js::WebIdlBindingsHost>>,
  options: RenderOptions,
  prepared: Option<PreparedDocument>,
  last_dom_mapping: Option<crate::dom2::RendererDomMapping>,
  animation_state_store: crate::animation::AnimationStateStore,
  style_dirty: bool,
  layout_dirty: bool,
  paint_dirty: bool,
  dirty_style_nodes: FxHashSet<crate::dom2::NodeId>,
  dirty_text_nodes: FxHashSet<crate::dom2::NodeId>,
  dirty_structure_nodes: FxHashSet<crate::dom2::NodeId>,
  invalidation_counters: BrowserDocumentDom2InvalidationCounters,
  last_seen_dom_mutation_generation: u64,
  realtime_animations_enabled: bool,
  animation_clock: Arc<dyn Clock>,
  animation_timeline_origin: Option<Duration>,
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
      webidl_bindings_host: None,
      options,
      prepared: None,
      last_dom_mapping: None,
      animation_state_store: crate::animation::AnimationStateStore::new(),
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
      dirty_style_nodes: FxHashSet::default(),
      dirty_text_nodes: FxHashSet::default(),
      dirty_structure_nodes: FxHashSet::default(),
      invalidation_counters: BrowserDocumentDom2InvalidationCounters::default(),
      last_seen_dom_mutation_generation,
      realtime_animations_enabled: false,
      animation_clock: Arc::new(RealClock::default()),
      animation_timeline_origin: None,
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

  pub(crate) fn set_webidl_bindings_host(
    &mut self,
    host: &mut dyn webidl_vm_js::WebIdlBindingsHost,
  ) {
    self.webidl_bindings_host = Some(NonNull::from(host));
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
    } else if !enabled && self.realtime_animations_enabled {
      self.realtime_animations_enabled = false;
      self.animation_timeline_origin = None;
      self.animation_state_store = crate::animation::AnimationStateStore::new();
    }
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
    // Reset per-document CSP state. `reset_with_dom` replaces the entire document, so any previously
    // captured CSP headers/meta should not leak into the new DOM.
    self.renderer.document_csp = None;
    self.animation_state_store = crate::animation::AnimationStateStore::new();
    self.animation_timeline_origin = None;
    self.invalidate_all();
  }

  /// Replaces the live DOM with a prepared document's DOM and installs the prepared cache.
  ///
  /// The next `render_if_needed` call will paint using the prepared layout without re-running
  /// cascade/layout.
  pub fn reset_with_prepared(&mut self, prepared: PreparedDocument, options: RenderOptions) {
    self.current_script.reset();
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
    self.dirty_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
    self.animation_timeline_origin = None;
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
    let changed = f(self.dom.as_mut());
    if changed {
      let mutations = self.dom.take_mutations();
      if mutations.is_empty() {
        // The caller reported changes but we have no structured mutation data (e.g. direct `node_mut`
        // edits). Fall back to a full invalidation to preserve correctness.
        self.invalidate_all();
      } else {
        self.apply_mutation_log(mutations);
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

  /// Returns true when style/layout must be recomputed before painting.
  pub fn needs_layout(&self) -> bool {
    self.prepared.is_none()
      || self.style_dirty
      || self.layout_dirty
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
    // Layout queries should not force a re-layout when we already have an up-to-date prepared cache.
    if self.prepared.is_some() && !self.needs_layout() {
      return Ok(());
    }

    // Match `render_frame_with_deadlines`: if we haven't prepared before, force a full pipeline run
    // even if dirty flags were cleared out-of-band.
    if self.prepared.is_none() {
      self.invalidate_all();
    }

    let needs_layout = self.style_dirty
      || self.layout_dirty
      || self.dom.mutation_generation() != self.last_seen_dom_mutation_generation;
    if !needs_layout {
      return Ok(());
    }

    // Layout without style changes can often avoid a full cascade by patching the existing box tree
    // and rerunning only layout (e.g. text content changes).
    let can_incremental_relayout = !self.style_dirty
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
      match self.incremental_relayout_for_text_changes(&mut prepared) {
        Ok(true) => {
          self.invalidation_counters.incremental_relayouts = self
            .invalidation_counters
            .incremental_relayouts
            .saturating_add(1);
          // Incremental relayout does not take a new renderer DOM snapshot, but it does satisfy the
          // outstanding layout invalidation for the current DOM generation.
          self.last_seen_dom_mutation_generation = self.dom.mutation_generation();
          self.prepared = Some(prepared);
          did_incremental_layout = true;
        }
        Ok(false) => {
          // Could not safely apply incremental relayout; fall back to a full pipeline run.
          self.prepared = Some(prepared);
        }
        Err(err) => {
          // Preserve the (possibly partially updated) prepared artifacts so callers can retry.
          self.prepared = Some(prepared);
          return Err(err);
        }
      }
    }

    if !did_incremental_layout {
      let prev_prepared = self.prepared.take();
      let mut prepared = match self.prepare_dom_with_options() {
        Ok(prepared) => prepared,
        Err(err) => {
          self.prepared = prev_prepared;
          return Err(err);
        }
      };

      self.invalidation_counters.full_restyles =
        self.invalidation_counters.full_restyles.saturating_add(1);
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

      self.prepared = Some(prepared);
    }

    // Style/layout are now satisfied, but we intentionally do not paint here.
    self.style_dirty = false;
    self.layout_dirty = false;
    self.dirty_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
    self.paint_dirty = true;
    Ok(())
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

  /// Returns the renderer computed style for a `dom2` node using the latest prepared layout.
  ///
  /// This is intended for DOM query APIs like `getComputedStyle()`. It prefers the style attached
  /// to the node's **principal box** (so used-value adjustments like blockification are observed).
  /// If the node does not generate a principal box (e.g. `display: contents`), it falls back to
  /// scanning the styled tree.
  pub fn computed_style_for_dom_node(&self, node_id: crate::dom2::NodeId) -> Option<Arc<ComputedStyle>> {
    let prepared = self.prepared.as_ref()?;
    let mapping = self.last_dom_mapping.as_ref()?;
    let preorder = mapping.preorder_for_node_id(node_id)?;

    principal_box_style_for_styled_node_id(&prepared.box_tree.root, preorder)
      .or_else(|| styled_tree_style_for_preorder_id(&prepared.styled_tree, preorder))
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

    let Some(mut node_id) = self.dom2_node_for_hit_test(&hit) else {
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

  /// Like [`BrowserDocumentDom2::element_from_point`], but returns all hit elements (topmost first).
  ///
  /// This backs `Document.elementsFromPoint()` when needed.
  pub fn elements_from_point(
    &mut self,
    x: f32,
    y: f32,
  ) -> Result<Vec<crate::dom2::NodeId>> {
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

    let mut out: Vec<crate::dom2::NodeId> = Vec::new();
    for hit in hits {
      let Some(mut node_id) = self.dom2_node_for_hit_test(&hit) else {
        continue;
      };

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
    if !self.is_dirty() && self.prepared.is_some() {
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
    // If we haven't rendered before, force a full pipeline run even if the flags were cleared.
    if self.prepared.is_none() {
      self.invalidate_all();
    }

    let needs_layout = self.style_dirty
      || self.layout_dirty
      || self.dom.mutation_generation() != self.last_seen_dom_mutation_generation;
    if needs_layout {
      // Layout without style changes can often avoid a full cascade by patching the existing box tree
      // and rerunning only layout (e.g. text content changes).
      let can_incremental_relayout = !self.style_dirty
        && self.layout_dirty
        && !self.dirty_text_nodes.is_empty()
        && self.dirty_style_nodes.is_empty()
        && self.dirty_structure_nodes.is_empty()
        && self.prepared.is_some()
        && self.last_dom_mapping.is_some();

      let mut did_incremental_layout = false;
      if can_incremental_relayout {
        match self.prepared.take() {
          Some(mut prepared) => match self.incremental_relayout_for_text_changes(&mut prepared) {
            Ok(true) => {
              self.invalidation_counters.incremental_relayouts = self
                .invalidation_counters
                .incremental_relayouts
                .saturating_add(1);
              // Incremental relayout produces fresh cached layout artifacts without taking a full
              // renderer-DOM snapshot, so we still need to record that we've now "seen" the live DOM
              // mutation generation. Without this, generation-based dirty detection would force an
              // extra full pipeline run on the next `render_if_needed()` call.
              self.last_seen_dom_mutation_generation = self.dom.mutation_generation();
              self.prepared = Some(prepared);
              did_incremental_layout = true;
            }
            Ok(false) => {
              // Could not safely apply incremental relayout; fall back to a full pipeline run.
              self.prepared = Some(prepared);
            }
            Err(err) => {
              // Preserve the (possibly partially updated) prepared artifacts so callers can retry.
              self.prepared = Some(prepared);
              return Err(err);
            }
          },
          None => {}
        }
      }

      if !did_incremental_layout {
        let prev_prepared = self.prepared.take();
        let mut prepared = match self.prepare_dom_with_options() {
          Ok(prepared) => prepared,
          Err(err) => {
            self.prepared = prev_prepared;
            return Err(err);
          }
        };

        self.invalidation_counters.full_restyles =
          self.invalidation_counters.full_restyles.saturating_add(1);
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

        self.prepared = Some(prepared);
      }

      // We now have fresh style/layout artifacts stored in `self.prepared`, even if the subsequent
      // paint step is cancelled or fails. Clear the layout dirtiness so callers can retry paint
      // from cache without re-running cascade/layout.
      self.style_dirty = false;
      self.layout_dirty = false;
      self.dirty_style_nodes.clear();
      self.dirty_text_nodes.clear();
      self.dirty_structure_nodes.clear();
      // Layout changes always require a paint attempt. Keep paint marked dirty so a cancelled paint
      // can be retried.
      self.paint_dirty = true;
    }

    let frame = self.paint_from_cache_frame_with_deadline(paint_deadline)?;

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }

    Ok(frame)
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
    let Some(prepared) = self.prepared.as_ref() else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no cached layout; call render_frame() first".to_string(),
      }));
    };

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
    let frame = prepared.paint_with_options_frame_with_animation_state_store(
      PreparedPaintOptions {
        scroll: Some(scroll_state),
        viewport: None,
        background: None,
        animation_time,
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
    Ok(frame)
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

  fn prepare_dom_with_options(&mut self) -> Result<PreparedDocument> {
    let options = self.options.clone();
    let dom_generation = self.dom.mutation_generation();
    let snapshot = self.dom.as_ref().to_renderer_dom_with_mapping();
    let renderer_dom = snapshot.dom;
    let mapping = snapshot.mapping;
    let renderer_dom_ref = &renderer_dom;

    let prepared = {
      let renderer = &mut self.renderer;

      let toggles = renderer.resolve_runtime_toggles(&options);
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
          None,
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
    Ok(prepared)
  }

  /// Ensure `self.prepared` + `self.last_dom_mapping` reflect the current live DOM.
  ///
  /// This is a "layout flush" that runs at most cascade+layout. It intentionally does **not** paint.
  fn ensure_layout_for_hit_testing(&mut self) -> Result<()> {
    // If we haven't rendered before, force a full pipeline run even if the flags were cleared.
    if self.prepared.is_none() {
      self.invalidate_all();
    }

    // Missing mapping implies we can't translate renderer preorder IDs back to dom2 node IDs.
    let needs_layout = self.style_dirty
      || self.layout_dirty
      || self.prepared.is_none()
      || self.last_dom_mapping.is_none()
      || self.dom.mutation_generation() != self.last_seen_dom_mutation_generation;

    if !needs_layout {
      return Ok(());
    }

    // Layout without style changes can often avoid a full cascade by patching the existing box tree
    // and rerunning only layout (e.g. text content changes).
    let can_incremental_relayout = !self.style_dirty
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
      match self.incremental_relayout_for_text_changes(&mut prepared) {
        Ok(true) => {
          self.invalidation_counters.incremental_relayouts =
            self.invalidation_counters.incremental_relayouts.saturating_add(1);
          self.last_seen_dom_mutation_generation = self.dom.mutation_generation();
          self.prepared = Some(prepared);
          did_incremental_layout = true;
        }
        Ok(false) => {
          // Could not safely apply incremental relayout; fall back to a full pipeline run.
          self.prepared = Some(prepared);
        }
        Err(err) => {
          // Preserve the (possibly partially updated) prepared artifacts so callers can retry.
          self.prepared = Some(prepared);
          return Err(err);
        }
      }
    }

    if !did_incremental_layout {
      let prev_prepared = self.prepared.take();
      let mut prepared = match self.prepare_dom_with_options() {
        Ok(prepared) => prepared,
        Err(err) => {
          self.prepared = prev_prepared;
          return Err(err);
        }
      };

      self.invalidation_counters.full_restyles =
        self.invalidation_counters.full_restyles.saturating_add(1);
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

      self.prepared = Some(prepared);
    }

    // We now have fresh style/layout artifacts stored in `self.prepared`. Clear the layout
    // dirtiness so callers can rely on the cached layout, while leaving paint marked dirty.
    self.style_dirty = false;
    self.layout_dirty = false;
    self.dirty_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
    self.paint_dirty = true;

    Ok(())
  }

  #[inline]
  fn invalidate_all(&mut self) {
    self.style_dirty = true;
    self.layout_dirty = true;
    self.paint_dirty = true;
    self.dirty_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
  }

  #[inline]
  fn clear_dirty(&mut self) {
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = false;
    self.dirty_style_nodes.clear();
    self.dirty_text_nodes.clear();
    self.dirty_structure_nodes.clear();
  }

  #[inline]
  pub fn is_dirty(&self) -> bool {
    self.style_dirty
      || self.layout_dirty
      || self.paint_dirty
      || self.dom.mutation_generation() != self.last_seen_dom_mutation_generation
  }

  fn apply_mutation_log(&mut self, mutations: crate::dom2::MutationLog) {
    // Treat changes in disconnected/inert subtrees as non-render-affecting.
    for node in mutations.attribute_changed {
      if self.dom.is_connected_for_scripting(node) {
        self.dirty_style_nodes.insert(node);
      }
    }

    for node in mutations.text_changed {
      if !self.dom.is_connected_for_scripting(node) {
        continue;
      }
      // Text changes inside <style> elements affect the stylesheet and require a full restyle.
      if self.text_node_affects_stylesheet(node) {
        self.dirty_style_nodes.insert(node);
      } else {
        self.dirty_text_nodes.insert(node);
      }
    }

    for parent in mutations.child_list_changed {
      if self.dom.is_connected_for_scripting(parent) {
        self.dirty_structure_nodes.insert(parent);
      }
    }

    // Upgrade to the minimal set of coarse invalidation flags we can currently satisfy.
    if !self.dirty_structure_nodes.is_empty() || !self.dirty_style_nodes.is_empty() {
      self.style_dirty = true;
      self.layout_dirty = true;
      self.paint_dirty = true;
      return;
    }

    if !self.dirty_text_nodes.is_empty() {
      self.layout_dirty = true;
      self.paint_dirty = true;
    }
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
    }

    if !updates.is_empty() {
      apply_text_updates_to_box_tree(&mut prepared.box_tree.root, &updates);
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

        crate::render_control::record_stage(crate::render_control::StageHeartbeat::Layout);
        let _layout_span = trace_handle.span("layout_tree", "layout");
        let mut fragment_tree = self
          .renderer
          .layout_engine
          .layout_tree_with_trace(&prepared.box_tree, trace_handle)
          .map_err(super::map_formatting_layout_error)?;
        drop(_layout_span);

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
}

fn apply_text_updates_to_box_tree(root: &mut BoxNode, updates: &FxHashMap<usize, String>) {
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
    let (result, changed) = f(self.dom.as_mut());
    if changed {
      let mutations = self.dom.take_mutations();
      if mutations.is_empty() {
        self.invalidate_all();
      } else {
        self.apply_mutation_log(mutations);
      }
    } else {
      self.dom.clear_mutations();
    }
    result
  }
}

impl webidl_vm_js::WebIdlBindingsHost for BrowserDocumentDom2 {
  fn call_operation(
    &mut self,
    vm: &mut vm_js::Vm,
    scope: &mut vm_js::Scope<'_>,
    receiver: Option<vm_js::Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: &[vm_js::Value],
  ) -> std::result::Result<vm_js::Value, vm_js::VmError> {
    let Some(mut host_ptr) = self.webidl_bindings_host else {
      return Err(vm_js::VmError::Unimplemented(
        "BrowserDocumentDom2 is missing a WebIDL bindings host",
      ));
    };

    // SAFETY: `webidl_bindings_host` is a non-owning pointer set by the surrounding tab/window
    // host. The embedder must guarantee it is valid for the lifetime of this document.
    let host = unsafe { host_ptr.as_mut() };
    host.call_operation(vm, scope, receiver, interface, operation, overload, args)
  }

  fn call_constructor(
    &mut self,
    vm: &mut vm_js::Vm,
    scope: &mut vm_js::Scope<'_>,
    interface: &'static str,
    overload: usize,
    args: &[vm_js::Value],
    new_target: vm_js::Value,
  ) -> std::result::Result<vm_js::Value, vm_js::VmError> {
    let Some(mut host_ptr) = self.webidl_bindings_host else {
      return Err(vm_js::VmError::Unimplemented(
        "BrowserDocumentDom2 is missing a WebIDL bindings host",
      ));
    };

    // SAFETY: see `call_operation`.
    let host = unsafe { host_ptr.as_mut() };
    host.call_constructor(vm, scope, interface, overload, args, new_target)
  }

  fn iterable_snapshot(
    &mut self,
    vm: &mut vm_js::Vm,
    scope: &mut vm_js::Scope<'_>,
    receiver: Option<vm_js::Value>,
    interface: &'static str,
    kind: webidl_vm_js::IterableKind,
  ) -> std::result::Result<Vec<webidl_vm_js::bindings_runtime::BindingValue>, vm_js::VmError> {
    let Some(mut host_ptr) = self.webidl_bindings_host else {
      return Err(vm_js::VmError::Unimplemented(
        "BrowserDocumentDom2 is missing a WebIDL bindings host",
      ));
    };

    // SAFETY: see `call_operation`.
    let host = unsafe { host_ptr.as_mut() };
    host.iterable_snapshot(vm, scope, receiver, interface, kind)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use selectors::context::QuirksMode;

  fn renderer_for_tests() -> super::super::FastRender {
    super::super::FastRender::builder()
      .font_sources(crate::text::font_db::FontConfig::bundled_only())
      .build()
      .expect("renderer")
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

    let clock = Arc::new(crate::js::clock::VirtualClock::new());
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

    let clock = Arc::new(crate::js::clock::VirtualClock::new());
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
          .find(|(name, _)| name.eq_ignore_ascii_case("id"))
          .map(|(_, value)| value.as_str());
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
          .find(|(name, _)| name.eq_ignore_ascii_case("id"))
          .map(|(_, value)| value.as_str());
        assert_eq!(id_attr, Some("after"));
      }
      other => panic!("expected mapped dom2 node to be an element, got {other:?}"),
    }
  }

  #[test]
  fn dom_pointer_is_stable_across_moves_and_changes_on_reset_paths() -> Result<()> {
    let renderer = renderer_for_tests();
    let options = RenderOptions::new().with_viewport(16, 16);
    let mut doc = BrowserDocumentDom2::new(
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

    let prepared = doc.prepare_dom_with_options()?;
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
