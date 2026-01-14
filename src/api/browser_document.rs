use crate::animation::{AnimationTickSchedule, TransitionState};
use crate::clock::{Clock, RealClock};
use crate::debug::runtime::RuntimeToggles;
use crate::dom::DomNode;
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::{Point, Size};
use crate::interaction::{form_controls, InteractionState};
use crate::resource::ReferrerPolicy;
use crate::scroll::anchoring::ScrollAnchoringPriorityCandidate;
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::FragmentTree;
use rustc_hash::FxHashMap;
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use super::{
  resolve_viewport, LayoutDocumentOptions, PreparedDocument, PreparedPaintOptions, RenderOptions,
};

/// Result of a navigation performed via [`BrowserDocument::navigate_url`].
#[derive(Debug, Clone)]
pub struct BrowserNavigationReport {
  /// Diagnostics captured while fetching resources.
  pub diagnostics: super::RenderDiagnostics,
  /// Final document URL after redirects, when available.
  pub final_url: Option<String>,
  /// Effective base URL used to resolve relative subresources (after `<base href>`).
  pub base_url: Option<String>,
}

/// Mutable, multi-frame renderer that caches the most recent layout result.
///
/// `BrowserDocument` owns a [`super::FastRender`] instance and a live DOM tree. DOM mutations
/// invalidate the cached style/layout/paint results, and the next call to [`BrowserDocument::render_if_needed`]
/// recomputes the pipeline once, coalescing all intermediate changes.
///
/// This type does **not** execute JavaScript and does not include an HTML event loop.
/// For a JS-capable runtime (scripts + event loop + navigation), use [`super::BrowserTab`].
pub struct BrowserDocument {
  renderer: super::FastRender,
  dom: DomNode,
  options: RenderOptions,
  document_url: Option<String>,
  prepared: Option<PreparedDocument>,
  animation_state_store: crate::animation::AnimationStateStore,
  style_dirty: bool,
  layout_dirty: bool,
  paint_dirty: bool,
  /// Optional high-priority viewport scroll anchoring candidate supplied by the embedding/UI.
  ///
  /// When set, the document will attempt to keep this candidate stable across relayouts (CSS Scroll
  /// Anchoring §2.2 "anchor priority candidates"), falling back to the default anchor selection when
  /// it is absent or ineligible.
  scroll_anchoring_priority_candidate: Option<ScrollAnchoringPriorityCandidate>,
  /// Hash of the most recently rendered interaction state's CSS-affecting subset.
  ///
  /// This captures pseudo-class matching (`:hover`, `:focus`, etc.) and other inputs that influence
  /// the CSS cascade.
  interaction_css_hash: u64,
  /// Hash of the most recently rendered interaction state's paint-only subset.
  ///
  /// This captures state that affects painting but must not force style/layout invalidation (e.g.
  /// caret/selection/IME/document selection, file-input selection labels).
  interaction_paint_hash: u64,
  realtime_animations_enabled: bool,
  animation_clock: Arc<dyn Clock>,
  animation_timeline_origin: Option<Duration>,
  last_painted_animation_clock: Option<Duration>,
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

fn collect_box_id_to_styled_node_id(box_tree: &BoxTree) -> FxHashMap<usize, usize> {
  let mut mapping: FxHashMap<usize, usize> = FxHashMap::default();
  let mut stack: Vec<&crate::tree::box_tree::BoxNode> = vec![&box_tree.root];
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

fn apply_form_control_paint_state(
  control: &mut crate::tree::box_tree::FormControl,
  node_id: usize,
  interaction_state: Option<&InteractionState>,
) {
  use crate::tree::box_tree::FormControlKind;

  match &mut control.control {
    FormControlKind::Text {
      value,
      caret,
      caret_affinity,
      selection,
      ..
    } => {
      let value_char_len = value.chars().count();
      let (next_caret, next_affinity, next_selection) =
        form_controls::text_edit_state_for_value_char_len(
          interaction_state,
          node_id,
          value_char_len,
        );
      *caret = next_caret;
      *caret_affinity = next_affinity;
      *selection = next_selection;

      control.ime_preedit = form_controls::ime_preedit_for_node(interaction_state, node_id);
    }
    FormControlKind::TextArea {
      value,
      caret,
      caret_affinity,
      selection,
      ..
    } => {
      let value_char_len = value.chars().count();
      let (next_caret, next_affinity, next_selection) =
        form_controls::text_edit_state_for_value_char_len(
          interaction_state,
          node_id,
          value_char_len,
        );
      *caret = next_caret;
      *caret_affinity = next_affinity;
      *selection = next_selection;

      control.ime_preedit = form_controls::ime_preedit_for_node(interaction_state, node_id);
    }
    FormControlKind::File { value } => {
      *value = form_controls::file_input_display_value(interaction_state, node_id);
      control.ime_preedit = None;
    }
    _ => {
      control.ime_preedit = None;
    }
  }
}

fn apply_paint_interaction_state_to_fragment(
  root: &mut crate::tree::fragment_tree::FragmentNode,
  box_id_to_styled_node_id: &FxHashMap<usize, usize>,
  interaction_state: Option<&InteractionState>,
) {
  use crate::tree::box_tree::ReplacedType;
  use crate::tree::fragment_tree::FragmentContent;

  let mut stack: Vec<*mut crate::tree::fragment_tree::FragmentNode> = vec![root as *mut _];
  while let Some(ptr) = stack.pop() {
    // SAFETY: We only push pointers to nodes owned by `root`, and we never mutate a `children`
    // vector while pointers into it are stored in `stack` (we use copy-on-write via
    // `children_mut()` and traverse each node once).
    let node = unsafe { &mut *ptr };

    if let FragmentContent::Replaced {
      replaced_type,
      box_id,
      ..
    } = &mut node.content
    {
      if let ReplacedType::FormControl(control) = replaced_type {
        if let Some(box_id) = *box_id {
          if let Some(node_id) = box_id_to_styled_node_id.get(&box_id).copied() {
            apply_form_control_paint_state(control, node_id, interaction_state);
          }
        }
      }
    }

    if matches!(
      node.content,
      FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
    ) {
      continue;
    }

    for child in node.children_mut().iter_mut().rev() {
      stack.push(child as *mut _);
    }
  }
}

fn apply_paint_interaction_state_to_fragment_tree(
  box_tree: &BoxTree,
  fragment_tree: &mut FragmentTree,
  interaction_state: Option<&InteractionState>,
) {
  // Apply document selection onto the fragment tree for paint-time highlighting.
  crate::interaction::document_selection::apply_document_selection_to_fragment_tree(
    box_tree,
    fragment_tree,
    interaction_state.and_then(|state| state.document_selection.as_ref()),
  );

  let box_id_to_styled_node_id = collect_box_id_to_styled_node_id(box_tree);

  apply_paint_interaction_state_to_fragment(
    &mut fragment_tree.root,
    &box_id_to_styled_node_id,
    interaction_state,
  );
  for root in fragment_tree.additional_fragments.iter_mut() {
    apply_paint_interaction_state_to_fragment(root, &box_id_to_styled_node_id, interaction_state);
  }

  if let Some(existing) = fragment_tree.appearance_none_form_controls.as_ref() {
    let existing = existing.as_ref();
    if !existing.is_empty() {
      let mut updated: HashMap<usize, Arc<crate::tree::box_tree::FormControl>> =
        HashMap::with_capacity(existing.len());
      for (box_id, control_arc) in existing.iter() {
        let mut control = (**control_arc).clone();
        if let Some(node_id) = box_id_to_styled_node_id.get(box_id).copied() {
          apply_form_control_paint_state(&mut control, node_id, interaction_state);
        }
        updated.insert(*box_id, Arc::new(control));
      }
      fragment_tree.appearance_none_form_controls = Some(Arc::new(updated));
    }
  }
}
impl BrowserDocument {
  /// Creates a new live document from an HTML string using a fresh renderer.
  pub fn from_html(html: &str, options: RenderOptions) -> Result<Self> {
    Self::new(super::FastRender::new()?, html, options)
  }

  /// Creates a new live document from an HTML string using the provided renderer.
  pub fn new(renderer: super::FastRender, html: &str, options: RenderOptions) -> Result<Self> {
    // `FastRender::parse_html` cooperatively checks any *active* render deadline, but it does not
    // accept `RenderOptions` directly. Install a temporary deadline so callers can cancel/timeout
    // large HTML parses (e.g. browser UI `about:` pages) via `RenderOptions::{timeout,cancel_callback}`.
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
    // Preserve the renderer's initial document URL hint so later `<base href>` mutations do not
    // accidentally change origin/referrer semantics.
    let document_url = renderer.document_url_hint().map(|url| url.to_string());
    Ok(Self {
      renderer,
      dom,
      options,
      document_url,
      prepared: None,
      animation_state_store: crate::animation::AnimationStateStore::new(),
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
      scroll_anchoring_priority_candidate: None,
      interaction_css_hash: interaction_state_css_fingerprint(None),
      interaction_paint_hash: interaction_state_paint_fingerprint(None),
      realtime_animations_enabled: false,
      animation_clock: Arc::new(RealClock::default()),
      animation_timeline_origin: None,
      last_painted_animation_clock: None,
    })
  }

  /// Creates a new live document from a prepared layout result.
  ///
  /// This constructor avoids reparsing or re-running layout before the first paint, while still
  /// allowing callers to mutate the DOM in-place and re-run the pipeline on demand.
  pub fn from_prepared(
    renderer: super::FastRender,
    prepared: PreparedDocument,
    options: RenderOptions,
  ) -> Result<Self> {
    let dom = prepared.dom.clone();
    let document_url = renderer.document_url_hint().map(|url| url.to_string());
    Ok(Self {
      renderer,
      dom,
      options,
      document_url,
      prepared: Some(prepared),
      animation_state_store: crate::animation::AnimationStateStore::new(),
      style_dirty: false,
      layout_dirty: false,
      // First frame still needs a paint.
      paint_dirty: true,
      scroll_anchoring_priority_candidate: None,
      interaction_css_hash: interaction_state_css_fingerprint(None),
      interaction_paint_hash: interaction_state_paint_fingerprint(None),
      realtime_animations_enabled: false,
      animation_clock: Arc::new(RealClock::default()),
      animation_timeline_origin: None,
      last_painted_animation_clock: None,
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
      self.paint_dirty = true;
    } else if !enabled && self.realtime_animations_enabled {
      self.realtime_animations_enabled = false;
      self.animation_timeline_origin = None;
      self.animation_state_store = crate::animation::AnimationStateStore::new();
      self.last_painted_animation_clock = None;
      self.paint_dirty = true;
    }
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

  /// Updates the renderer's document/base URL hints for the current navigation.
  ///
  /// - `document_url` is used for referrer/origin semantics (after redirects).
  /// - `base_url` is used for resolving relative URLs (and can be overridden by `<base href>`).
  pub fn set_navigation_urls(&mut self, document_url: Option<String>, base_url: Option<String>) {
    // Normalize empty/whitespace-only inputs so we avoid unnecessary state churn.
    let document_url =
      document_url.and_then(|url| (!super::trim_ascii_whitespace(&url).is_empty()).then_some(url));
    let base_url = match base_url {
      Some(url) if !super::trim_ascii_whitespace(&url).is_empty() => Some(url),
      _ => None,
    };

    if self.renderer.document_url != document_url {
      match document_url {
        Some(url) => self.renderer.set_document_url(url),
        None => self.renderer.clear_document_url(),
      }
    }
    if self.renderer.base_url != base_url {
      match base_url {
        Some(url) => self.renderer.set_base_url(url),
        None => self.renderer.clear_base_url(),
      }
    }
  }

  /// Fetches and prepares a URL using the internal renderer, replacing the live document in-place.
  ///
  /// This enables UIs to keep a long-lived `BrowserDocument` (and its internal caches) across
  /// navigations without constructing a new [`super::FastRender`] instance per load.
  pub fn navigate_url(
    &mut self,
    url: &str,
    options: RenderOptions,
  ) -> Result<BrowserNavigationReport> {
    // `prepare_url` updates the renderer's URL hints early (before doing layout). If it errors
    // (e.g. cancellation), restore the previous hints so callers that keep this `BrowserDocument`
    // alive continue to resolve resources/links against the currently committed document.
    let prev_document_url = self.renderer.document_url.clone();
    let prev_base_url = self.renderer.base_url.clone();
    let super::PreparedDocumentReport {
      document,
      diagnostics,
      final_url,
      base_url,
    } = match self.renderer.prepare_url(url, options.clone()) {
      Ok(report) => report,
      Err(err) => {
        self.set_navigation_urls(prev_document_url, prev_base_url);
        return Err(err);
      }
    };

    // Keep the renderer's URL hints consistent with the navigation result. (This is typically a
    // no-op because `prepare_url` already updates the hints, but it ensures callers that manually
    // tweaked them don't drift out of sync.)
    self.set_navigation_urls(final_url.clone(), base_url.clone());

    // Preserve the post-navigation document URL hint so later `<base href>` changes do not alter
    // origin/referrer semantics for subsequent resource fetches.
    let stable_document_url = final_url
      .clone()
      .or_else(|| self.renderer.document_url_hint().map(|url| url.to_string()));
    self.set_document_url(stable_document_url);

    // Swap the live DOM while retaining the renderer instance and its caches.
    self.reset_with_prepared(document, options);

    Ok(BrowserNavigationReport {
      diagnostics,
      final_url,
      base_url,
    })
  }

  /// Prepares and installs an HTML string as the current document using the internal renderer.
  ///
  /// This is the HTML-string equivalent of [`BrowserDocument::navigate_url_with_options`]. It
  /// allows browser-UI integrations to keep a stable `BrowserDocument`/renderer per tab while
  /// rendering internal `about:` pages (or error pages) without constructing a new renderer.
  ///
  /// Returns `(committed_url, base_url)` where:
  /// - `committed_url` is the provided `document_url` (after normalization).
  /// - `base_url` is the effective base used for resolving relative URLs (after `<base href>`),
  ///   falling back to `committed_url` when absent.
  pub fn navigate_html_with_options(
    &mut self,
    document_url: &str,
    html: &str,
    base_url_hint: Option<&str>,
    options: RenderOptions,
  ) -> Result<(String, String)> {
    // Mirror `navigate_url_with_options`: apply the navigation URL hints up-front (so relative URL
    // resolution and resource context semantics match the new document), but restore the previous
    // hints if parsing/preparing fails (including cancellation).
    let prev_document_url = self.renderer.document_url.clone();
    let prev_base_url = self.renderer.base_url.clone();

    // Seed relative URL resolution when the HTML document does not contain `<base href>`.
    match base_url_hint
      .map(super::trim_ascii_whitespace)
      .filter(|url| !url.is_empty())
    {
      Some(url) => self.renderer.set_base_url(url.to_string()),
      None => self.renderer.clear_base_url(),
    }

    // Ensure the resource context sees the provided document URL (used for referrer/origin
    // semantics). Unlike `base_url`, this must remain stable even if `<base href>` mutates.
    let sanitized_document_url = super::trim_ascii_whitespace(document_url);
    if sanitized_document_url.is_empty() {
      self.renderer.clear_document_url();
    } else {
      self
        .renderer
        .set_document_url(sanitized_document_url.to_string());
    }

    // Like `BrowserDocument::new`/`reset_with_html`, install a scoped deadline so callers can
    // cooperatively cancel HTML parsing via `RenderOptions::{timeout,cancel_callback}`.
    //
    // This is important for browser-UI integrations that render internal `about:` pages through
    // this API and may need to abort a large parse when a navigation is superseded.
    let deadline_enabled = options.timeout.is_some() || options.cancel_callback.is_some();
    let dom_result = if deadline_enabled {
      let deadline = crate::render_control::RenderDeadline::new(
        options.timeout,
        options.cancel_callback.clone(),
      );
      let _guard = crate::render_control::DeadlineGuard::install(Some(&deadline));
      self.renderer.parse_html(html)
    } else {
      self.renderer.parse_html(html)
    };
    let dom = match dom_result {
      Ok(dom) => dom,
      Err(err) => {
        self.set_navigation_urls(prev_document_url, prev_base_url);
        return Err(err);
      }
    };

    // Prepare the DOM using the same lightweight pipeline as `BrowserDocument::render_frame*`
    // (which avoids the diagnostics plumbing in `FastRender::prepare_dom_with_options`).
    let prepared = {
      let renderer = &mut self.renderer;
      let toggles = renderer.resolve_runtime_toggles(&options);
      let _toggles_guard =
        super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
      crate::debug::runtime::with_thread_runtime_toggles(toggles, || {
        let trace = super::TraceSession::from_options(Some(&options));
        let trace_handle = trace.handle();
        let _root_span = trace_handle.span("browser_document_prepare_html", "pipeline");

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
        let result = prepare_dom_inner(renderer, &dom, options.clone(), trace_handle, None, None);
        renderer.pop_resource_context(prev_self, prev_image, prev_layout_image, prev_font);
        drop(_root_span);
        trace.finalize(result)
      })
    };

    let prepared = match prepared {
      Ok(prepared) => prepared,
      Err(err) => {
        self.set_navigation_urls(prev_document_url, prev_base_url);
        return Err(err);
      }
    };

    let committed_url = sanitized_document_url.to_string();
    let base_url = self
      .renderer
      .base_url
      .clone()
      .filter(|base| !super::trim_ascii_whitespace(base).is_empty())
      .unwrap_or_else(|| committed_url.clone());

    // Update our stable document URL hint (used for origin/referrer semantics) and the renderer's
    // navigation URL hints for relative URL resolution.
    self.document_url =
      (!super::trim_ascii_whitespace(&committed_url).is_empty()).then_some(committed_url.clone());
    self.set_navigation_urls(Some(committed_url.clone()), Some(base_url.clone()));

    // Install the prepared layout result and mark paint dirty so the next render call produces a
    // frame without re-running layout.
    self.reset_with_prepared(prepared, options);

    Ok((committed_url, base_url))
  }

  /// Replaces the live DOM, clears any cached preparation state, and marks the document dirty.
  pub fn reset_with_dom(&mut self, dom: DomNode, options: RenderOptions) {
    self.dom = dom;
    self.options = options;
    self.prepared = None;
    // Reset per-document CSP state. Callers using `reset_with_dom` are replacing the entire DOM
    // (i.e. a new navigation/document), so carrying over the previous document's CSP would be
    // incorrect once CSP enforcement is enabled for BrowserDocument renders.
    self.renderer.document_csp = None;
    self.animation_timeline_origin = None;
    self.animation_state_store = crate::animation::AnimationStateStore::new();
    self.last_painted_animation_clock = None;
    self.invalidate_all();
  }

  /// Replaces the live DOM with a prepared document's DOM and installs the prepared cache.
  ///
  /// The next `render_if_needed` call will paint using the prepared layout without re-running
  /// cascade/layout.
  pub fn reset_with_prepared(&mut self, prepared: PreparedDocument, options: RenderOptions) {
    self.dom = prepared.dom.clone();
    self.options = options;
    self.prepared = Some(prepared);
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = true;
    self.animation_timeline_origin = None;
    self.animation_state_store = crate::animation::AnimationStateStore::new();
    self.last_painted_animation_clock = None;
  }

  /// Parses HTML using the internal renderer and resets the document state.
  pub fn reset_with_html(&mut self, html: &str, options: RenderOptions) -> Result<()> {
    // Like `BrowserDocument::new`, install a scoped deadline so HTML parsing can be cancelled via
    // `RenderOptions::{timeout,cancel_callback}`. This is particularly important for browser-UI
    // integrations that may cancel a navigation mid-way and immediately enqueue a new one.
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
    self.reset_with_dom(dom, options);
    Ok(())
  }

  /// Navigates this document to a URL using the internal renderer and installs the prepared cache.
  ///
  /// This is intended for browser-UI integrations that want to keep a stable renderer instance per
  /// tab (sharing caches/fetcher) while swapping out the live DOM on navigation.
  ///
  /// Returns `(committed_url, base_url)` where:
  /// - `committed_url` is the final URL after redirects when available (falls back to the input).
  /// - `base_url` is the effective base used for resolving relative URLs (falls back to
  ///   `committed_url` when absent).
  pub fn navigate_url_with_options(
    &mut self,
    url: &str,
    options: RenderOptions,
  ) -> Result<(String, String)> {
    // `prepare_url` updates the renderer's URL hints early (before doing layout). If it errors
    // (e.g. cancellation), restore the previous hints so the existing document continues to have
    // consistent origin/base semantics.
    let prev_document_url = self.renderer.document_url.clone();
    let prev_base_url = self.renderer.base_url.clone();
    let report = match self.renderer.prepare_url(url, options.clone()) {
      Ok(report) => report,
      Err(err) => {
        self.set_navigation_urls(prev_document_url, prev_base_url);
        return Err(err);
      }
    };

    let committed_url = report.final_url.clone().unwrap_or_else(|| url.to_string());
    let base_url = report
      .base_url
      .clone()
      .filter(|base| !super::trim_ascii_whitespace(base).is_empty())
      .unwrap_or_else(|| committed_url.clone());

    // Update our stable document URL hint (used for origin/referrer semantics) and the renderer's
    // navigation URL hints for relative URL resolution.
    self.document_url =
      (!super::trim_ascii_whitespace(&committed_url).is_empty()).then_some(committed_url.clone());
    self.set_navigation_urls(Some(committed_url.clone()), Some(base_url.clone()));

    // Install the prepared layout result and mark paint dirty so the next render call produces a
    // frame without re-running layout.
    self.reset_with_prepared(report.document, options);

    Ok((committed_url, base_url))
  }

  /// Navigate this document using an explicit HTTP method, headers, and optional body.
  ///
  /// This is primarily used for HTML form submission (POST).
  pub fn navigate_http_request_with_options(
    &mut self,
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    options: RenderOptions,
  ) -> Result<(String, String)> {
    // Like `navigate_url_with_options`: if prepare fails (e.g. cancellation), restore the previous
    // URL hints so the existing document continues to have consistent origin/base semantics.
    let prev_document_url = self.renderer.document_url.clone();
    let prev_base_url = self.renderer.base_url.clone();
    let report =
      match self
        .renderer
        .prepare_http_request(url, method, headers, body, options.clone())
      {
        Ok(report) => report,
        Err(err) => {
          self.set_navigation_urls(prev_document_url, prev_base_url);
          return Err(err);
        }
      };

    let committed_url = report.final_url.clone().unwrap_or_else(|| url.to_string());
    let base_url = report
      .base_url
      .clone()
      .filter(|base| !super::trim_ascii_whitespace(base).is_empty())
      .unwrap_or_else(|| committed_url.clone());

    self.document_url =
      (!super::trim_ascii_whitespace(&committed_url).is_empty()).then_some(committed_url.clone());
    self.set_navigation_urls(Some(committed_url.clone()), Some(base_url.clone()));

    self.reset_with_prepared(report.document, options);

    Ok((committed_url, base_url))
  }

  /// Returns an immutable reference to the live DOM tree.
  pub fn dom(&self) -> &DomNode {
    &self.dom
  }

  /// Returns the cached prepared document, if available.
  pub fn prepared(&self) -> Option<&PreparedDocument> {
    self.prepared.as_ref()
  }

  /// Returns a mutable reference to the cached prepared document, if available.
  pub fn prepared_mut(&mut self) -> Option<&mut PreparedDocument> {
    self.prepared.as_mut()
  }

  pub(crate) fn animation_tick_schedule(&mut self, timeline_time_ms: f32) -> AnimationTickSchedule {
    let Some(prepared) = self.prepared.as_ref() else {
      return AnimationTickSchedule::default();
    };
    crate::animation::compute_animation_tick_schedule(
      prepared.fragment_tree(),
      timeline_time_ms,
      Some(&mut self.animation_state_store),
    )
  }

  /// Returns the shared image cache used by this document's renderer.
  ///
  /// This is exposed for browser-UI integrations that want to fetch/decode small extra resources
  /// (e.g. favicons) using the same resource policy, fetcher, and caches as normal page rendering.
  pub fn image_cache(&self) -> &crate::image_loader::ImageCache {
    &self.renderer.image_cache
  }

  pub(crate) fn fetcher(&self) -> Arc<dyn crate::resource::ResourceFetcher> {
    self.renderer.resource_fetcher()
  }

  /// Updates the document URL used for origin/referrer policy decisions.
  ///
  /// This is intentionally distinct from the effective base URL derived from `<base href>`, which
  /// is allowed to change as the DOM mutates.
  pub fn set_document_url(&mut self, url: Option<String>) {
    let sanitized =
      url.and_then(|url| (!super::trim_ascii_whitespace(&url).is_empty()).then_some(url));
    if sanitized != self.document_url {
      self.document_url = sanitized;
      self.invalidate_all();
    }
  }

  /// Updates the document URL used for origin/referrer policy decisions **without** invalidating
  /// cached style/layout/paint state.
  ///
  /// This is intended for same-document fragment navigations (e.g. `#target`) where the document
  /// identity has not changed and the UI wants to reuse the cached layout artifacts for scrolling.
  ///
  /// Note: since this does not mark the document dirty, `:target` styling will only update once a
  /// later operation triggers style/layout invalidation.
  pub fn set_document_url_without_invalidation(&mut self, url: Option<String>) {
    let sanitized =
      url.and_then(|url| (!super::trim_ascii_whitespace(&url).is_empty()).then_some(url));
    if sanitized != self.document_url {
      // Keep the renderer's internal document URL hint in sync so any subsequent fetches or
      // pipeline re-runs use the updated value.
      match sanitized.as_deref() {
        Some(url) => self.renderer.set_document_url(url.to_string()),
        None => self.renderer.clear_document_url(),
      }
      self.document_url = sanitized;
    }
  }

  /// Returns the document URL used for origin/referrer policy decisions.
  pub fn document_url(&self) -> Option<&str> {
    self.document_url.as_deref()
  }

  /// Returns the effective base URL used for resolving relative links, reflecting `<base href>`
  /// after the most recent prepare/layout pass.
  pub fn base_url(&self) -> Option<&str> {
    self.renderer.base_url.as_deref()
  }

  /// Returns a mutable reference to the live DOM tree, marking the document dirty.
  pub fn dom_mut(&mut self) -> &mut DomNode {
    self.invalidate_all();
    &mut self.dom
  }

  /// Mutates the DOM tree, marking the document dirty only when `f` reports that it changed
  /// something.
  pub fn mutate_dom<F>(&mut self, f: F) -> bool
  where
    F: FnOnce(&mut DomNode) -> bool,
  {
    let changed = f(&mut self.dom);
    if changed {
      self.invalidate_all();
    }
    changed
  }

  /// Mutates the DOM tree while granting access to the cached layout artifacts.
  ///
  /// This is primarily intended for UI interaction/hit-testing layers that need to consult the
  /// last computed layout (box + fragment trees) while mutating the live DOM.
  ///
  /// The closure returns `(changed, output)` where `changed` indicates whether the DOM mutation
  /// should invalidate cached style/layout/paint state.
  ///
  /// # Errors
  ///
  /// Returns an error when the document has no cached layout yet (i.e. `render_frame()` has not
  /// been called). Call `render_frame()` first to populate the layout cache.
  pub fn mutate_dom_with_layout_artifacts<F, R>(&mut self, f: F) -> Result<R>
  where
    F: FnOnce(&mut DomNode, &BoxTree, &FragmentTree) -> (bool, R),
  {
    let Some(prepared) = self.prepared.as_ref() else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument has no cached layout; call render_frame() first".to_string(),
      }));
    };

    let (changed, out) = f(&mut self.dom, prepared.box_tree(), prepared.fragment_tree());
    if changed {
      self.invalidate_all();
    }
    Ok(out)
  }

  /// Updates the viewport size (in CSS px), marking layout+paint dirty.
  pub fn set_viewport(&mut self, width: u32, height: u32) {
    self.options.viewport = Some((width, height));
    self.invalidate_layout();
  }

  /// Updates the device pixel ratio used for media queries and resolution-dependent resources.
  ///
  /// Non-finite or non-positive values clear the override (falling back to the renderer default).
  /// Changing DPR invalidates layout+paint.
  pub fn set_device_pixel_ratio(&mut self, dpr: f32) {
    let sanitized = super::sanitize_scale(Some(dpr));
    if sanitized != self.options.device_pixel_ratio {
      self.options.device_pixel_ratio = sanitized;
      self.invalidate_layout();
    }
  }

  /// Returns true when style/layout must be recomputed before painting.
  pub fn needs_layout(&self) -> bool {
    self.prepared.is_none() || self.style_dirty || self.layout_dirty
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
  /// This is intended for dynamic sources whose intrinsic sizing information can change without
  /// any DOM mutations (for example: video metadata becoming available, or aspect ratio updates).
  /// Calling this ensures the next [`render_if_needed`](Self::render_if_needed) recomputes layout
  /// (and then paints) while reusing the cached stylesheet/styled tree when possible.
  ///
  /// This sets `layout_dirty = true` and `paint_dirty = true` while leaving `style_dirty`
  /// unchanged, and does not clear any existing dirtiness flags.
  pub fn invalidate_layout(&mut self) {
    self.layout_dirty = true;
    self.paint_dirty = true;
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
      self.invalidate_paint();
    }
  }

  /// Updates (or clears) the animation/transition sampling timestamp.
  ///
  /// When set to `None`, time-based animations resolve to a deterministic settled state.
  ///
  /// When the value changes, this marks paint dirty (but does not invalidate style/layout).
  pub fn set_animation_time(&mut self, time_ms: Option<f32>) {
    let sanitized = super::sanitize_animation_time_ms(time_ms);
    if sanitized != self.options.animation_time {
      self.options.animation_time = sanitized;
      self.invalidate_paint();
    }
  }

  /// Updates the animation/transition sampling timestamp in milliseconds since load.
  ///
  /// Unlike DOM mutations, updating time only marks the paint stage dirty. This allows callers to
  /// advance the animation clock and request a new frame without rerunning cascade/layout.
  pub fn set_animation_time_ms(&mut self, time_ms: f32) {
    self.set_animation_time(Some(time_ms));
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
      self.invalidate_paint();
    }
  }

  /// Sets (or clears) the priority candidate used by scroll anchoring.
  ///
  /// This is an embedding/UI hint used when the document performs a layout recomputation while the
  /// viewport is scrolled. When set, scroll anchoring will attempt to keep this candidate stable
  /// across the relayout (CSS Scroll Anchoring §2.2).
  ///
  /// The hint does not invalidate style/layout/paint; it is consulted opportunistically during the
  /// next layout pass that requires scroll anchoring.
  pub fn set_scroll_anchoring_priority_candidate(
    &mut self,
    candidate: Option<ScrollAnchoringPriorityCandidate>,
  ) {
    self.scroll_anchoring_priority_candidate = candidate;
  }

  /// Updates the cooperative cancellation callback used during prepare/layout.
  ///
  /// This is a control knob (e.g. for UI-level cancellation) and does not mark the document dirty.
  pub fn set_cancel_callback(
    &mut self,
    cb: Option<std::sync::Arc<crate::render_control::CancelCallback>>,
  ) {
    self.options.cancel_callback = cb;
  }

  /// Updates the hard timeout used during prepare/layout.
  ///
  /// This is a control knob (e.g. for UI-level cancellation) and does not mark the document dirty.
  pub fn set_timeout(&mut self, timeout: Option<std::time::Duration>) {
    self.options.timeout = timeout;
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
    viewport_point_css: crate::geometry::Point,
    delta_css: (f32, f32),
  ) -> crate::Result<bool> {
    let prepared = self.prepared.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument has no cached layout; call render_frame() first".to_string(),
      })
    })?;

    let current_scroll_state = self.scroll_state();
    let page_point_css = viewport_point_css.translate(current_scroll_state.viewport);
    let (delta_x, delta_y) = delta_css;
    let mut next = crate::interaction::scroll_wheel::apply_wheel_scroll_at_point_prepared(
      prepared,
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

  /// Renders a new frame if anything has been invalidated since the last successful frame.
  ///
  /// Returns `Ok(None)` when no dirty flags are set.
  pub fn render_if_needed(&mut self) -> Result<Option<super::Pixmap>> {
    self.render_if_needed_with_interaction_state(None)
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame,
  /// returning the pixmap plus the effective scroll state used during painting.
  pub fn render_if_needed_with_scroll_state(&mut self) -> Result<Option<super::PaintedFrame>> {
    self.render_if_needed_with_scroll_state_and_interaction_state(None)
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame,
  /// applying an optional deadline to the *paint* phase.
  ///
  /// This mirrors [`render_if_needed_with_scroll_state`](Self::render_if_needed_with_scroll_state)
  /// while allowing callers (such as the browser UI worker loop) to provide a cooperative
  /// cancellation deadline for repainting.
  pub fn render_if_needed_with_deadlines(
    &mut self,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<Option<super::PaintedFrame>> {
    self.render_if_needed_with_deadlines_and_interaction_state(paint_deadline, None)
  }

  /// Renders one frame.
  ///
  /// If the document is dirty, this triggers a full pipeline run. Otherwise, it repaints from
  /// cached layout artifacts.
  pub fn render_frame(&mut self) -> Result<super::Pixmap> {
    Ok(
      self
        .render_frame_with_scroll_state_and_interaction_state(None)?
        .pixmap,
    )
  }

  /// Renders one frame, applying an optional deadline to the *paint* phase.
  ///
  /// When layout is required, prepare/layout is executed using the currently configured
  /// `RenderOptions::{timeout,cancel_callback}`, then painting proceeds under `paint_deadline`.
  pub fn render_frame_with_deadlines(
    &mut self,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::PaintedFrame> {
    self.render_frame_with_deadlines_and_interaction_state(paint_deadline, None)
  }

  /// Renders one frame, returning the pixmap plus the effective scroll state used during painting.
  ///
  /// If the document is dirty, this triggers a full pipeline run. Otherwise, it repaints from
  /// cached layout artifacts.
  pub fn render_frame_with_scroll_state(&mut self) -> Result<super::PaintedFrame> {
    self.render_frame_with_deadlines_and_interaction_state(None, None)
  }

  /// Like [`BrowserDocument::render_if_needed`](Self::render_if_needed), but supplies internal
  /// interaction state used for pseudo-class matching and form-control paint hints.
  pub fn render_if_needed_with_interaction_state(
    &mut self,
    interaction_state: Option<&InteractionState>,
  ) -> Result<Option<super::Pixmap>> {
    Ok(
      self
        .render_if_needed_with_scroll_state_and_interaction_state(interaction_state)?
        .map(|frame| frame.pixmap),
    )
  }

  /// Like [`BrowserDocument::render_if_needed_with_scroll_state`](Self::render_if_needed_with_scroll_state),
  /// but supplies internal interaction state used for pseudo-class matching and form-control paint hints.
  pub fn render_if_needed_with_scroll_state_and_interaction_state(
    &mut self,
    interaction_state: Option<&InteractionState>,
  ) -> Result<Option<super::PaintedFrame>> {
    let interaction_css_hash = interaction_state_css_fingerprint(interaction_state);
    let interaction_paint_hash = interaction_state_paint_fingerprint(interaction_state);
    if !self.is_dirty()
      && self.prepared.is_some()
      && interaction_css_hash == self.interaction_css_hash
      && interaction_paint_hash == self.interaction_paint_hash
      && !self.needs_animation_frame()
    {
      return Ok(None);
    }
    let frame = self.render_frame_with_scroll_state_and_interaction_state(interaction_state)?;
    Ok(Some(frame))
  }

  /// Like [`BrowserDocument::render_if_needed_with_deadlines`](Self::render_if_needed_with_deadlines),
  /// but supplies internal interaction state used for pseudo-class matching and form-control paint hints.
  pub fn render_if_needed_with_deadlines_and_interaction_state(
    &mut self,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
    interaction_state: Option<&InteractionState>,
  ) -> Result<Option<super::PaintedFrame>> {
    let interaction_css_hash = interaction_state_css_fingerprint(interaction_state);
    let interaction_paint_hash = interaction_state_paint_fingerprint(interaction_state);
    if !self.is_dirty()
      && self.prepared.is_some()
      && interaction_css_hash == self.interaction_css_hash
      && interaction_paint_hash == self.interaction_paint_hash
      && !self.needs_animation_frame()
    {
      return Ok(None);
    }
    let frame =
      self.render_frame_with_deadlines_and_interaction_state(paint_deadline, interaction_state)?;
    Ok(Some(frame))
  }

  /// Like [`BrowserDocument::render_frame_with_deadlines`](Self::render_frame_with_deadlines), but supplies
  /// internal interaction state used for pseudo-class matching and form-control paint hints.
  pub fn render_frame_with_deadlines_and_interaction_state(
    &mut self,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
    interaction_state: Option<&InteractionState>,
  ) -> Result<super::PaintedFrame> {
    let prev_css_hash = self.interaction_css_hash;
    let prev_paint_hash = self.interaction_paint_hash;
    let base_dirty = self.prepared.is_none() || self.style_dirty || self.layout_dirty;

    let log_enabled = self
      .options
      .runtime_toggles
      .as_ref()
      .unwrap_or(&self.renderer.runtime_toggles)
      .truthy("FASTR_LOG_INTERACTION_INVALIDATION");
    let prev_layout_fingerprint = if log_enabled {
      self
        .prepared
        .as_ref()
        .map(PreparedDocument::layout_style_fingerprint_digest)
    } else {
      None
    };

    let interaction_css_hash = interaction_state_css_fingerprint(interaction_state);
    let interaction_paint_hash = interaction_state_paint_fingerprint(interaction_state);
    let css_changed = interaction_css_hash != prev_css_hash;
    let paint_changed = interaction_paint_hash != prev_paint_hash;

    if css_changed {
      // Interaction state affects pseudo-class matching and other cascade inputs, so we must rerun
      // style (and subsequently paint) when it changes. Do not unconditionally mark layout dirty:
      // hover/focus changes often do not require layout, and paint-only interaction changes should
      // never force cascade/layout.
      self.style_dirty = true;
      self.paint_dirty = true;
    } else if paint_changed {
      // Paint-only interaction changes should only force a repaint from cached layout artifacts.
      self.paint_dirty = true;
    }

    // If we haven't rendered before, force a full pipeline run even if the flags were cleared.
    if self.prepared.is_none() {
      self.invalidate_all();
    }

    let needs_layout = self.style_dirty || self.layout_dirty;
    if needs_layout {
      let prev_prepared = self.prepared.take();
      let mut prepared =
        match self.prepare_dom_with_options_and_interaction_state(interaction_state) {
          Ok(prepared) => prepared,
          Err(err) => {
            self.prepared = prev_prepared;
            return Err(err);
          }
        };

      // Scroll anchoring: adjust the scroll offsets to keep an anchor stable across this relayout.
      //
      // This is a best-effort implementation intended to avoid visible jumps when content above the
      // viewport changes (CSS Scroll Anchoring Module Level 1). When an embedding supplies a
      // priority candidate (e.g. active find-in-page match), anchoring starts from it.
      if let Some(prev_prepared) = prev_prepared.as_ref() {
        let scroll_state = self.scroll_state();
        let snapshot = crate::scroll::anchoring::capture_scroll_anchors_with_priority(
          prev_prepared.fragment_tree(),
          &scroll_state,
          self.scroll_anchoring_priority_candidate,
        );
        let (anchored, _next_snapshot) =
          crate::scroll::apply_scroll_anchoring(&snapshot, prepared.fragment_tree(), &scroll_state);

        let viewport_delta = Point::new(
          anchored.viewport.x - scroll_state.viewport.x,
          anchored.viewport.y - scroll_state.viewport.y,
        );

        let mut element_deltas: HashMap<usize, Point> = HashMap::new();
        for (&id, old_offset) in &scroll_state.elements {
          let new_offset = anchored.elements.get(&id).copied().unwrap_or(Point::ZERO);
          let delta = Point::new(
            new_offset.x - old_offset.x,
            new_offset.y - old_offset.y,
          );
          if delta != Point::ZERO {
            element_deltas.insert(id, delta);
          }
        }
        for (&id, &new_offset) in &anchored.elements {
          if scroll_state.elements.contains_key(&id) {
            continue;
          }
          if new_offset != Point::ZERO {
            element_deltas.insert(id, new_offset);
          }
        }

        self.options.scroll_x = anchored.viewport.x;
        self.options.scroll_y = anchored.viewport.y;
        self.options.scroll_delta = viewport_delta;
        self.options.element_scroll_offsets = anchored.elements.clone();
        self.options.element_scroll_deltas = element_deltas;
      }

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
      // We now have fresh style/layout artifacts stored in `self.prepared`, even if the subsequent
      // paint step is cancelled or fails. Clear the layout dirtiness so callers can retry paint
      // from cache without re-running cascade/layout.
      self.style_dirty = false;
      self.layout_dirty = false;
      // Layout changes always require a paint attempt. Keep paint marked dirty so a cancelled paint
      // can be retried.
      self.paint_dirty = true;
    }

    if log_enabled {
      let layout_fingerprint_matched = if needs_layout {
        let next_layout_fingerprint = self
          .prepared
          .as_ref()
          .map(PreparedDocument::layout_style_fingerprint_digest);
        match (prev_layout_fingerprint, next_layout_fingerprint) {
          (Some(prev), Some(next)) => prev == next,
          _ => false,
        }
      } else {
        true
      };

      let path = if base_dirty {
        "full_prepare"
      } else if css_changed {
        if layout_fingerprint_matched {
          "restyle_reuse_layout"
        } else {
          "restyle_relayout"
        }
      } else {
        "paint_only"
      };

      eprintln!(
        "[interaction-invalidation] path={} css={:#x}->{:#x} paint={:#x}->{:#x} layout_fp_match={}",
        path,
        prev_css_hash,
        interaction_css_hash,
        prev_paint_hash,
        interaction_paint_hash,
        layout_fingerprint_matched
      );
    }

    let frame = self.paint_from_cache_frame_with_deadline_and_interaction_state(
      paint_deadline,
      interaction_state,
    )?;

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }
    // Only commit interaction state hashes after a successful paint, mirroring the existing
    // BrowserDocument semantics.
    self.interaction_css_hash = interaction_css_hash;
    self.interaction_paint_hash = interaction_paint_hash;

    Ok(frame)
  }

  /// Like [`BrowserDocument::render_frame_with_scroll_state`](Self::render_frame_with_scroll_state), but supplies
  /// internal interaction state used for pseudo-class matching and form-control paint hints.
  pub fn render_frame_with_scroll_state_and_interaction_state(
    &mut self,
    interaction_state: Option<&InteractionState>,
  ) -> Result<super::PaintedFrame> {
    self.render_frame_with_deadlines_and_interaction_state(None, interaction_state)
  }

  /// Paints the most recently laid-out document without re-running style/layout.
  ///
  /// This is primarily intended for UI-driven repaints (scrolling, hit-testing highlights, etc)
  /// where the caller wants to provide a [`crate::render_control::RenderDeadline`] for cooperative
  /// cancellation. Callers should check [`BrowserDocument::needs_layout`] first and fall back to
  /// [`BrowserDocument::render_frame_with_scroll_state`] when layout is required.
  pub fn paint_from_cache_frame_with_deadline(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::PaintedFrame> {
    self.paint_from_cache_frame_with_deadline_and_interaction_state(deadline, None)
  }

  /// Paints the most recently laid-out document without re-running style/layout, applying paint-time
  /// interaction-state overlays (document selection + form-control caret/IME).
  pub fn paint_from_cache_frame_with_deadline_and_interaction_state(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
    interaction_state: Option<&InteractionState>,
  ) -> Result<super::PaintedFrame> {
    let animation_time = self.animation_time_for_paint();
    let Some(prepared) = self.prepared.as_ref() else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument has no cached layout; call render_frame() first".to_string(),
      }));
    };

    // Prefer an explicitly provided deadline; otherwise fall back to this document's configured
    // `RenderOptions::{timeout,cancel_callback}`.
    let _deadline_guard = if let Some(deadline) = deadline {
      Some(crate::render_control::DeadlineGuard::install(Some(
        deadline,
      )))
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

    let scroll_state = self.scroll_state();

    // Clone and patch the fragment tree so interaction-state paint overlays do not mutate the
    // cached `PreparedDocument` layout artifacts.
    let mut fragment_tree = prepared.fragment_tree.clone();
    crate::interaction::paint_overlays::apply_interaction_state_paint_overlays_to_fragment_tree(
      prepared.box_tree(),
      &mut fragment_tree,
      prepared.document_selection_index.as_ref(),
      interaction_state,
    );

    let frame = prepared.paint_with_options_frame_with_animation_state_store_and_fragment_tree(
      fragment_tree,
      PreparedPaintOptions {
        scroll: Some(scroll_state),
        viewport: None,
        background: None,
        animation_time,
        media_provider: None,
      },
      &mut self.animation_state_store,
    )?;

    // Keep our internal scroll model synchronized with any adjustments made during painting (e.g.
    // scroll snap/clamp). This must not mark the document dirty because the frame we just painted
    // already reflects this state.
    //
    // IMPORTANT: Painting can clamp and/or snap scroll offsets. When that happens, we need to
    // synchronize both offsets *and* deltas; otherwise `BrowserDocument::scroll_state()` will
    // report offsets that no longer correspond to the stored deltas.
    //
    // Compute deltas relative to the previous internal offsets (the state we asked paint to use)
    // before overwriting the stored offsets.
    let prev_viewport = Point::new(self.options.scroll_x, self.options.scroll_y);
    let effective_viewport = frame.scroll_state.viewport;
    if effective_viewport != prev_viewport {
      let dx = effective_viewport.x - prev_viewport.x;
      let dy = effective_viewport.y - prev_viewport.y;
      self.options.scroll_delta = Point::new(
        if dx.is_finite() { dx } else { 0.0 },
        if dy.is_finite() { dy } else { 0.0 },
      );
    }

    // Compute element scroll deltas for any offsets that paint adjusted (including offsets that
    // were clamped back to 0 and therefore removed from the effective scroll-state map).
    let mut element_changed = false;
    let mut updates: Vec<(usize, Point)> = Vec::new();
    for (&box_id, &effective) in frame.scroll_state.elements.iter() {
      let prev = self
        .options
        .element_scroll_offsets
        .get(&box_id)
        .copied()
        .unwrap_or(Point::ZERO);
      if effective != prev {
        element_changed = true;
        let dx = effective.x - prev.x;
        let dy = effective.y - prev.y;
        let delta = Point::new(
          if dx.is_finite() { dx } else { 0.0 },
          if dy.is_finite() { dy } else { 0.0 },
        );
        updates.push((box_id, delta));
      }
    }
    for (&box_id, &prev) in self.options.element_scroll_offsets.iter() {
      if frame.scroll_state.elements.contains_key(&box_id) {
        continue;
      }
      let effective = Point::ZERO;
      if effective != prev {
        element_changed = true;
        let dx = effective.x - prev.x;
        let dy = effective.y - prev.y;
        let delta = Point::new(
          if dx.is_finite() { dx } else { 0.0 },
          if dy.is_finite() { dy } else { 0.0 },
        );
        updates.push((box_id, delta));
      }
    }
    if element_changed {
      for (box_id, delta) in updates {
        if delta == Point::ZERO {
          self.options.element_scroll_deltas.remove(&box_id);
        } else {
          self.options.element_scroll_deltas.insert(box_id, delta);
        }
      }
      // Canonicalize: omit zero entries.
      self
        .options
        .element_scroll_deltas
        .retain(|_, delta| *delta != Point::ZERO);
    }

    self.options.scroll_x = effective_viewport.x;
    self.options.scroll_y = effective_viewport.y;
    self.options.element_scroll_offsets = frame.scroll_state.elements.clone();

    // A successful paint always satisfies any outstanding paint invalidation, but must not clear
    // pending style/layout dirtiness.
    self.paint_dirty = false;
    if self.realtime_animations_enabled && self.options.animation_time.is_none() {
      self.last_painted_animation_clock = Some(self.animation_clock.now());
    } else {
      self.last_painted_animation_clock = None;
    }

    Ok(frame)
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
    self.prepare_dom_with_options_and_interaction_state(None)
  }

  fn prepare_dom_with_options_and_interaction_state(
    &mut self,
    interaction_state: Option<&InteractionState>,
  ) -> Result<PreparedDocument> {
    let options = self.options.clone();
    let dom = &self.dom;
    let document_url = self.document_url.clone();
    let renderer = &mut self.renderer;

    let toggles = renderer.resolve_runtime_toggles(&options);
    let _toggles_guard =
      super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
    crate::debug::runtime::with_thread_runtime_toggles(toggles, || {
      let trace = super::TraceSession::from_options(Some(&options));
      let trace_handle = trace.handle();
      let _root_span = trace_handle.span("browser_document_prepare", "pipeline");

      // Ensure the resource context sees the stable document URL hint, rather than a potentially
      // mutable `<base href>` override stored in `renderer.base_url`.
      if let Some(url) = document_url.as_deref() {
        renderer.set_document_url(url.to_string());
      } else {
        renderer.clear_document_url();
      }

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
        dom,
        options.clone(),
        trace_handle,
        interaction_state,
        None,
      );
      renderer.pop_resource_context(prev_self, prev_image, prev_layout_image, prev_font);
      drop(_root_span);
      trace.finalize(result)
    })
  }

  #[inline]
  fn invalidate_all(&mut self) {
    self.style_dirty = true;
    self.layout_dirty = true;
    self.paint_dirty = true;
  }

  #[inline]
  fn clear_dirty(&mut self) {
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = false;
  }

  #[inline]
  fn is_dirty(&self) -> bool {
    self.style_dirty || self.layout_dirty || self.paint_dirty
  }
}

pub(super) fn prepare_dom_inner(
  renderer: &mut super::FastRender,
  dom: &DomNode,
  options: RenderOptions,
  trace: &crate::debug::trace::TraceHandle,
  interaction_state: Option<&InteractionState>,
  cascade_reuse: Option<super::CascadeReuse>,
) -> Result<PreparedDocument> {
  let (width, height) = options
    .viewport
    .unwrap_or((renderer.default_width, renderer.default_height));
  if width == 0 || height == 0 {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!("Invalid dimensions: width={width}, height={height}"),
    }));
  }

  // Prefer an already-installed deadline when present so embeddings can share a *single*
  // `RenderDeadline` across multiple phases (e.g. JS execution + multi-frame rendering). Installing
  // a fresh deadline here would reset the start time and effectively grant extra time.
  //
  // When no deadline is active, install one unconditionally (even when disabled) to reset the
  // per-thread interrupt flag for a new render job.
  let active_deadline = crate::render_control::active_deadline();
  let deadline = active_deadline.clone().unwrap_or_else(|| {
    crate::render_control::RenderDeadline::new(options.timeout, options.cancel_callback.clone())
  });
  let _deadline_guard = active_deadline
    .is_none()
    .then(|| crate::render_control::DeadlineGuard::install(Some(&deadline)));

  // Ensure cooperative cancellation is observable even if the subsequent stage preamble (base URL
  // / referrer policy extraction) finishes quickly without checking the deadline.
  crate::render_control::check_active(RenderStage::DomParse).map_err(Error::Render)?;

  // `BrowserDocument`/`BrowserDocumentDom2` share a long-lived `FastRender` instance across multiple
  // navigations. Unlike `FastRender::prepare_html`/`prepare_url`, the BrowserDocument pipeline
  // builds a fresh `ResourceContext` for each layout pass, so we must explicitly seed it with the
  // document-level CSP (e.g. header-delivered CSP from `prepare_url`).
  //
  // Additionally, scan for `<meta http-equiv="Content-Security-Policy">` so DOM-based entrypoints
  // enforce CSP consistently with HTML-string entrypoints.
  if let Some(doc_csp) = renderer.document_csp.clone() {
    if let Some(mut ctx) = renderer.resource_context.clone() {
      let needs_update = ctx.csp.as_ref() != Some(&doc_csp);
      if needs_update {
        ctx.csp = Some(doc_csp);
        renderer.push_resource_context(Some(ctx));
      }
    }
  }

  renderer.update_base_url_from_dom(dom);
  if let Some(policy) = crate::html::referrer_policy::extract_referrer_policy_with_deadline(dom)? {
    let needs_update = renderer
      .resource_context
      .as_ref()
      .is_some_and(|ctx| ctx.referrer_policy != policy);
    if needs_update {
      if let Some(mut ctx) = renderer.resource_context.clone() {
        ctx.referrer_policy = policy;
        // Propagate the updated policy to all caches/fetchers that hold a copy of the current
        // resource context.
        renderer.push_resource_context(Some(ctx));
      }
    }
  }

  if let Some(meta_csp) = crate::html::content_security_policy::extract_csp_with_deadline(dom)? {
    // Persist CSP at the document level so follow-up layout passes (and any host integrations that
    // consult `FastRender::document_csp`) observe the parsed policy.
    match renderer.document_csp.as_mut() {
      Some(existing) => {
        existing.extend(meta_csp.clone());
      }
      None => {
        renderer.document_csp = Some(meta_csp.clone());
      }
    }

    if let Some(mut ctx) = renderer.resource_context.clone() {
      let changed = match ctx.csp.as_mut() {
        Some(existing) => existing.extend(meta_csp),
        None => {
          ctx.csp = Some(meta_csp);
          true
        }
      };
      if changed {
        renderer.push_resource_context(Some(ctx));
      }
    }
  }

  let requested_viewport = Size::new(width as f32, height as f32);
  let base_dpr = options
    .device_pixel_ratio
    .unwrap_or(renderer.device_pixel_ratio);
  let meta_viewport = if renderer.apply_meta_viewport {
    crate::html::viewport::extract_viewport_with_deadline(dom)?
  } else {
    None
  };
  let resolved_viewport = resolve_viewport(requested_viewport, base_dpr, meta_viewport.as_ref());
  let layout_width = resolved_viewport.layout_viewport.width.max(1.0).round() as u32;
  let layout_height = resolved_viewport.layout_viewport.height.max(1.0).round() as u32;
  let paint_parallelism = renderer.resolve_paint_parallelism(&options);
  let layout_parallelism = renderer.resolve_layout_parallelism(&options);

  let previous_dpr = renderer.device_pixel_ratio;
  let artifacts_result = (|| -> Result<super::LayoutArtifacts> {
    renderer.device_pixel_ratio = resolved_viewport.device_pixel_ratio;
    let scroll_state = ScrollState::from_parts_with_deltas(
      Point::new(options.scroll_x, options.scroll_y),
      options.element_scroll_offsets.clone(),
      options.scroll_delta,
      options.element_scroll_deltas.clone(),
    );
    renderer.layout_document_for_media_with_artifacts(
      dom,
      interaction_state,
      layout_width,
      layout_height,
      options.media_type,
      LayoutDocumentOptions {
        page_stacking: super::PageStacking::Stacked { gap: 0.0 },
        animation_time: options.animation_time,
        treat_custom_elements_as_defined: options.treat_custom_elements_as_defined,
      },
      &scroll_state,
      Some(&deadline),
      options.stage_mem_budget_bytes,
      trace,
      layout_parallelism,
      None,
      None,
      None,
      cascade_reuse,
    )
  })();

  renderer.device_pixel_ratio = previous_dpr;
  let artifacts = artifacts_result?;

  let layout_viewport = artifacts.fragment_tree.viewport_size();
  let paint_viewport = Size::new(layout_width as f32, layout_height as f32);
  let layout_style_fingerprint_digest =
    super::styled_layout_fingerprint_digest(&artifacts.styled_tree);
  let document_selection_index = Arc::new(
    crate::interaction::document_selection::DocumentSelectionIndex::build(
      &artifacts.box_tree,
      &artifacts.fragment_tree,
    ),
  );
  Ok(PreparedDocument {
    dom: artifacts.dom,
    stylesheet: artifacts.stylesheet,
    styled_tree: artifacts.styled_tree,
    layout_style_fingerprint_digest,
    box_tree: artifacts.box_tree,
    fragment_tree: artifacts.fragment_tree,
    document_selection_index,
    layout_viewport,
    paint_viewport,
    visual_viewport: resolved_viewport.visual_viewport,
    device_pixel_ratio: resolved_viewport.device_pixel_ratio,
    page_zoom: resolved_viewport.zoom,
    background_color: renderer.background_color,
    default_scroll: ScrollState::from_parts_with_deltas(
      Point::new(options.scroll_x, options.scroll_y),
      options.element_scroll_offsets.clone(),
      options.scroll_delta,
      options.element_scroll_deltas.clone(),
    ),
    animation_time: options.animation_time,
    font_context: renderer.font_context.clone(),
    image_cache: renderer.image_cache.clone(),
    max_iframe_depth: renderer.max_iframe_depth,
    paint_parallelism,
    runtime_toggles: std::sync::Arc::clone(&renderer.runtime_toggles),
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom::DomNodeType;
  use crate::render_control::{push_stage_listener, RenderDeadline, StageHeartbeat};
  use crate::text::font_db::FontConfig;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex};
  use std::time::Duration;
  use tiny_skia::PremultipliedColorU8;

  fn capture_stages<T>(f: impl FnOnce() -> Result<T>) -> Result<Vec<StageHeartbeat>> {
    let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_for_listener = Arc::clone(&stages);
    let _guard = push_stage_listener(Some(Arc::new(move |stage| {
      stages_for_listener.lock().unwrap().push(stage);
    })));
    let _ = f()?;
    let captured = stages.lock().unwrap().clone();
    Ok(captured)
  }

  fn capture_stages_with_output<T>(
    f: impl FnOnce() -> Result<T>,
  ) -> Result<(T, Vec<StageHeartbeat>)> {
    let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_for_listener = Arc::clone(&stages);
    let _guard = push_stage_listener(Some(Arc::new(move |stage| {
      stages_for_listener.lock().unwrap().push(stage);
    })));
    let output = f()?;
    let captured = stages.lock().unwrap().clone();
    Ok((output, captured))
  }

  fn find_first_element_preorder_id(dom: &DomNode, tag: &str) -> Option<usize> {
    let ids = crate::dom::enumerate_dom_ids(dom);
    let mut stack: Vec<&DomNode> = vec![dom];
    while let Some(node) = stack.pop() {
      if let DomNodeType::Element { tag_name, .. } = &node.node_type {
        if tag_name.eq_ignore_ascii_case(tag) {
          return ids.get(&(node as *const DomNode)).copied();
        }
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
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

  fn extract_first_form_control_from_fragment_tree(
    fragment_tree: &crate::tree::fragment_tree::FragmentTree,
  ) -> Option<crate::tree::box_tree::FormControl> {
    use crate::tree::box_tree::ReplacedType;
    use crate::tree::fragment_tree::FragmentContent;

    let mut stack: Vec<&crate::tree::fragment_tree::FragmentNode> = Vec::new();
    stack.push(&fragment_tree.root);
    for root in fragment_tree.additional_fragments.iter() {
      stack.push(root);
    }
    while let Some(node) = stack.pop() {
      if let FragmentContent::Replaced { replaced_type, .. } = &node.content {
        if let ReplacedType::FormControl(control) = replaced_type {
          return Some(control.clone());
        }
      }

      if matches!(
        node.content,
        FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
      ) {
        continue;
      }

      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
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

    let mut document =
      BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(200, 40))?;

    let base_state = InteractionState::default();
    let frame0 =
      document.render_frame_with_scroll_state_and_interaction_state(Some(&base_state))?;
    assert_eq!(
      count_document_selection_pixels(&frame0.pixmap),
      0,
      "expected no selection pixels before selection is applied"
    );

    let mut selected_state = base_state.clone();
    selected_state.set_document_selection(Some(
      crate::interaction::state::DocumentSelectionState::All,
    ));

    let (frame1, stages) = capture_stages_with_output(|| {
      document
        .render_if_needed_with_scroll_state_and_interaction_state(Some(&selected_state))?
        .ok_or_else(|| {
          Error::Other("expected render_if_needed to repaint for selection".to_string())
        })
    })?;

    assert!(
      !stages.contains(&StageHeartbeat::Cascade)
        && !stages.contains(&StageHeartbeat::BoxTree)
        && !stages.contains(&StageHeartbeat::Layout),
      "expected no cascade/box-tree/layout stages; got {stages:?}"
    );
    assert!(
      stages.contains(&StageHeartbeat::PaintBuild)
        || stages.contains(&StageHeartbeat::PaintRasterize),
      "expected paint stage heartbeats; got {stages:?}"
    );
    assert!(
      count_document_selection_pixels(&frame1.pixmap) > 0,
      "expected selection highlight pixels after selection is applied"
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
      <input value="aaaaaaaaaa">
    "#;

    let mut document =
      BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(320, 40))?;
    let input_id = find_first_element_preorder_id(document.dom(), "input")
      .expect("expected to find <input> preorder id");

    let mut state_end = InteractionState::default();
    state_end.set_focused(Some(input_id));
    state_end.set_focus_chain(vec![input_id]);
    state_end.set_text_edit(Some(crate::interaction::state::TextEditPaintState {
      node_id: input_id,
      caret: 10,
      caret_affinity: CaretAffinity::Downstream,
      selection: None,
    }));

    let frame_end =
      document.render_frame_with_scroll_state_and_interaction_state(Some(&state_end))?;
    let caret_end = caret_red_x_range(&frame_end.pixmap).expect("expected caret pixels");

    let mut state_start = state_end.clone();
    if let Some(edit) = state_start.text_edit_mut().as_mut() {
      edit.caret = 0;
    }

    let (frame_start, stages) = capture_stages_with_output(|| {
      document
        .render_if_needed_with_scroll_state_and_interaction_state(Some(&state_start))?
        .ok_or_else(|| Error::Other("expected caret change to invalidate and repaint".to_string()))
    })?;

    assert!(
      !stages.contains(&StageHeartbeat::Cascade)
        && !stages.contains(&StageHeartbeat::BoxTree)
        && !stages.contains(&StageHeartbeat::Layout),
      "expected no cascade/box-tree/layout stages; got {stages:?}"
    );
    assert!(
      stages.contains(&StageHeartbeat::PaintBuild)
        || stages.contains(&StageHeartbeat::PaintRasterize),
      "expected paint stage heartbeats; got {stages:?}"
    );

    let caret_start = caret_red_x_range(&frame_start.pixmap).expect("expected caret pixels");
    assert!(
      caret_start.0 + 5 < caret_end.0,
      "expected caret x to move left; start={caret_start:?}, end={caret_end:?}"
    );
    Ok(())
  }

  #[test]
  fn form_control_ime_preedit_propagates_into_paint_state() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        html, body { margin: 0; background: white; }
        input {
          font: 24px monospace;
          border: 0;
          padding: 0;
          margin: 0;
          background: white;
          color: black;
        }
      </style>
      <input value="hello">
    "#;
    let mut document =
      BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(320, 40))?;
    let input_id = find_first_element_preorder_id(document.dom(), "input")
      .expect("expected to find <input> preorder id");

    // Simulate the typical cached paint flow:
    // - Layout was already performed with focus state (CSS-affecting).
    // - Then IME preedit begins (paint-only).
    let mut focused_state = InteractionState::default();
    focused_state.set_focused(Some(input_id));
    focused_state.set_focus_chain(vec![input_id]);
    let prepared = document.prepare_dom_with_options_and_interaction_state(Some(&focused_state))?;

    let mut preedit_state = focused_state.clone();
    preedit_state.set_ime_preedit(Some(crate::interaction::ImePreeditState {
      node_id: input_id,
      text: "abc".to_string(),
      cursor: Some((1, 2)),
    }));

    let mut fragment_tree = prepared.fragment_tree().clone();
    crate::interaction::paint_overlays::apply_form_control_paint_overlays_to_fragment_tree(
      prepared.box_tree(),
      &mut fragment_tree,
      Some(&preedit_state),
    );

    let control = extract_first_form_control_from_fragment_tree(&fragment_tree)
      .expect("expected to find a form control in fragment tree");
    assert!(
      control.focused,
      "expected form control to remain focused after overlay patching"
    );
    assert_eq!(
      control.ime_preedit,
      Some(crate::tree::box_tree::ImePreeditPaintState {
        text: "abc".to_string(),
        cursor: Some((1, 2)),
      })
    );
    Ok(())
  }

  #[test]
  fn reset_with_prepared_skips_layout_on_first_paint() -> Result<()> {
    let mut renderer = renderer_for_tests();
    let options = RenderOptions::default().with_viewport(64, 64);
    let prepared = renderer.prepare_html("<div>hi</div>", options.clone())?;

    let mut document = BrowserDocument::from_prepared(renderer, prepared, options.clone())?;
    let stages = capture_stages(|| document.render_if_needed().map(|_| ()))?;

    assert!(
      !stages.contains(&StageHeartbeat::Layout),
      "expected no layout stage; got {stages:?}"
    );
    assert!(
      stages.contains(&StageHeartbeat::PaintBuild)
        || stages.contains(&StageHeartbeat::PaintRasterize),
      "expected paint stage heartbeats; got {stages:?}"
    );
    Ok(())
  }

  #[test]
  fn reset_with_html_clears_prepared_and_triggers_layout() -> Result<()> {
    let mut renderer = renderer_for_tests();
    let options = RenderOptions::default().with_viewport(64, 64);
    let prepared = renderer.prepare_html("<div>old</div>", options.clone())?;

    let mut document = BrowserDocument::from_prepared(renderer, prepared, options.clone())?;
    document.reset_with_html("<div>new</div>", options.clone())?;

    let stages = capture_stages(|| document.render_if_needed().map(|_| ()))?;
    assert!(
      stages.contains(&StageHeartbeat::Layout),
      "expected layout stage after reset_with_html; got {stages:?}"
    );
    Ok(())
  }

  #[test]
  fn non_ascii_whitespace_set_navigation_urls_does_not_trim_nbsp_base_url() -> Result<()> {
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      "<div>hi</div>",
      RenderOptions::default(),
    )?;
    let nbsp = "\u{00A0}".to_string();
    document.set_navigation_urls(None, Some(nbsp.clone()));
    assert_eq!(document.renderer.base_url.as_deref(), Some(nbsp.as_str()));
    Ok(())
  }

  #[test]
  fn csp_style_src_attr_meta_blocks_style_attributes_and_does_not_leak_across_reset() -> Result<()>
  {
    let options = RenderOptions::default().with_viewport(20, 20);
    let html_blocked = r#"<!doctype html>
      <html>
        <head>
          <meta http-equiv="Content-Security-Policy" content="style-src-attr 'none'">
          <style>
            html, body { margin: 0; background: rgb(0, 255, 0); }
            #box { width: 10px; height: 10px; }
          </style>
        </head>
        <body>
          <div id="box" style="background: rgb(255, 0, 0);"></div>
        </body>
      </html>"#;
    let html_allowed = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; background: rgb(0, 255, 0); }
            #box { width: 10px; height: 10px; }
          </style>
        </head>
        <body>
          <div id="box" style="background: rgb(255, 0, 0);"></div>
        </body>
      </html>"#;

    let mut document = BrowserDocument::new(renderer_for_tests(), html_blocked, options.clone())?;
    let pixmap_blocked = document.render_frame()?;
    let blocked = pixmap_blocked.pixel(5, 5).expect("pixel 5,5");
    assert_eq!(
      [
        blocked.red(),
        blocked.green(),
        blocked.blue(),
        blocked.alpha()
      ],
      [0, 255, 0, 255],
      "expected style attribute to be blocked by CSP meta"
    );

    document.reset_with_html(html_allowed, options.clone())?;
    let pixmap_allowed = document.render_frame()?;
    let allowed = pixmap_allowed.pixel(5, 5).expect("pixel 5,5");
    assert_eq!(
      [
        allowed.red(),
        allowed.green(),
        allowed.blue(),
        allowed.alpha()
      ],
      [255, 0, 0, 255],
      "expected CSP state not to leak across reset_with_html"
    );

    Ok(())
  }

  #[test]
  fn set_device_pixel_ratio_triggers_layout() -> Result<()> {
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      "<div>hi</div>",
      RenderOptions::default().with_viewport(32, 32),
    )?;
    document.render_frame()?;

    document.set_device_pixel_ratio(2.0);
    let stages = capture_stages(|| document.render_if_needed().map(|_| ()))?;

    assert!(
      stages.contains(&StageHeartbeat::Layout),
      "expected layout stage after DPR change; got {stages:?}"
    );
    Ok(())
  }

  #[test]
  fn invalidate_paint_triggers_repaint_without_layout() -> Result<()> {
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      "<div>hi</div>",
      RenderOptions::default().with_viewport(32, 32),
    )?;

    // Prime the layout cache.
    document.render_frame()?;
    assert!(
      !document.needs_layout(),
      "expected needs_layout to be false after first render"
    );

    document.invalidate_paint();
    assert!(
      !document.needs_layout(),
      "expected invalidate_paint to not mark layout dirty"
    );

    let stages = capture_stages(|| {
      let painted = document.render_if_needed()?;
      assert!(painted.is_some(), "expected render_if_needed to repaint");
      Ok(())
    })?;
    assert!(
      !stages.contains(&StageHeartbeat::Layout),
      "expected paint-only rerender; got {stages:?}"
    );
    assert!(
      stages.contains(&StageHeartbeat::PaintBuild)
        || stages.contains(&StageHeartbeat::PaintRasterize),
      "expected paint stage heartbeats; got {stages:?}"
    );

    Ok(())
  }

  #[test]
  fn needs_layout_transitions() -> Result<()> {
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      "<div>hi</div>",
      RenderOptions::default().with_viewport(32, 32),
    )?;

    assert!(
      document.needs_layout(),
      "expected needs_layout before first render"
    );
    document.render_frame()?;
    assert!(
      !document.needs_layout(),
      "expected needs_layout to be false after first render"
    );

    document.set_device_pixel_ratio(2.0);
    assert!(
      document.needs_layout(),
      "expected needs_layout to be true after DPR change"
    );
    Ok(())
  }

  #[test]
  fn paint_from_cache_frame_with_deadline_can_cancel() -> Result<()> {
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      "<div>hi</div>",
      RenderOptions::default().with_viewport(32, 32),
    )?;
    document.render_frame()?;

    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    let deadline = RenderDeadline::new(None, Some(cancel));
    let err = match document.paint_from_cache_frame_with_deadline(Some(&deadline)) {
      Ok(_) => panic!("expected paint to be cancelled"),
      Err(err) => err,
    };
    assert!(
      matches!(
        err,
        Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected paint timeout error, got {err:?}"
    );
    Ok(())
  }

  #[test]
  fn render_frame_with_deadlines_cancels_layout_via_cancel_callback() -> Result<()> {
    let options = RenderOptions::default().with_viewport(32, 32);
    let mut document = BrowserDocument::new(renderer_for_tests(), "<div>hi</div>", options)?;

    let cb: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    document.set_cancel_callback(Some(cb));

    let err = match document.render_frame_with_deadlines(None) {
      Ok(_) => panic!("expected cancellation error"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::DomParse);
      }
      other => panic!("expected RenderError::Timeout; got {other:?}"),
    }
    Ok(())
  }

  #[test]
  fn render_frame_with_deadlines_cancels_paint_via_paint_deadline() -> Result<()> {
    let options = RenderOptions::default().with_viewport(32, 32);
    let mut document = BrowserDocument::new(renderer_for_tests(), "<div>hi</div>", options)?;

    // Prime the layout cache.
    let _ = document.render_frame_with_deadlines(None)?;

    // Mark paint dirty.
    document.set_scroll(1.0, 0.0);

    let cb: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    let paint_deadline = RenderDeadline::new(None, Some(cb));
    let err = match document.render_frame_with_deadlines(Some(&paint_deadline)) {
      Ok(_) => panic!("expected cancellation error"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("expected RenderError::Timeout; got {other:?}"),
    }
    Ok(())
  }

  #[test]
  fn render_frame_with_deadlines_caches_layout_on_paint_cancel() -> Result<()> {
    let options = RenderOptions::default().with_viewport(32, 32);
    let mut document = BrowserDocument::from_html("<div>hi</div>", options)?;

    // Cancel the first paint. Layout should still complete and be cached so the next render can
    // repaint without rerunning layout.
    let cb: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    let paint_deadline = RenderDeadline::new(None, Some(cb));
    let err = match document.render_frame_with_deadlines(Some(&paint_deadline)) {
      Ok(_) => panic!("expected paint to be cancelled"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("expected RenderError::Timeout; got {other:?}"),
    }

    let stages = capture_stages(|| document.render_frame_with_deadlines(None).map(|_| ()))?;
    assert!(
      !stages.contains(&StageHeartbeat::Layout),
      "expected cached layout reuse after paint cancellation; got {stages:?}"
    );
    assert!(
      stages.contains(&StageHeartbeat::PaintBuild)
        || stages.contains(&StageHeartbeat::PaintRasterize),
      "expected paint stage heartbeats; got {stages:?}"
    );
    Ok(())
  }

  #[test]
  fn paint_clamps_programmatic_viewport_scroll_to_bounds_excluding_fixed() -> Result<()> {
    let html = r#"<!doctype html>
 <html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #spacer { height: 2000px; }
      #fixed { position: fixed; top: 50000px; width: 10px; height: 10px; }
    </style>
  </head>
  <body>
    <div id="spacer"></div>
    <div id="fixed"></div>
  </body>
</html>"#;
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      html,
      RenderOptions::default().with_viewport(100, 100),
    )?;
    // Prime the layout cache so we can repaint after changing scroll offsets.
    document.render_frame()?;

    // Overshoot far beyond the real scroll range; the viewport-fixed element should not inflate the
    // maximum scroll offset.
    document.set_scroll(0.0, 50000.0);
    let frame = document.paint_from_cache_frame_with_deadline(None)?;

    let expected_max_y = 2000.0 - 100.0;
    assert!(
      (frame.scroll_state.viewport.y - expected_max_y).abs() < 0.5,
      "expected scroll_y to clamp to {expected_max_y}, got {}",
      frame.scroll_state.viewport.y
    );
    assert!(
      (document.scroll_state().viewport.y - expected_max_y).abs() < 0.5,
      "expected BrowserDocument scroll state to be updated to {expected_max_y}, got {}",
      document.scroll_state().viewport.y
    );
    Ok(())
  }

  #[test]
  fn paint_syncs_viewport_scroll_delta_when_scroll_snap_adjusts_offset() -> Result<()> {
    let html = r#"<!doctype html>
 <html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; height: 100%; scroll-snap-type: y mandatory; }
      section { height: 100px; scroll-snap-align: start; }
    </style>
  </head>
  <body>
    <section></section>
    <section></section>
  </body>
 </html>"#;
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      html,
      RenderOptions::default().with_viewport(100, 100),
    )?;

    // Prime the layout cache.
    document.render_frame_with_scroll_state()?;

    // Provide a scroll state without a delta (simulating callers that only set offsets).
    let requested_y = 60.0;
    document.set_scroll_state(ScrollState::with_viewport(Point::new(0.0, requested_y)));

    // Paint-from-cache should snap to the nearest scroll-snap target and synchronize deltas to the
    // effective scroll offset.
    document.render_frame_with_scroll_state()?;

    let state = document.scroll_state();
    assert!(
      (state.viewport.y - 100.0).abs() < 0.5,
      "expected scroll-snap to snap viewport.y to 100, got {}",
      state.viewport.y
    );
    let expected_delta_y = state.viewport.y - requested_y;
    assert!(
      (state.viewport_delta.y - expected_delta_y).abs() < 0.5 && state.viewport_delta.y.abs() > 0.1,
      "expected viewport_delta.y to reflect paint-time snap adjustment (~{expected_delta_y}), got {}",
      state.viewport_delta.y
    );
    Ok(())
  }

  #[test]
  fn paint_clamps_programmatic_element_scroll_to_bounds() -> Result<()> {
    let html = r#"<!doctype html>
 <html>
   <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #scroller { width: 100px; height: 100px; overflow-y: scroll; }
      #content { height: 2000px; }
    </style>
  </head>
  <body>
    <div id="scroller"><div id="content"></div></div>
  </body>
</html>"#;
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      html,
      RenderOptions::default().with_viewport(200, 200),
    )?;
    // Prime the layout cache so we can scroll and repaint.
    document.render_frame()?;

    // Scroll the element once so we learn its box id from the produced scroll state.
    assert!(
      document.wheel_scroll_at_viewport_point(Point::new(10.0, 10.0), (0.0, 10.0))?,
      "expected wheel scroll to affect the scroll container"
    );
    let initial = document.scroll_state();
    assert!(
      initial.elements.len() == 1,
      "expected exactly one element scroll offset; got {:?}",
      initial.elements
    );
    let (&box_id, _) = initial
      .elements
      .iter()
      .next()
      .expect("expected element scroll state");

    // Force the element scroll offset far beyond the scrollable range and ensure paint clamps it.
    let mut next = initial.clone();
    next.elements.insert(box_id, Point::new(0.0, 50000.0));
    document.set_scroll_state(next);

    let frame = document.paint_from_cache_frame_with_deadline(None)?;
    let expected_max_y = 2000.0 - 100.0;
    let painted_y = frame.scroll_state.element_offset(box_id).y;
    assert!(
      (painted_y - expected_max_y).abs() < 0.5,
      "expected element scroll_y to clamp to {expected_max_y}, got {painted_y}"
    );
    let stored_y = document.scroll_state().element_offset(box_id).y;
    assert!(
      (stored_y - expected_max_y).abs() < 0.5,
      "expected BrowserDocument element scroll to sync to {expected_max_y}, got {stored_y}"
    );
    Ok(())
  }

  #[test]
  fn base_url_reflects_base_href_after_prepare() -> Result<()> {
    fn set_base_href(dom: &mut DomNode, href: &str) -> bool {
      let mut stack = vec![dom];
      while let Some(node) = stack.pop() {
        if let crate::dom::DomNodeType::Element {
          tag_name,
          attributes,
          ..
        } = &mut node.node_type
        {
          if tag_name.eq_ignore_ascii_case("base") {
            if let Some((_, value)) = attributes
              .iter_mut()
              .find(|(name, _)| name.eq_ignore_ascii_case("href"))
            {
              *value = href.to_string();
            } else {
              attributes.push(("href".to_string(), href.to_string()));
            }
            return true;
          }
        }
        for child in node.children.iter_mut() {
          stack.push(child);
        }
      }
      false
    }

    let options = RenderOptions::default().with_viewport(32, 32);
    let html = r#"<!doctype html><html><head><base href="https://example.com/base/"></head><body><a href="x">x</a></body></html>"#;
    let mut document = BrowserDocument::new(renderer_for_tests(), html, options)?;
    let _ = document.render_frame_with_deadlines(None)?;
    assert_eq!(document.base_url(), Some("https://example.com/base/"));

    let changed = document.mutate_dom(|dom| set_base_href(dom, "https://example.com/next/"));
    assert!(changed, "expected DOM mutation to update <base href>");

    let _ = document.render_frame_with_deadlines(None)?;
    assert_eq!(document.base_url(), Some("https://example.com/next/"));
    Ok(())
  }

  #[test]
  fn navigate_url_updates_live_document_in_place() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let file_path = temp_dir.path().join("index.html");
    std::fs::write(
      &file_path,
      "<!doctype html><html><head><title>New Title</title></head><body>hello</body></html>",
    )?;
    let file_url = url::Url::from_file_path(&file_path)
      .expect("file url")
      .to_string();

    let renderer = super::super::FastRender::builder()
      .font_sources(crate::text::font_db::FontConfig::bundled_only())
      .build()?;

    let mut document = BrowserDocument::new(
      renderer,
      "<!doctype html><html><head><title>Old Title</title></head><body>old</body></html>",
      RenderOptions::new().with_viewport(64, 64),
    )?;

    let report = document.navigate_url(&file_url, RenderOptions::new().with_viewport(64, 64))?;

    // Ensure we can paint the newly navigated document from the prepared cache.
    document.render_frame()?;
    assert_eq!(
      crate::html::title::find_document_title(document.dom()),
      Some("New Title".to_string())
    );

    assert_eq!(report.final_url.as_deref(), Some(file_url.as_str()));
    assert_eq!(report.base_url.as_deref(), Some(file_url.as_str()));
    assert_eq!(document.document_url(), Some(file_url.as_str()));
    Ok(())
  }

  #[test]
  fn navigate_url_restores_navigation_urls_on_cancel() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let file_path = temp_dir.path().join("index.html");
    std::fs::write(
      &file_path,
      "<!doctype html><html><head><title>Cancelled</title></head><body>hello</body></html>",
    )?;
    let file_url = url::Url::from_file_path(&file_path)
      .expect("file url")
      .to_string();

    let renderer = renderer_for_tests();
    let mut document = BrowserDocument::new(
      renderer,
      "<!doctype html><html><head><title>Old</title></head><body>old</body></html>",
      RenderOptions::new().with_viewport(64, 64),
    )?;
    document.set_document_url_without_invalidation(Some("https://example.com/doc".to_string()));
    document.set_navigation_urls(
      Some("https://example.com/doc".to_string()),
      Some("https://example.com/base/".to_string()),
    );

    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    let options = RenderOptions::new()
      .with_viewport(64, 64)
      .with_cancel_callback(Some(cancel));
    let err = match document.navigate_url(&file_url, options) {
      Ok(_) => panic!("expected navigation to be cancelled"),
      Err(err) => err,
    };
    assert!(
      matches!(err, Error::Render(RenderError::Timeout { .. })),
      "expected timeout/cancel error; got {err:?}"
    );

    // Cancellation must not perturb the currently committed URL hints. These are used for
    // resolving relative resource/link URLs and should remain stable for long-lived documents.
    assert_eq!(document.base_url(), Some("https://example.com/base/"));
    assert_eq!(document.document_url(), Some("https://example.com/doc"));
    assert_eq!(
      document.renderer.document_url.as_deref(),
      Some("https://example.com/doc")
    );
    assert_eq!(
      document.renderer.base_url.as_deref(),
      Some("https://example.com/base/")
    );
    Ok(())
  }

  #[test]
  fn navigate_html_with_options_cancels_dom_parse_and_restores_navigation_urls() -> Result<()> {
    let mut document = BrowserDocument::new(
      renderer_for_tests(),
      "<!doctype html><html><head><title>Old</title></head><body>old</body></html>",
      RenderOptions::new().with_viewport(64, 64),
    )?;
    document.set_document_url_without_invalidation(Some("https://example.com/doc".to_string()));
    document.set_navigation_urls(
      Some("https://example.com/doc".to_string()),
      Some("https://example.com/base/".to_string()),
    );

    // Cancel on the *second* `DomParse` deadline check. Without a scoped deadline around
    // `navigate_html_with_options`'s HTML parse, the cancel callback is only invoked during the
    // subsequent prepare/layout pipeline (which checks `DomParse` once in its preamble).
    let dom_parse_checks = Arc::new(AtomicUsize::new(0));
    let dom_parse_checks_for_cb = Arc::clone(&dom_parse_checks);
    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(move || {
      if crate::render_control::active_stage() != Some(RenderStage::DomParse) {
        return false;
      }
      dom_parse_checks_for_cb.fetch_add(1, Ordering::Relaxed) >= 1
    });

    // Ensure the HTML is large enough to require multiple reads; `DeadlineCheckedRead` caps reads
    // to 16KiB, so a >16KiB document guarantees multiple deadline checks during parsing.
    let large_comment = "x".repeat(32 * 1024);
    let html =
      format!("<!doctype html><html><head><!--{large_comment}--></head><body>new</body></html>");

    let options = RenderOptions::new()
      .with_viewport(64, 64)
      .with_cancel_callback(Some(cancel));
    let err = match document.navigate_html_with_options(
      "https://example.com/new",
      &html,
      Some("https://example.com/new_base/"),
      options,
    ) {
      Ok(_) => panic!("expected navigation to be cancelled"),
      Err(err) => err,
    };
    assert!(
      matches!(
        err,
        Error::Render(RenderError::Timeout {
          stage: RenderStage::DomParse,
          ..
        })
      ),
      "expected dom_parse timeout/cancel error; got {err:?}"
    );
    assert!(
      dom_parse_checks.load(Ordering::Relaxed) >= 2,
      "expected cancel callback to be invoked multiple times during dom parse"
    );

    // Cancellation must not perturb the currently committed URL hints.
    assert_eq!(document.base_url(), Some("https://example.com/base/"));
    assert_eq!(document.document_url(), Some("https://example.com/doc"));
    assert_eq!(
      document.renderer.document_url.as_deref(),
      Some("https://example.com/doc")
    );
    assert_eq!(
      document.renderer.base_url.as_deref(),
      Some("https://example.com/base/")
    );
    Ok(())
  }

  fn renderer_for_tests() -> super::super::FastRender {
    super::super::FastRender::builder()
      .font_sources(FontConfig::bundled_only())
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

  fn assert_rgb_close(color: PremultipliedColorU8, expected: u8, tolerance: u8) {
    assert_channel_close(color.red(), expected, tolerance);
    assert_channel_close(color.green(), expected, tolerance);
    assert_channel_close(color.blue(), expected, tolerance);
    assert_eq!(color.alpha(), 255);
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
    let mut document =
      BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(20, 20))?;

    let clock = Arc::new(crate::clock::VirtualClock::new());
    document.set_animation_clock(clock.clone());
    document.set_realtime_animations_enabled(true);

    let pixmap0 = document.render_frame()?;
    let c0 = pixmap0.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c0, 255, 0);

    clock.advance(Duration::from_millis(500));
    let pixmap1 = document.render_frame()?;
    let c1 = pixmap1.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c1, 128, 8);

    // Explicit per-render timestamps always override the real-time document timeline.
    document.set_animation_time_ms(1000.0);
    let pixmap_override = document.render_frame()?;
    let c2 = pixmap_override.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c2, 0, 0);

    Ok(())
  }

  #[test]
  fn render_if_needed_rerenders_for_realtime_animation_progress() -> Result<()> {
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
    let mut document =
      BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(20, 20))?;

    let clock = Arc::new(crate::js::clock::VirtualClock::new());
    document.set_animation_clock(clock.clone());
    document.set_realtime_animations_enabled(true);

    let pixmap0 = document.render_frame()?;
    assert!(
      document.render_if_needed()?.is_none(),
      "expected render_if_needed to return None before time advances"
    );

    clock.advance(Duration::from_millis(500));
    let pixmap1 = document
      .render_if_needed()?
      .expect("expected render_if_needed to repaint after clock advance");
    assert_ne!(pixmap1.data(), pixmap0.data());

    Ok(())
  }

  fn set_style_for_id(node: &mut DomNode, target_id: &str, style: &str) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
      if let DomNodeType::Element { attributes, .. } = &mut node.node_type {
        let has_id = attributes
          .iter()
          .any(|(name, value)| name == "id" && value == target_id);
        if has_id {
          if let Some((_, value)) = attributes.iter_mut().find(|(name, _)| name == "style") {
            if value == style {
              return false;
            }
            *value = style.to_string();
            return true;
          }
          attributes.push(("style".to_string(), style.to_string()));
          return true;
        }
      }
      for child in &mut node.children {
        stack.push(child);
      }
    }
    false
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
    let renderer = renderer_for_tests();
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
    let mut document =
      BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(2, 2))?;

    let clock = Arc::new(crate::clock::VirtualClock::new());
    document.set_animation_clock(clock.clone());
    document.set_realtime_animations_enabled(true);

    // t=0ms: opacity=0 => background shines through.
    let frame0 = document.render_frame()?;
    assert_eq!(pixel_gray(&frame0), 255);

    // t=600ms: opacity=0.6 => ~40% white.
    clock.advance(Duration::from_millis(600));
    let frame600 = document.render_frame()?;
    assert_pixel_gray_approx(&frame600, 102, 4);

    // Pause at t=600ms.
    let changed =
      document.mutate_dom(|dom| set_style_for_id(dom, "a", "animation-play-state: paused;"));
    assert!(changed, "expected DOM mutation to update style");
    let paused600 = document.render_frame()?;
    assert_pixel_gray_approx(&paused600, 102, 4);

    // Advance time while paused; output should remain frozen.
    clock.advance(Duration::from_millis(300));
    let paused900 = document.render_frame()?;
    assert_pixel_gray_approx(&paused900, 102, 4);

    // Resume at t=900ms (without advancing time).
    let changed =
      document.mutate_dom(|dom| set_style_for_id(dom, "a", "animation-play-state: running;"));
    assert!(changed, "expected DOM mutation to update style");
    let resumed900 = document.render_frame()?;
    assert_pixel_gray_approx(&resumed900, 102, 4);

    // t=1000ms: animation should have progressed to 700ms of active time (0.7 opacity).
    clock.advance(Duration::from_millis(100));
    let frame1000 = document.render_frame()?;
    assert_pixel_gray_approx(&frame1000, 77, 5);

    Ok(())
  }
}
