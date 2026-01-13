use crate::geometry::{Point, Rect, Size};
use crate::dom::DomNode;
use crate::html::title::find_document_title;
use crate::interaction::focus_scroll;
use crate::interaction::scroll_wheel::{apply_wheel_scroll_at_point_prepared, ScrollWheelInput};
use crate::interaction::{
  DateTimeInputKind, FormSubmission, FormSubmissionMethod, InteractionAction, InteractionEngine,
  InteractionState,
};
use crate::paint::rasterize::fill_rect;
use crate::scroll::ScrollState;
use crate::style::color::Rgba;
use crate::ui::about_pages;
use crate::ui::clipboard;
use crate::ui::find_in_page::{FindIndex, FindMatch, FindOptions};
use crate::ui::messages::{
  DatalistOption, NavigationReason, PointerButton, RenderedFrame, ScrollMetrics, TabId, UiToWorker,
  WorkerToUi,
};
use crate::{BrowserDocument, FastRender, RenderOptions, Result};
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

/// Per-tab worker-side controller that owns interactive document state (DOM + scroll + input).
///
/// This is a synchronous, message-driven component intended to be used by a render worker thread.
/// The UI thread sends [`UiToWorker`] messages and the controller returns the corresponding
/// [`WorkerToUi`] outputs.
pub struct BrowserTabController {
  tab_id: TabId,
  document: BrowserDocument,
  interaction: InteractionEngine,
  current_url: String,
  base_url: String,
  scroll_state: ScrollState,
  last_reported_scroll_state: ScrollState,
  last_pointer_pos_css: Option<(f32, f32)>,
  viewport_css: (u32, u32),
  dpr: f32,
  tick_animation_time_ms: f32,
  datalist_open_input: Option<usize>,
  find_query: String,
  find_case_sensitive: bool,
  find_matches: Vec<FindMatch>,
  find_active_match_index: Option<usize>,
}

impl BrowserTabController {
  fn apply_autofocus_if_present(&mut self) {
    let Some(target_id) =
      crate::interaction::autofocus::autofocus_target_node_id(self.document.dom())
    else {
      return;
    };

    // `InteractionEngine::focus_node_id` does not mutate the DOM; avoid invalidating cached layout
    // so that navigation-prepared documents can still paint without rerunning layout.
    self.document.mutate_dom(|dom| {
      let _ = self.interaction.focus_node_id(dom, Some(target_id), true);
      false
    });

    // Best-effort: scroll to reveal the focused node (including within scroll containers). This
    // requires layout artifacts, so it only applies when we already have a prepared document (e.g.
    // after a URL/about navigation that installed a prepared cache).
    if let Some(prepared) = self.document.prepared() {
      if let Some(next_scroll) = crate::interaction::focus_scroll::scroll_state_for_focus(
        prepared.box_tree(),
        prepared.fragment_tree(),
        &self.scroll_state,
        target_id,
      ) {
        if next_scroll != self.scroll_state {
          self.scroll_state = next_scroll;
          self.document.set_scroll_state(self.scroll_state.clone());
        }
      }
    }
  }

  /// Create a new controller backed by an HTML string.
  ///
  /// This is primarily intended for tests and `about:` pages.
  pub fn from_html(
    tab_id: TabId,
    html: &str,
    document_url: &str,
    viewport_css: (u32, u32),
    dpr: f32,
  ) -> Result<Self> {
    let renderer = FastRender::new()?;
    Self::from_html_with_renderer(renderer, tab_id, html, document_url, viewport_css, dpr)
  }

  /// Like [`BrowserTabController::from_html`], but allows the caller to provide a pre-built
  /// renderer instance.
  ///
  /// This is useful for tests that need a deterministic font configuration.
  pub fn from_html_with_renderer(
    renderer: FastRender,
    tab_id: TabId,
    html: &str,
    document_url: &str,
    viewport_css: (u32, u32),
    dpr: f32,
  ) -> Result<Self> {
    let options = RenderOptions::new()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);

    let mut document = BrowserDocument::new(renderer, html, options)?;
    document.set_navigation_urls(
      Some(document_url.to_string()),
      Some(document_url.to_string()),
    );

    let mut controller = Self {
      tab_id,
      document,
      interaction: InteractionEngine::new(),
      current_url: document_url.to_string(),
      base_url: strip_fragment(document_url),
      scroll_state: ScrollState::default(),
      last_reported_scroll_state: ScrollState::default(),
      last_pointer_pos_css: None,
      viewport_css,
      dpr,
      tick_animation_time_ms: 0.0,
      datalist_open_input: None,
      find_query: String::new(),
      find_case_sensitive: false,
      find_matches: Vec::new(),
      find_active_match_index: None,
    };

    // Match UI-worker behaviour: apply HTML autofocus once up-front so the first render can show
    // focus styling/caret/selection for trusted documents (e.g. chrome UI documents).
    controller.apply_autofocus_if_present();

    Ok(controller)
  }

  pub fn tab_id(&self) -> TabId {
    self.tab_id
  }

  pub fn document(&self) -> &BrowserDocument {
    &self.document
  }

  /// Mutate the underlying DOM tree, invalidating layout/paint state only if `f` reports changes.
  ///
  /// This is primarily intended for headless integrations (such as renderer-chrome experiments)
  /// that need to synchronize external UI state into an interactive `BrowserTabController`.
  pub fn mutate_dom<F>(&mut self, f: F) -> bool
  where
    F: FnOnce(&mut DomNode) -> bool,
  {
    self.document.mutate_dom(f)
  }

  /// Programmatically update the focused DOM node id.
  ///
  /// This is a thin wrapper around [`InteractionEngine::focus_node_id`]. It borrows the DOM mutably
  /// without marking it dirty (focus is tracked out-of-DOM via `InteractionState`).
  pub fn focus_node_id(
    &mut self,
    node_id: Option<usize>,
    focus_visible: bool,
  ) -> (bool, InteractionAction) {
    let mut changed = false;
    let mut action = InteractionAction::None;
    self.document.mutate_dom(|dom| {
      let (did_change, next_action) = self.interaction.focus_node_id(dom, node_id, focus_visible);
      changed = did_change;
      action = next_action;
      false
    });
    (changed, action)
  }

  pub fn interaction_state(&self) -> &InteractionState {
    self.interaction.interaction_state()
  }

  pub fn scroll_state(&self) -> &ScrollState {
    &self.scroll_state
  }

  pub fn current_url(&self) -> &str {
    &self.current_url
  }

  pub fn base_url(&self) -> &str {
    &self.base_url
  }

  /// Handle one UI → worker message and return any outputs.
  pub fn handle_message(&mut self, msg: UiToWorker) -> Result<Vec<WorkerToUi>> {
    match msg {
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } if tab_id == self.tab_id => self.handle_viewport_changed(viewport_css, dpr),
      UiToWorker::ScrollTo { tab_id, pos_css } if tab_id == self.tab_id => {
        self.handle_scroll_to(pos_css)
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } if tab_id == self.tab_id => self.handle_scroll(delta_css, pointer_css),
      UiToWorker::DropFiles {
        tab_id,
        pos_css,
        paths,
      } if tab_id == self.tab_id => self.handle_drop_files(pos_css, paths),
      UiToWorker::PointerMove {
        tab_id, pos_css, ..
      } if tab_id == self.tab_id => self.handle_pointer_move(pos_css),
      UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button,
        modifiers,
        click_count,
      } if tab_id == self.tab_id => {
        self.handle_pointer_down(pos_css, button, modifiers, click_count)
      }
      UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button,
        modifiers,
      } if tab_id == self.tab_id => self.handle_pointer_up(pos_css, button, modifiers),
      UiToWorker::SelectDropdownChoose {
        tab_id,
        select_node_id,
        option_node_id,
      } if tab_id == self.tab_id => {
        self.handle_select_dropdown_choose(select_node_id, option_node_id)
      }
      UiToWorker::DatalistChoose {
        tab_id,
        input_node_id,
        option_node_id,
      } if tab_id == self.tab_id => self.handle_datalist_choose(input_node_id, option_node_id),
      UiToWorker::DatalistCancel { tab_id } if tab_id == self.tab_id => {
        self.datalist_open_input = None;
        Ok(vec![WorkerToUi::DatalistClosed { tab_id }])
      }
      UiToWorker::DateTimePickerChoose {
        tab_id,
        input_node_id,
        value,
      } if tab_id == self.tab_id => self.handle_date_time_picker_choose(input_node_id, &value),
      UiToWorker::DateTimePickerCancel { tab_id } if tab_id == self.tab_id => Ok(vec![
        WorkerToUi::DateTimePickerClosed { tab_id },
      ]),
      UiToWorker::FilePickerChoose {
        tab_id,
        input_node_id,
        paths,
      } if tab_id == self.tab_id => self.handle_file_picker_choose(input_node_id, paths),
      UiToWorker::ColorPickerChoose {
        tab_id,
        input_node_id,
        value,
      } if tab_id == self.tab_id => self.handle_color_picker_choose(input_node_id, value),
      UiToWorker::ColorPickerCancel { tab_id } if tab_id == self.tab_id => Ok(vec![
        WorkerToUi::ColorPickerClosed { tab_id },
      ]),
      UiToWorker::FilePickerCancel { tab_id } if tab_id == self.tab_id => Ok(vec![
        WorkerToUi::FilePickerClosed { tab_id },
      ]),
      UiToWorker::TextInput { tab_id, text } if tab_id == self.tab_id => {
        self.handle_text_input(&text)
      }
      UiToWorker::A11ySetTextValue {
        tab_id,
        node_id,
        value,
      } if tab_id == self.tab_id => self.handle_a11y_set_text_value(node_id, &value),
      UiToWorker::A11ySetTextSelectionRange {
        tab_id,
        node_id,
        anchor,
        focus,
      } if tab_id == self.tab_id => self.handle_a11y_set_text_selection(node_id, anchor, focus),
      UiToWorker::Copy { tab_id } if tab_id == self.tab_id => self.handle_copy(),
      UiToWorker::Cut { tab_id } if tab_id == self.tab_id => self.handle_cut(),
      UiToWorker::Paste { tab_id, text } if tab_id == self.tab_id => self.handle_paste(&text),
      UiToWorker::SelectAll { tab_id } if tab_id == self.tab_id => self.handle_select_all(),
      UiToWorker::KeyAction { tab_id, key } if tab_id == self.tab_id => self.handle_key_action(key),
      UiToWorker::FindQuery {
        tab_id,
        query,
        case_sensitive,
      } if tab_id == self.tab_id => self.handle_find_query(&query, case_sensitive),
      UiToWorker::FindNext { tab_id } if tab_id == self.tab_id => self.handle_find_next(),
      UiToWorker::FindPrev { tab_id } if tab_id == self.tab_id => self.handle_find_prev(),
      UiToWorker::FindStop { tab_id } if tab_id == self.tab_id => self.handle_find_stop(),
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } if tab_id == self.tab_id => self.navigate(&url, reason),
      UiToWorker::NavigateRequest {
        tab_id,
        request,
        reason,
      } if tab_id == self.tab_id => self.handle_navigation_request_action(request, reason),
      UiToWorker::RequestRepaint { tab_id, .. } if tab_id == self.tab_id => self.force_repaint(),
      UiToWorker::Tick { tab_id, delta } if tab_id == self.tab_id => self.handle_tick(delta),
      _ => Ok(Vec::new()),
    }
  }

  /// Handle an accessibility (AccessKit) action targeted at a DOM node.
  ///
  /// This is used by UI layers that expose the rendered page via AccessKit to assistive
  /// technologies.
  ///
  /// Behaviour notes:
  /// - `accesskit::Action::Focus` updates the interaction engine focus state (with
  ///   `focus_visible=true`) and, when cached layout artifacts are available, scrolls the document
  ///   so the focused node is visible (matching browser behaviour).
  /// - `accesskit::Action::ScrollIntoView` scrolls the document to reveal the target node without
  ///   changing focus. (This mirrors DOM `scrollIntoView()` semantics; AT can request focus
  ///   separately if desired.)
  ///
  /// When no layout artifacts are available yet (e.g. before the first paint), focus updates are
  /// still applied best-effort but scrolling is skipped.
  #[cfg(feature = "browser_ui")]
  pub fn handle_accesskit_action(
    &mut self,
    node_id: usize,
    action: accesskit::Action,
  ) -> Result<Vec<WorkerToUi>> {
    match action {
      accesskit::Action::Focus => self.handle_accesskit_focus(node_id),
      accesskit::Action::ScrollIntoView => self.handle_accesskit_scroll_into_view(node_id),
      _ => Ok(Vec::new()),
    }
  }

  #[cfg(feature = "browser_ui")]
  fn handle_accesskit_focus(&mut self, node_id: usize) -> Result<Vec<WorkerToUi>> {
    let scroll_snapshot = self.scroll_state.clone();
    let interaction = &mut self.interaction;
    let document = &mut self.document;

    let mut focus_changed = false;
    let mut focus_scroll: Option<ScrollState> = None;

    if document.prepared().is_some() {
      let (changed, scroll) = document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let (changed, _action) = interaction.focus_node_id(dom, Some(node_id), true);
        let scroll = crate::interaction::focus_scroll::scroll_state_for_focus(
          box_tree,
          fragment_tree,
          &scroll_snapshot,
          node_id,
        );
        // `focus_node_id` does not mutate the DOM; avoid invalidating cached layout.
        (false, (changed, scroll))
      })?;
      focus_changed = changed;
      focus_scroll = scroll;
    } else {
      // No layout yet; still update focus best-effort but skip scrolling.
      document.mutate_dom(|dom| {
        let (changed, _action) = interaction.focus_node_id(dom, Some(node_id), true);
        focus_changed = changed;
        false
      });
    }

    let mut scroll_changed = false;
    if let Some(next_scroll) = focus_scroll {
      self.scroll_state = next_scroll;
      self.document.set_scroll_state(self.scroll_state.clone());
      scroll_changed = true;
    }

    if focus_changed || scroll_changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  #[cfg(feature = "browser_ui")]
  fn handle_accesskit_scroll_into_view(&mut self, node_id: usize) -> Result<Vec<WorkerToUi>> {
    let scroll_snapshot = self.scroll_state.clone();
    let focus_scroll = self.document.prepared().and_then(|prepared| {
      crate::interaction::focus_scroll::scroll_state_for_focus(
        prepared.box_tree(),
        prepared.fragment_tree(),
        &scroll_snapshot,
        node_id,
      )
    });

    if let Some(next_scroll) = focus_scroll {
      self.scroll_state = next_scroll;
      self.document.set_scroll_state(self.scroll_state.clone());
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_tick(&mut self, delta: Duration) -> Result<Vec<WorkerToUi>> {
    let wants_ticks = crate::ui::document_ticks::browser_document_wants_ticks(&self.document);
    if !wants_ticks {
      return Ok(Vec::new());
    }

    let delta_ms = duration_to_ms_f32(delta);
    if delta_ms == 0.0 {
      return Ok(Vec::new());
    }

    let prev = self.tick_animation_time_ms;
    let next = prev + delta_ms;
    let next = if next.is_finite() { next } else { f32::MAX };
    if next == prev {
      return Ok(Vec::new());
    }

    self.tick_animation_time_ms = next;
    self.document.set_animation_time_ms(next);

    self.paint_if_needed()
  }

  fn handle_find_query(&mut self, query: &str, case_sensitive: bool) -> Result<Vec<WorkerToUi>> {
    let query = query.to_string();
    let query_changed = self.find_query != query || self.find_case_sensitive != case_sensitive;
    self.find_query = query.clone();
    self.find_case_sensitive = case_sensitive;
    if query_changed {
      self.find_active_match_index = None;
    }

    if query.is_empty() {
      self.find_matches.clear();
      self.find_active_match_index = None;

      let mut out = vec![WorkerToUi::FindResult {
        tab_id: self.tab_id,
        query,
        case_sensitive,
        match_count: 0,
        active_match_index: None,
      }];

      // Force a repaint so any existing highlight overlays are cleared.
      out.extend(self.force_repaint()?);
      return Ok(out);
    }

    self.rebuild_find_matches();
    if self.find_active_match_index.is_none() && !self.find_matches.is_empty() {
      self.find_active_match_index = Some(0);
    }

    self.scroll_to_active_find_match();

    let mut out = vec![self.find_result_msg()];
    out.extend(self.force_repaint()?);
    Ok(out)
  }

  fn handle_find_next(&mut self) -> Result<Vec<WorkerToUi>> {
    if self.find_query.is_empty() {
      return Ok(Vec::new());
    }

    if self.find_matches.is_empty() {
      self.rebuild_find_matches();
    }

    if self.find_matches.is_empty() {
      let mut out = vec![self.find_result_msg()];
      out.extend(self.force_repaint()?);
      return Ok(out);
    }

    let count = self.find_matches.len();
    let next = self.find_active_match_index.unwrap_or(0).saturating_add(1) % count;
    self.find_active_match_index = Some(next);
    self.scroll_to_active_find_match();

    let mut out = vec![self.find_result_msg()];
    out.extend(self.force_repaint()?);
    Ok(out)
  }

  fn handle_find_prev(&mut self) -> Result<Vec<WorkerToUi>> {
    if self.find_query.is_empty() {
      return Ok(Vec::new());
    }

    if self.find_matches.is_empty() {
      self.rebuild_find_matches();
    }

    if self.find_matches.is_empty() {
      let mut out = vec![self.find_result_msg()];
      out.extend(self.force_repaint()?);
      return Ok(out);
    }

    let count = self.find_matches.len();
    let current = self.find_active_match_index.unwrap_or(0) % count;
    let prev = if current == 0 { count - 1 } else { current - 1 };
    self.find_active_match_index = Some(prev);
    self.scroll_to_active_find_match();

    let mut out = vec![self.find_result_msg()];
    out.extend(self.force_repaint()?);
    Ok(out)
  }

  fn handle_find_stop(&mut self) -> Result<Vec<WorkerToUi>> {
    self.find_query.clear();
    self.find_case_sensitive = false;
    self.find_matches.clear();
    self.find_active_match_index = None;

    let mut out = vec![WorkerToUi::FindResult {
      tab_id: self.tab_id,
      query: String::new(),
      case_sensitive: false,
      match_count: 0,
      active_match_index: None,
    }];
    out.extend(self.force_repaint()?);
    Ok(out)
  }

  fn find_result_msg(&self) -> WorkerToUi {
    WorkerToUi::FindResult {
      tab_id: self.tab_id,
      query: self.find_query.clone(),
      case_sensitive: self.find_case_sensitive,
      match_count: self.find_matches.len(),
      active_match_index: self.find_active_match_index,
    }
  }

  fn rebuild_find_matches(&mut self) {
    let Some(prepared) = self.document.prepared() else {
      // Layout not ready yet; keep matches empty until after the first paint.
      self.find_matches.clear();
      self.find_active_match_index = None;
      return;
    };

    // Mirror paint-time geometry (sticky + element scroll offsets + viewport-fixed scroll cancel)
    // so highlight rects stay aligned with what the user sees after scrolling.
    let tree = prepared.fragment_tree_for_geometry(&self.scroll_state);
    let index = FindIndex::build(&tree);
    self.find_matches = index.find(
      &self.find_query,
      FindOptions {
        case_sensitive: self.find_case_sensitive,
      },
    );

    if self.find_matches.is_empty() {
      self.find_active_match_index = None;
    } else {
      let max = self.find_matches.len() - 1;
      let current = self.find_active_match_index.unwrap_or(0).min(max);
      self.find_active_match_index = Some(current);
    }
  }

  fn scroll_to_active_find_match(&mut self) {
    let Some(active) = self.find_active_match_index else {
      return;
    };
    let Some(m) = self.find_matches.get(active) else {
      return;
    };
    let bounds = m.bounds;
    if bounds == Rect::ZERO {
      return;
    }

    let viewport_w = self.viewport_css.0 as f32;
    let viewport_h = self.viewport_css.1 as f32;

    let mut target = self.scroll_state.viewport;

    // Try to keep the full match bounds visible, clamping later.
    if bounds.min_y() < target.y {
      target.y = bounds.min_y();
    } else if bounds.max_y() > target.y + viewport_h {
      target.y = bounds.max_y() - viewport_h;
    }

    if bounds.min_x() < target.x {
      target.x = bounds.min_x();
    } else if bounds.max_x() > target.x + viewport_w {
      target.x = bounds.max_x() - viewport_w;
    }

    if !target.x.is_finite() {
      target.x = 0.0;
    }
    if !target.y.is_finite() {
      target.y = 0.0;
    }
    target.x = target.x.max(0.0);
    target.y = target.y.max(0.0);

    // Clamp to root scroll bounds when possible.
    if let Some(prepared) = self.document.prepared() {
      let viewport_size = Size::new(viewport_w, viewport_h);
      if let Some(root) =
        crate::scroll::build_scroll_chain(&prepared.fragment_tree().root, viewport_size, &[]).last()
      {
        target = root.bounds.clamp(target);
      }
    }

    if target != self.scroll_state.viewport {
      let mut next = self.scroll_state.clone();
      next.viewport = target;
      self.scroll_state = next;
      self.document.set_scroll_state(self.scroll_state.clone());
    }
  }

  fn handle_scroll_to(&mut self, pos_css: (f32, f32)) -> Result<Vec<WorkerToUi>> {
    let sanitize = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
    let target = Point::new(sanitize(pos_css.0), sanitize(pos_css.1));

    // Ensure we have a prepared tree for clamping.
    if self.document.prepared().is_none() {
      self.force_repaint()?;
    }

    if let Some(prepared) = self.document.prepared() {
      let viewport = prepared.fragment_tree().viewport_size();
      let bounds = crate::scroll::build_scroll_chain(&prepared.fragment_tree().root, viewport, &[])
        .first()
        .map(|state| state.bounds);
      let mut next = self.scroll_state.clone();
      next.viewport = bounds.map(|b| b.clamp(target)).unwrap_or(target);
      if next != self.scroll_state {
        self.scroll_state = next;
        self.document.set_scroll_state(self.scroll_state.clone());
      }
    } else {
      let mut next = self.scroll_state.clone();
      next.viewport = target;
      if next != self.scroll_state {
        self.scroll_state = next;
        self.document.set_scroll_state(self.scroll_state.clone());
      }
    }

    self.paint_if_needed()
  }

  fn handle_viewport_changed(
    &mut self,
    viewport_css: (u32, u32),
    dpr: f32,
  ) -> Result<Vec<WorkerToUi>> {
    self.viewport_css = viewport_css;
    self.dpr = dpr;
    self.document.set_viewport(viewport_css.0, viewport_css.1);
    self.document.set_device_pixel_ratio(dpr);
    // Keep the document's scroll state stable across the resize until painting clamps it.
    self.document.set_scroll_state(self.scroll_state.clone());
    self.paint_if_needed()
  }

  fn handle_scroll(
    &mut self,
    delta_css: (f32, f32),
    pointer_css: Option<(f32, f32)>,
  ) -> Result<Vec<WorkerToUi>> {
    // Ensure we have a prepared tree for hit-testing scroll containers.
    if self.document.prepared().is_none() {
      self.force_repaint()?;
    }

    let delta_x = if delta_css.0.is_finite() { delta_css.0 } else { 0.0 };
    let delta_y = if delta_css.1.is_finite() { delta_css.1 } else { 0.0 };

    let pointer_css =
      pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite() && *x >= 0.0 && *y >= 0.0);

    if let Some(pointer_css) = pointer_css {
      // Give a focused `<input type=number>` under the pointer a chance to consume the wheel
      // gesture for numeric stepping (instead of scrolling the page).
      let scroll_snapshot = self.scroll_state.clone();
      let engine = &mut self.interaction;
      let hit_tree =
        (scroll_snapshot.viewport != Point::ZERO || !scroll_snapshot.elements.is_empty())
          .then(|| {
            self
              .document
              .prepared()
              .map(|prepared| prepared.fragment_tree_for_geometry(&scroll_snapshot))
          })
          .flatten();
      if let Ok(step_result) =
        self
          .document
          .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
            let hit_tree = hit_tree.as_ref().unwrap_or(fragment_tree);
            let step_result = engine.wheel_step_number_input(
              dom,
              box_tree,
              hit_tree,
              &scroll_snapshot,
              Point::new(pointer_css.0, pointer_css.1),
              delta_y,
            );
            let changed = step_result.unwrap_or(false);
            (changed, step_result)
          })
      {
        if let Some(dom_changed) = step_result {
          // Numeric stepping does not update scroll state.
          if dom_changed {
            return self.paint_if_needed();
          }
          return Ok(Vec::new());
        }
      }
    }

    let Some(prepared) = self.document.prepared() else {
      return Ok(Vec::new());
    };

    let mut next_state = self.scroll_state.clone();

    if let Some(pointer_css) = pointer_css {
      let page_point =
        Point::new(pointer_css.0, pointer_css.1).translate(self.scroll_state.viewport);
      next_state = apply_wheel_scroll_at_point_prepared(
        prepared,
        &self.scroll_state,
        Size::new(self.viewport_css.0 as f32, self.viewport_css.1 as f32),
        page_point,
        ScrollWheelInput {
          delta_x,
          delta_y,
        },
      );
    } else {
      // No pointer location: treat this as a viewport scroll.
      let mut viewport_scroll = next_state.viewport;

      let delta = Point::new(delta_x, delta_y);
      if delta != Point::ZERO {
        let viewport = prepared.fragment_tree().viewport_size();
        let bounds = crate::scroll::build_scroll_chain(&prepared.fragment_tree().root, viewport, &[])
          .first()
          .map(|state| state.bounds);
        let target = Point::new(viewport_scroll.x + delta.x, viewport_scroll.y + delta.y);
        if let Some(bounds) = bounds {
          viewport_scroll = bounds.clamp(target);
        } else {
          viewport_scroll = Point::new(target.x.max(0.0), target.y.max(0.0));
        }
      }
      next_state.viewport = viewport_scroll;
    }

    if next_state != self.scroll_state {
      self.scroll_state = next_state;
      self.document.set_scroll_state(self.scroll_state.clone());
    }

    self.paint_if_needed()
  }

  fn handle_pointer_move(&mut self, pos_css: (f32, f32)) -> Result<Vec<WorkerToUi>> {
    self.last_pointer_pos_css = Some(pos_css);

    let pointer_in_page =
      pos_css.0.is_finite() && pos_css.1.is_finite() && pos_css.0 >= 0.0 && pos_css.1 >= 0.0;
    let active_document_selection_drag = self.interaction.active_document_selection_drag();

    // Mirror the UI worker: when extending a document selection, auto-scroll as the pointer nears
    // the viewport edges so selection can extend beyond the visible region.
    const EDGE_THRESHOLD: f32 = 32.0;
    const SCROLL_STEP: f32 = 20.0;
    let autoscroll_delta_y = if active_document_selection_drag && pointer_in_page {
      let h = self.viewport_css.1 as f32;
      if pos_css.1 <= EDGE_THRESHOLD {
        -SCROLL_STEP
      } else if pos_css.1 >= h - EDGE_THRESHOLD {
        SCROLL_STEP
      } else {
        0.0
      }
    } else {
      0.0
    };

    let (box_tree_ptr, fragment_tree_ptr, hit_tree) = {
      let Some(prepared) = self.document.prepared() else {
        return Ok(Vec::new());
      };
      let box_tree_ptr = prepared.box_tree() as *const crate::BoxTree;
      let fragment_tree_ptr =
        prepared.fragment_tree() as *const crate::tree::fragment_tree::FragmentTree;
      let hit_tree = (self.scroll_state.viewport != Point::ZERO
        || !self.scroll_state.elements.is_empty())
        .then(|| prepared.fragment_tree_for_geometry(&self.scroll_state));
      (box_tree_ptr, fragment_tree_ptr, hit_tree)
    };

    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let box_tree = unsafe { &*box_tree_ptr };
    let fragment_tree_unscrolled = unsafe { &*fragment_tree_ptr };
    let fragment_tree_before = hit_tree.as_ref().unwrap_or(fragment_tree_unscrolled);

    let mut changed = self.document.mutate_dom(|dom| {
      self.interaction.pointer_move(
        dom,
        box_tree,
        fragment_tree_before,
        &self.scroll_state,
        viewport_point,
      )
    });

    let mut scroll_changed = false;
    if autoscroll_delta_y != 0.0 {
      let prev_scroll = self.scroll_state.clone();
      let mut candidate = prev_scroll.clone();
      let next_y = candidate.viewport.y + autoscroll_delta_y;
      if next_y.is_finite() {
        candidate.viewport.y = next_y.max(0.0);
      }

      let viewport_size = Size::new(self.viewport_css.0 as f32, self.viewport_css.1 as f32);
      if let Some(root) =
        crate::scroll::build_scroll_chain(&fragment_tree_unscrolled.root, viewport_size, &[]).last()
      {
        candidate.viewport = root.bounds.clamp(candidate.viewport);
      }

      if candidate.viewport != prev_scroll.viewport {
        candidate.update_deltas_from(&prev_scroll);
        self.scroll_state = candidate;
        self.document.set_scroll_state(self.scroll_state.clone());
        scroll_changed = true;

        // Important: after scrolling, re-run pointer_move with the updated scroll state so document
        // selection focus advances in the same event.
        let hit_tree_after = self.document.prepared().and_then(|prepared| {
          (self.scroll_state.viewport != Point::ZERO || !self.scroll_state.elements.is_empty())
            .then(|| prepared.fragment_tree_for_geometry(&self.scroll_state))
        });
        let fragment_tree_after = hit_tree_after.as_ref().unwrap_or(fragment_tree_unscrolled);
        changed |= self.document.mutate_dom(|dom| {
          self.interaction.pointer_move(
            dom,
            box_tree,
            fragment_tree_after,
            &self.scroll_state,
            viewport_point,
          )
        });
      }
    }

    if changed || scroll_changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_drop_files(
    &mut self,
    pos_css: (f32, f32),
    paths: Vec<PathBuf>,
  ) -> Result<Vec<WorkerToUi>> {
    self.last_pointer_pos_css = Some(pos_css);
    let (box_tree_ptr, fragment_tree_ptr, hit_tree) = {
      let Some(prepared) = self.document.prepared() else {
        return Ok(Vec::new());
      };
      let box_tree_ptr = prepared.box_tree() as *const crate::BoxTree;
      let fragment_tree_ptr =
        prepared.fragment_tree() as *const crate::tree::fragment_tree::FragmentTree;
      let hit_tree = (self.scroll_state.viewport != Point::ZERO
        || !self.scroll_state.elements.is_empty())
        .then(|| prepared.fragment_tree_for_geometry(&self.scroll_state));
      (box_tree_ptr, fragment_tree_ptr, hit_tree)
    };

    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let fragment_tree = unsafe { &*fragment_tree_ptr };
    let fragment_tree = hit_tree.as_ref().unwrap_or(fragment_tree);

    let changed = self.document.mutate_dom(|dom| {
      self.interaction.drop_files_with_scroll(
        dom,
        unsafe { &*box_tree_ptr },
        fragment_tree,
        &self.scroll_state,
        viewport_point,
        &paths,
      )
    });

    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_pointer_down(
    &mut self,
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: crate::ui::PointerModifiers,
    click_count: u8,
  ) -> Result<Vec<WorkerToUi>> {
    self.last_pointer_pos_css = Some(pos_css);
    if button != PointerButton::Primary && button != PointerButton::Middle {
      return Ok(Vec::new());
    }

    let (box_tree_ptr, fragment_tree_ptr, hit_tree) = {
      let Some(prepared) = self.document.prepared() else {
        return Ok(Vec::new());
      };
      let box_tree_ptr = prepared.box_tree() as *const crate::BoxTree;
      let fragment_tree_ptr =
        prepared.fragment_tree() as *const crate::tree::fragment_tree::FragmentTree;
      let hit_tree = (self.scroll_state.viewport != Point::ZERO
        || !self.scroll_state.elements.is_empty())
        .then(|| prepared.fragment_tree_for_geometry(&self.scroll_state));
      (box_tree_ptr, fragment_tree_ptr, hit_tree)
    };

    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let fragment_tree = unsafe { &*fragment_tree_ptr };
    let fragment_tree = hit_tree.as_ref().unwrap_or(fragment_tree);

    let changed = self.document.mutate_dom(|dom| {
      self.interaction.pointer_down_with_click_count(
        dom,
        unsafe { &*box_tree_ptr },
        fragment_tree,
        &self.scroll_state,
        viewport_point,
        button,
        modifiers,
        click_count,
      )
    });
    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_pointer_up(
    &mut self,
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: crate::ui::PointerModifiers,
  ) -> Result<Vec<WorkerToUi>> {
    self.last_pointer_pos_css = Some(pos_css);
    if button != PointerButton::Primary && button != PointerButton::Middle {
      return Ok(Vec::new());
    }

    let scroll_snapshot = self.scroll_state.clone();

    let (box_tree_ptr, fragment_tree_ptr, hit_tree) = {
      let Some(prepared) = self.document.prepared() else {
        return Ok(Vec::new());
      };
      let box_tree_ptr = prepared.box_tree() as *const crate::BoxTree;
      let fragment_tree_ptr =
        prepared.fragment_tree() as *const crate::tree::fragment_tree::FragmentTree;
      let hit_tree = (scroll_snapshot.viewport != Point::ZERO
        || !scroll_snapshot.elements.is_empty())
        .then(|| prepared.fragment_tree_for_geometry(&scroll_snapshot));
      (box_tree_ptr, fragment_tree_ptr, hit_tree)
    };

    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let fragment_tree_layout = unsafe { &*fragment_tree_ptr };
    let fragment_tree_hit = hit_tree.as_ref().unwrap_or(fragment_tree_layout);

    let mut action = InteractionAction::None;
    let mut picker_value: Option<String> = None;
    let mut up_hit: Option<crate::interaction::HitTestResult> = None;
    let changed = self.document.mutate_dom(|dom| {
      let (dom_changed, next_action, hit) = self.interaction.pointer_up_with_scroll_and_hit(
        dom,
        unsafe { &*box_tree_ptr },
        fragment_tree_hit,
        &scroll_snapshot,
        viewport_point,
        button,
        modifiers,
        true,
        &self.current_url,
        &self.base_url,
      );
      if let InteractionAction::OpenColorPicker { input_node_id } = &next_action {
        picker_value = crate::dom::find_node_mut_by_preorder_id(dom, *input_node_id)
          .and_then(|node| crate::dom::input_color_value_string(node));
      }
      action = next_action;
      up_hit = hit;
      dom_changed
    });

    // Pointer-driven focus changes (e.g. clicking a <label> that focuses a visually-hidden input)
    // should not unexpectedly scroll away from the clicked content. Only apply focus scroll when
    // the newly-focused element is the actual hit-test target at the pointer location.
    let mut scroll_changed = false;
    if let InteractionAction::FocusChanged {
      node_id: Some(focused_id),
    } = &action
    {
      let apply_focus_scroll = up_hit
        .as_ref()
        .is_some_and(|hit| hit.styled_node_id == *focused_id || hit.dom_node_id == *focused_id);
      if apply_focus_scroll {
        if let Some(next_scroll) = focus_scroll::scroll_state_for_focus(
          unsafe { &*box_tree_ptr },
          fragment_tree_layout,
          &scroll_snapshot,
          *focused_id,
        ) {
          if next_scroll != self.scroll_state {
            self.scroll_state = next_scroll;
            self.document.set_scroll_state(self.scroll_state.clone());
            scroll_changed = true;
          }
        }
      }
    }

    match action {
      InteractionAction::Navigate { href } => {
        // Link click navigation.
        return self.handle_navigation_action(href, NavigationReason::LinkClick);
      }
      InteractionAction::OpenInNewTab { href } => {
        let mut out = vec![WorkerToUi::RequestOpenInNewTab {
          tab_id: self.tab_id,
          url: href,
        }];
        if changed {
          out.extend(self.paint_if_needed()?);
        }
        Ok(out)
      }
      InteractionAction::OpenInNewTabRequest { request } => {
        let mut out = vec![WorkerToUi::RequestOpenInNewTabRequest {
          tab_id: self.tab_id,
          request,
        }];
        if changed {
          out.extend(self.paint_if_needed()?);
        }
        Ok(out)
      }
      InteractionAction::NavigateRequest { request } => {
        return self.handle_navigation_request_action(request, NavigationReason::LinkClick);
      }
      InteractionAction::TextDrop { target_dom_id, text } => {
        // This controller does not currently dispatch JS drag/drop events. Preserve the legacy
        // (pre-deferred-drop) behavior by applying the default insertion immediately.
        let drop_changed = self
          .document
          .mutate_dom(|dom| self.interaction.apply_text_drop(dom, target_dom_id, &text));
        if changed || drop_changed {
          self.paint_if_needed()
        } else {
          Ok(Vec::new())
        }
      }
      InteractionAction::OpenSelectDropdown {
        select_node_id,
        control,
      } => {
        // Back-compat: older UIs listen for `OpenSelectDropdown`.
        let mut out = vec![WorkerToUi::OpenSelectDropdown {
          tab_id: self.tab_id,
          select_node_id,
          control: control.clone(),
        }];

        let anchor_css = self
          .select_anchor_css(select_node_id)
          .filter(|rect| {
            rect.origin.x.is_finite()
              && rect.origin.y.is_finite()
              && rect.size.width.is_finite()
              && rect.size.height.is_finite()
          })
          .unwrap_or_else(|| {
            Rect::from_xywh(
              if viewport_point.x.is_finite() {
                viewport_point.x
              } else {
                0.0
              },
              if viewport_point.y.is_finite() {
                viewport_point.y
              } else {
                0.0
              },
              0.0,
              0.0,
            )
          });
        out.push(WorkerToUi::SelectDropdownOpened {
          tab_id: self.tab_id,
          select_node_id,
          control,
          anchor_css,
        });

        if changed {
          out.extend(self.paint_if_needed()?);
        }

        Ok(out)
      }
      InteractionAction::OpenFilePicker {
        input_node_id,
        multiple,
        accept,
      } => {
        let mut out = vec![WorkerToUi::FilePickerOpened {
          tab_id: self.tab_id,
          input_node_id,
          multiple,
          accept,
          anchor_css: self
            .select_anchor_css(input_node_id)
            .filter(|rect| {
              rect.origin.x.is_finite()
                && rect.origin.y.is_finite()
                && rect.size.width.is_finite()
                && rect.size.height.is_finite()
            })
            .unwrap_or_else(|| Rect::from_xywh(viewport_point.x, viewport_point.y, 1.0, 1.0)),
        }];
        if changed {
          out.extend(self.paint_if_needed()?);
        }
        Ok(out)
      }
      InteractionAction::OpenDateTimePicker { input_node_id, kind } => {
        let mut out = vec![WorkerToUi::DateTimePickerOpened {
          tab_id: self.tab_id,
          input_node_id,
          kind,
          value: self.date_time_picker_value(input_node_id, kind),
          anchor_css: self
            .select_anchor_css(input_node_id)
            .filter(|rect| {
              rect.origin.x.is_finite()
                && rect.origin.y.is_finite()
                && rect.size.width.is_finite()
                && rect.size.height.is_finite()
            })
            .unwrap_or_else(|| {
              Rect::from_xywh(
                if viewport_point.x.is_finite() {
                  viewport_point.x
                } else {
                  0.0
                },
                if viewport_point.y.is_finite() {
                  viewport_point.y
                } else {
                  0.0
                },
                1.0,
                1.0,
              )
            }),
        }];
        if changed {
          out.extend(self.paint_if_needed()?);
        }
        Ok(out)
      }
      InteractionAction::OpenColorPicker { input_node_id } => {
        let mut out = vec![WorkerToUi::ColorPickerOpened {
          tab_id: self.tab_id,
          input_node_id,
          value: picker_value.unwrap_or_else(|| "#000000".to_string()),
          anchor_css: self
            .select_anchor_css(input_node_id)
            .filter(|rect| {
              rect.origin.x.is_finite()
                && rect.origin.y.is_finite()
                && rect.size.width.is_finite()
                && rect.size.height.is_finite()
            })
            .unwrap_or_else(|| {
              Rect::from_xywh(
                if viewport_point.x.is_finite() {
                  viewport_point.x
                } else {
                  0.0
                },
                if viewport_point.y.is_finite() {
                  viewport_point.y
                } else {
                  0.0
                },
                1.0,
                1.0,
              )
            }),
        }];
        if changed {
          out.extend(self.paint_if_needed()?);
        }
        Ok(out)
      }
      InteractionAction::OpenMediaControls { media_node_id, kind } => {
        let mut out = vec![WorkerToUi::MediaControlsOpened {
          tab_id: self.tab_id,
          node_id: media_node_id,
          kind,
          anchor_css: self
            .select_anchor_css(media_node_id)
            .filter(|rect| {
              rect.origin.x.is_finite()
                && rect.origin.y.is_finite()
                && rect.size.width.is_finite()
                && rect.size.height.is_finite()
            })
            .unwrap_or_else(|| Rect::from_xywh(viewport_point.x, viewport_point.y, 1.0, 1.0)),
        }];
        if changed {
          out.extend(self.paint_if_needed()?);
        }
        Ok(out)
      }
      _ => {
        if changed || scroll_changed {
          self.paint_if_needed()
        } else {
          Ok(Vec::new())
        }
      }
    }
  }

  fn select_anchor_css(&self, select_node_id: usize) -> Option<Rect> {
    let prepared = self.document.prepared()?;

    let select_box_id = {
      let mut stack: Vec<&crate::tree::box_tree::BoxNode> = vec![&prepared.box_tree().root];
      let mut found = None;
      while let Some(node) = stack.pop() {
        if node.styled_node_id == Some(select_node_id) {
          found = Some(node.id);
          break;
        }
        if let Some(body) = node.footnote_body.as_deref() {
          stack.push(body);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      found
    }?;

    let geom_tree = prepared.fragment_tree_for_geometry(&self.scroll_state);
    let page_rect = crate::interaction::absolute_bounds_for_box_id(&geom_tree, select_box_id)?;

    // Convert page-space bounds (includes scroll) to viewport-local coords for UI positioning.
    Some(page_rect.translate(Point::new(
      -self.scroll_state.viewport.x,
      -self.scroll_state.viewport.y,
    )))
  }

  fn handle_text_input(&mut self, text: &str) -> Result<Vec<WorkerToUi>> {
    let prev_open = self.datalist_open_input;
    let scroll_snapshot = self.scroll_state.clone();
    let mut datalist_open: Option<(usize, Vec<DatalistOption>)> = None;
    // Prefer using cached layout artifacts when available so `<select>` typeahead can use the
    // painted option list (skipping options hidden via computed `display:none`, etc).
    let engine = &mut self.interaction;
    let result = self
      .document
      .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let changed = engine.text_input_with_box_tree(dom, Some(box_tree), text);
        if changed {
          if let Some(focused) = engine.focused_node_id() {
            datalist_open = datalist_popup_options(dom, focused).map(|options| (focused, options));
          }
        }
        let caret_scroll =
          crate::interaction::textarea_caret_scroll::textarea_scroll_y_to_reveal_focused_caret(
            dom,
            engine.interaction_state(),
            box_tree,
            fragment_tree,
            &scroll_snapshot,
          );
        (changed, (changed, caret_scroll))
      });
    let (changed, caret_scroll) = match result {
      Ok(result) => result,
      Err(_) => {
        let changed = self.document.mutate_dom(|dom| {
          let changed = engine.text_input(dom, text);
          if changed {
            if let Some(focused) = engine.focused_node_id() {
              datalist_open = datalist_popup_options(dom, focused).map(|options| (focused, options));
            }
          }
          changed
        });
        (changed, None)
      }
    };

    let mut scroll_changed = false;
    if let Some((textarea_box_id, next_y)) = caret_scroll {
      let mut next_state = self.scroll_state.clone();
      let existing = next_state.element_offset(textarea_box_id);
      let next_offset = Point::new(existing.x, next_y);
      if next_offset == Point::ZERO {
        next_state.elements.remove(&textarea_box_id);
      } else {
        next_state.elements.insert(textarea_box_id, next_offset);
      }
      if next_state != self.scroll_state {
        self.scroll_state = next_state;
        self.document.set_scroll_state(self.scroll_state.clone());
        scroll_changed = true;
      }
    }

    let mut out = Vec::new();
    if let Some((input_node_id, options)) = datalist_open {
      let anchor_css = self.select_anchor_css(input_node_id).unwrap_or(Rect::ZERO);
      out.push(WorkerToUi::DatalistOpened {
        tab_id: self.tab_id,
        input_node_id,
        options,
        anchor_css,
      });
      self.datalist_open_input = Some(input_node_id);
    } else if prev_open.is_some() {
      out.push(WorkerToUi::DatalistClosed { tab_id: self.tab_id });
      self.datalist_open_input = None;
    }
    if changed || scroll_changed {
      out.extend(self.paint_if_needed()?);
    }
    Ok(out)
  }

  fn handle_datalist_choose(
    &mut self,
    input_node_id: usize,
    option_node_id: usize,
  ) -> Result<Vec<WorkerToUi>> {
    // Mirror the threaded worker semantics: choosing any option in the datalist overlay should close
    // the popup even when the selection is rejected (disabled option) or a no-op.
    let mut out = vec![WorkerToUi::DatalistClosed { tab_id: self.tab_id }];
    self.datalist_open_input = None;
    let engine = &mut self.interaction;
    let changed = self
      .document
      .mutate_dom(|dom| engine.activate_datalist_option(dom, input_node_id, option_node_id));
    if changed {
      out.extend(self.paint_if_needed()?);
    }
    Ok(out)
  }

  fn handle_a11y_set_text_value(&mut self, node_id: usize, value: &str) -> Result<Vec<WorkerToUi>> {
    let changed = self
      .document
      .mutate_dom(|dom| self.interaction.set_text_control_value(dom, node_id, value));
    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_a11y_set_text_selection(
    &mut self,
    node_id: usize,
    anchor: usize,
    focus: usize,
  ) -> Result<Vec<WorkerToUi>> {
    let changed = self.document.mutate_dom(|dom| {
      self
        .interaction
        .a11y_set_text_selection_range(dom, node_id, anchor, focus)
    });
    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn close_datalist_if_focus_changed(&mut self, out: &mut Vec<WorkerToUi>) {
    let Some(open_input) = self.datalist_open_input else {
      return;
    };
    if self.interaction_state().focused != Some(open_input) {
      out.push(WorkerToUi::DatalistClosed {
        tab_id: self.tab_id,
      });
      self.datalist_open_input = None;
    }
  }

  fn handle_select_all(&mut self) -> Result<Vec<WorkerToUi>> {
    let scroll_snapshot = self.scroll_state.clone();
    let result = self
      .document
      .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let dom_changed = self.interaction.clipboard_select_all(dom);
        let caret_scroll =
          crate::interaction::textarea_caret_scroll::textarea_scroll_y_to_reveal_focused_caret(
            dom,
            self.interaction.interaction_state(),
            box_tree,
            fragment_tree,
            &scroll_snapshot,
          );
        (dom_changed, (dom_changed, caret_scroll))
      });
    let (changed, caret_scroll) = match result {
      Ok(result) => result,
      Err(_) => {
        let changed = self
          .document
          .mutate_dom(|dom| self.interaction.clipboard_select_all(dom));
        (changed, None)
      }
    };

    let mut scroll_changed = false;
    if let Some((textarea_box_id, next_y)) = caret_scroll {
      let mut next_state = self.scroll_state.clone();
      let existing = next_state.element_offset(textarea_box_id);
      let next_offset = Point::new(existing.x, next_y);
      if next_offset == Point::ZERO {
        next_state.elements.remove(&textarea_box_id);
      } else {
        next_state.elements.insert(textarea_box_id, next_offset);
      }
      if next_state != self.scroll_state {
        self.scroll_state = next_state;
        self.document.set_scroll_state(self.scroll_state.clone());
        scroll_changed = true;
      }
    }

    if changed || scroll_changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_copy(&mut self) -> Result<Vec<WorkerToUi>> {
    let mut copied: Option<String> = None;
    if self
      .document
      .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        copied = self
          .interaction
          .clipboard_copy_with_layout(dom, box_tree, fragment_tree);
        (false, ())
      })
      .is_err()
    {
      let _ = self.document.mutate_dom(|dom| {
        copied = self.interaction.clipboard_copy(dom);
        false
      });
    }
    let Some(mut text) = copied else {
      return Ok(Vec::new());
    };
    clipboard::clamp_clipboard_text_in_place(&mut text);
    Ok(vec![WorkerToUi::SetClipboardText {
      tab_id: self.tab_id,
      text,
    }])
  }

  fn handle_cut(&mut self) -> Result<Vec<WorkerToUi>> {
    let mut cut_text: Option<String> = None;
    let scroll_snapshot = self.scroll_state.clone();
    let result = self
      .document
      .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let (dom_changed, text) = self.interaction.clipboard_cut(dom);
        cut_text = text;
        let caret_scroll =
          crate::interaction::textarea_caret_scroll::textarea_scroll_y_to_reveal_focused_caret(
            dom,
            self.interaction.interaction_state(),
            box_tree,
            fragment_tree,
            &scroll_snapshot,
          );
        (dom_changed, (dom_changed, caret_scroll))
      });
    let (changed, caret_scroll) = match result {
      Ok(result) => result,
      Err(_) => {
        let changed = self.document.mutate_dom(|dom| {
          let (dom_changed, text) = self.interaction.clipboard_cut(dom);
          cut_text = text;
          dom_changed
        });
        (changed, None)
      }
    };

    let mut out = Vec::new();
    if let Some(mut text) = cut_text {
      clipboard::clamp_clipboard_text_in_place(&mut text);
      out.push(WorkerToUi::SetClipboardText {
        tab_id: self.tab_id,
        text,
      });
    }

    let mut scroll_changed = false;
    if let Some((textarea_box_id, next_y)) = caret_scroll {
      let mut next_state = self.scroll_state.clone();
      let existing = next_state.element_offset(textarea_box_id);
      let next_offset = Point::new(existing.x, next_y);
      if next_offset == Point::ZERO {
        next_state.elements.remove(&textarea_box_id);
      } else {
        next_state.elements.insert(textarea_box_id, next_offset);
      }
      if next_state != self.scroll_state {
        self.scroll_state = next_state;
        self.document.set_scroll_state(self.scroll_state.clone());
        scroll_changed = true;
      }
    }

    if changed || scroll_changed {
      out.extend(self.paint_if_needed()?);
    }
    Ok(out)
  }

  fn handle_paste(&mut self, text: &str) -> Result<Vec<WorkerToUi>> {
    let text = clipboard::clamp_clipboard_text(text);
    let scroll_snapshot = self.scroll_state.clone();
    let result = self
      .document
      .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let dom_changed = self.interaction.clipboard_paste(dom, text);
        let caret_scroll =
          crate::interaction::textarea_caret_scroll::textarea_scroll_y_to_reveal_focused_caret(
            dom,
            self.interaction.interaction_state(),
            box_tree,
            fragment_tree,
            &scroll_snapshot,
          );
        (dom_changed, (dom_changed, caret_scroll))
      });
    let (changed, caret_scroll) = match result {
      Ok(result) => result,
      Err(_) => {
        let changed = self
          .document
          .mutate_dom(|dom| self.interaction.clipboard_paste(dom, text));
        (changed, None)
      }
    };

    let mut scroll_changed = false;
    if let Some((textarea_box_id, next_y)) = caret_scroll {
      let mut next_state = self.scroll_state.clone();
      let existing = next_state.element_offset(textarea_box_id);
      let next_offset = Point::new(existing.x, next_y);
      if next_offset == Point::ZERO {
        next_state.elements.remove(&textarea_box_id);
      } else {
        next_state.elements.insert(textarea_box_id, next_offset);
      }
      if next_state != self.scroll_state {
        self.scroll_state = next_state;
        self.document.set_scroll_state(self.scroll_state.clone());
        scroll_changed = true;
      }
    }

    if changed || scroll_changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_key_action(&mut self, key: crate::interaction::KeyAction) -> Result<Vec<WorkerToUi>> {
    // Ensure we have a prepared tree so focus scrolling can compute geometry.
    if self.document.prepared().is_none() {
      let _ = self.force_repaint()?;
    }

    let scroll_snapshot = self.scroll_state.clone();

    let mut picker_value: Option<String> = None;
    let result = self
      .document
      .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let (dom_changed, action) = self.interaction.key_activate_with_layout_artifacts(
          dom,
          Some(box_tree),
          fragment_tree,
          key,
          &self.current_url,
          &self.base_url,
        );

        if let InteractionAction::OpenColorPicker { input_node_id } = &action {
          picker_value = crate::dom::find_node_mut_by_preorder_id(dom, *input_node_id)
            .and_then(|node| crate::dom::input_color_value_string(node));
        }

        let focused = self.interaction.focused_node_id();
        let (focused_is_input, focused_is_textarea, focused_is_select, focused_is_button) = focused
          .and_then(|focused_id| {
            crate::dom::find_node_mut_by_preorder_id(dom, focused_id).map(|node| {
              (
                dom_is_input(node),
                dom_is_textarea(node),
                dom_is_select(node),
                dom_is_button(node),
              )
            })
          })
          .unwrap_or((false, false, false, false));

        let focus_scroll = match &action {
          InteractionAction::FocusChanged {
            node_id: Some(node_id),
          } => focus_scroll::scroll_state_for_focus(box_tree, fragment_tree, &scroll_snapshot, *node_id),
          _ => None,
        };

        let caret_scroll =
          crate::interaction::textarea_caret_scroll::textarea_scroll_y_to_reveal_focused_caret(
            dom,
            self.interaction.interaction_state(),
            box_tree,
            fragment_tree,
            focus_scroll.as_ref().unwrap_or(&scroll_snapshot),
          );

        (
          dom_changed,
          (
            dom_changed,
            action,
            focus_scroll,
            caret_scroll,
            focused_is_input,
            focused_is_textarea,
            focused_is_select,
            focused_is_button,
          ),
        )
      });

    let (
      changed,
      action,
      focus_scroll,
      caret_scroll,
      focused_is_input,
      focused_is_textarea,
      focused_is_select,
      focused_is_button,
    ) = match result {
      Ok(result) => result,
      Err(_) => {
        let mut action = InteractionAction::None;
        let mut focused_is_input = false;
        let mut focused_is_textarea = false;
        let mut focused_is_select = false;
        let mut focused_is_button = false;
        let mut fallback_picker_value: Option<String> = None;
        let changed = self.document.mutate_dom(|dom| {
          let (dom_changed, next_action) =
            self
              .interaction
              .key_activate(dom, key, &self.current_url, &self.base_url);
          if let InteractionAction::OpenColorPicker { input_node_id } = &next_action {
            fallback_picker_value = crate::dom::find_node_mut_by_preorder_id(dom, *input_node_id)
              .and_then(|node| crate::dom::input_color_value_string(node));
          }
          action = next_action;

          if let Some(focused_id) = self.interaction.focused_node_id() {
            if let Some(node) = crate::dom::find_node_mut_by_preorder_id(dom, focused_id) {
              focused_is_input = dom_is_input(node);
              focused_is_textarea = dom_is_textarea(node);
              focused_is_select = dom_is_select(node);
              focused_is_button = dom_is_button(node);
            }
          }

          dom_changed
        });
        if fallback_picker_value.is_some() {
          picker_value = fallback_picker_value;
        }
        (
          changed,
          action,
          None,
          None,
          focused_is_input,
          focused_is_textarea,
          focused_is_select,
          focused_is_button,
        )
      }
    };
    let action_is_none = matches!(&action, InteractionAction::None);

    let mut scroll_changed = false;
    if let Some(next_scroll) = focus_scroll {
      if next_scroll != self.scroll_state {
        self.scroll_state = next_scroll;
        self.document.set_scroll_state(self.scroll_state.clone());
        scroll_changed = true;
      }
    }
    if let Some((textarea_box_id, next_y)) = caret_scroll {
      let mut next_state = self.scroll_state.clone();
      let existing = next_state.element_offset(textarea_box_id);
      let next_offset = Point::new(existing.x, next_y);
      if next_offset == Point::ZERO {
        next_state.elements.remove(&textarea_box_id);
      } else {
        next_state.elements.insert(textarea_box_id, next_offset);
      }
      if next_state != self.scroll_state {
        self.scroll_state = next_state;
        self.document.set_scroll_state(self.scroll_state.clone());
        scroll_changed = true;
      }
    }

    // Basic keyboard scrolling: when scroll keys are pressed and the focused element is not a form
    // control that would normally consume them, treat the key as a viewport scrolling shortcut.
    if action_is_none {
      let focus_consumes_space =
        focused_is_input || focused_is_textarea || focused_is_select || focused_is_button;
      let focus_consumes_arrows = focused_is_input || focused_is_textarea || focused_is_select;
      let focus_consumes_home_end = focus_consumes_arrows;

      let allow_scroll = match key {
        crate::interaction::KeyAction::Space | crate::interaction::KeyAction::ShiftSpace => {
          !focus_consumes_space
        }
        crate::interaction::KeyAction::PageUp | crate::interaction::KeyAction::PageDown => {
          !focus_consumes_arrows
        }
        crate::interaction::KeyAction::ArrowDown | crate::interaction::KeyAction::ArrowUp => {
          !focus_consumes_arrows
        }
        crate::interaction::KeyAction::Home
        | crate::interaction::KeyAction::End
        | crate::interaction::KeyAction::ShiftHome
        | crate::interaction::KeyAction::ShiftEnd => !focus_consumes_home_end,
        _ => false,
      };

      if allow_scroll {
        // Mirror render_worker behavior by reusing our existing scroll handlers.
        return match key {
          crate::interaction::KeyAction::Home | crate::interaction::KeyAction::ShiftHome => {
            self.handle_scroll_to((self.scroll_state.viewport.x, 0.0))
          }
          crate::interaction::KeyAction::End | crate::interaction::KeyAction::ShiftEnd => {
            self.handle_scroll_to((self.scroll_state.viewport.x, f32::MAX))
          }
          crate::interaction::KeyAction::ArrowDown => self.handle_scroll((0.0, 40.0), None),
          crate::interaction::KeyAction::ArrowUp => self.handle_scroll((0.0, -40.0), None),
          crate::interaction::KeyAction::Space => {
            let h = self.viewport_css.1.max(1) as f32;
            let dy = (h * 0.9).max(1.0);
            self.handle_scroll((0.0, dy), None)
          }
          crate::interaction::KeyAction::ShiftSpace => {
            let h = self.viewport_css.1.max(1) as f32;
            let dy = -((h * 0.9).max(1.0));
            self.handle_scroll((0.0, dy), None)
          }
          crate::interaction::KeyAction::PageDown => {
            let h = self.viewport_css.1.max(1) as f32;
            let dy = (h * 0.9).max(1.0);
            self.handle_scroll((0.0, dy), None)
          }
          crate::interaction::KeyAction::PageUp => {
            let h = self.viewport_css.1.max(1) as f32;
            let dy = -((h * 0.9).max(1.0));
            self.handle_scroll((0.0, dy), None)
          }
          _ => Ok(Vec::new()),
        };
      }
    }

    let mut out = Vec::new();
    match action {
      InteractionAction::Navigate { href } => {
        return self.handle_navigation_action(href, NavigationReason::LinkClick);
      }
      InteractionAction::OpenInNewTab { href } => {
        out.push(WorkerToUi::RequestOpenInNewTab {
          tab_id: self.tab_id,
          url: href,
        });
      }
      InteractionAction::OpenInNewTabRequest { request } => {
        out.push(WorkerToUi::RequestOpenInNewTabRequest {
          tab_id: self.tab_id,
          request,
        });
      }
      InteractionAction::NavigateRequest { request } => {
        return self.handle_navigation_request_action(request, NavigationReason::LinkClick);
      }
      InteractionAction::OpenSelectDropdown {
        select_node_id,
        control,
      } => {
        // Back-compat: older UIs listen for `OpenSelectDropdown`.
        out.push(WorkerToUi::OpenSelectDropdown {
          tab_id: self.tab_id,
          select_node_id,
          control: control.clone(),
        });
        let anchor_css = self.select_anchor_css(select_node_id).unwrap_or(Rect::ZERO);
        out.push(WorkerToUi::SelectDropdownOpened {
          tab_id: self.tab_id,
          select_node_id,
          control,
          anchor_css,
        });
      }
      InteractionAction::OpenFilePicker {
        input_node_id,
        multiple,
        accept,
      } => {
        let anchor_css = self.select_anchor_css(input_node_id).unwrap_or(Rect::ZERO);
        out.push(WorkerToUi::FilePickerOpened {
          tab_id: self.tab_id,
          input_node_id,
          multiple,
          accept,
          anchor_css,
        });
      }
      InteractionAction::OpenDateTimePicker { input_node_id, kind } => {
        let anchor_css = self
          .select_anchor_css(input_node_id)
          .filter(|rect| {
            rect.origin.x.is_finite()
              && rect.origin.y.is_finite()
              && rect.size.width.is_finite()
              && rect.size.height.is_finite()
          })
          .unwrap_or_else(|| {
            let (x, y) = self.last_pointer_pos_css.unwrap_or((0.0, 0.0));
            Rect::from_xywh(
              if x.is_finite() { x } else { 0.0 },
              if y.is_finite() { y } else { 0.0 },
              1.0,
              1.0,
            )
          });
        out.push(WorkerToUi::DateTimePickerOpened {
          tab_id: self.tab_id,
          input_node_id,
          kind,
          value: self.date_time_picker_value(input_node_id, kind),
          anchor_css,
        });
      }
      InteractionAction::OpenColorPicker { input_node_id } => {
        let anchor_css = self
          .select_anchor_css(input_node_id)
          .filter(|rect| {
            rect.origin.x.is_finite()
              && rect.origin.y.is_finite()
              && rect.size.width.is_finite()
              && rect.size.height.is_finite()
          })
          .unwrap_or_else(|| {
            let (x, y) = self.last_pointer_pos_css.unwrap_or((0.0, 0.0));
            Rect::from_xywh(
              if x.is_finite() { x } else { 0.0 },
              if y.is_finite() { y } else { 0.0 },
              1.0,
              1.0,
            )
          });
        out.push(WorkerToUi::ColorPickerOpened {
          tab_id: self.tab_id,
          input_node_id,
          value: picker_value.unwrap_or_else(|| "#000000".to_string()),
          anchor_css,
        });
      }
      _ => {}
    }

    // Datalist popup should close when the focused input loses focus (e.g. Tab traversal).
    self.close_datalist_if_focus_changed(&mut out);

    if changed || scroll_changed {
      out.extend(self.paint_if_needed()?);
    }

    Ok(out)
  }

  fn handle_date_time_picker_choose(
    &mut self,
    input_node_id: usize,
    value: &str,
  ) -> Result<Vec<WorkerToUi>> {
    // Mirror the threaded worker semantics: choosing a value should always close the popup even if
    // it results in no DOM mutation (e.g. choosing the already-selected value).
    let mut out = vec![WorkerToUi::DateTimePickerClosed { tab_id: self.tab_id }];
    let engine = &mut self.interaction;
    let changed = self
      .document
      .mutate_dom(|dom| engine.set_date_time_input_value(dom, input_node_id, value));
    if changed {
      out.extend(self.paint_if_needed()?);
    }
    Ok(out)
  }

  fn date_time_picker_value(&mut self, input_node_id: usize, kind: DateTimeInputKind) -> String {
    let mut value = String::new();
    let _ = self.document.mutate_dom(|dom| {
      value = crate::dom::find_node_mut_by_preorder_id(dom, input_node_id)
        .map(|node| match kind {
          DateTimeInputKind::Date => crate::dom::input_date_value_string(node).unwrap_or_default(),
          DateTimeInputKind::Time => crate::dom::input_time_value_string(node).unwrap_or_default(),
          DateTimeInputKind::DateTimeLocal => {
            crate::dom::input_datetime_local_value_string(node).unwrap_or_default()
          }
          DateTimeInputKind::Month => crate::dom::input_month_value_string(node).unwrap_or_default(),
          DateTimeInputKind::Week => crate::dom::input_week_value_string(node).unwrap_or_default(),
        })
        .unwrap_or_default();
      false
    });
    value
  }

  fn handle_select_dropdown_choose(
    &mut self,
    select_node_id: usize,
    option_node_id: usize,
  ) -> Result<Vec<WorkerToUi>> {
    // Mirror the threaded worker semantics: choosing any option in the dropdown overlay should
    // close the popup even if it results in no DOM mutation (e.g. choosing the currently-selected
    // option).
    let mut out = vec![WorkerToUi::SelectDropdownClosed {
      tab_id: self.tab_id,
    }];
    let engine = &mut self.interaction;
    let changed = self
      .document
      .mutate_dom(|dom| engine.activate_select_option(dom, select_node_id, option_node_id, false));
    if changed {
      out.extend(self.paint_if_needed()?);
      Ok(out)
    } else {
      Ok(out)
    }
  }

  fn handle_file_picker_choose(
    &mut self,
    input_node_id: usize,
    paths: Vec<std::path::PathBuf>,
  ) -> Result<Vec<WorkerToUi>> {
    // Mirror the threaded worker semantics: choosing files should close the popup even when it
    // results in no DOM mutation (e.g. choosing the already-selected path).
    let mut out = vec![WorkerToUi::FilePickerClosed {
      tab_id: self.tab_id,
    }];

    let engine = &mut self.interaction;
    let changed = self
      .document
      .mutate_dom(|dom| engine.file_picker_choose(dom, input_node_id, &paths));

    if changed {
      out.extend(self.paint_if_needed()?);
    }

    Ok(out)
  }

  fn handle_color_picker_choose(
    &mut self,
    input_node_id: usize,
    value: String,
  ) -> Result<Vec<WorkerToUi>> {
    // Mirror the threaded worker semantics: choosing a value should close the popup even when it
    // results in no DOM mutation (e.g. choosing the already-set value).
    let mut out = vec![WorkerToUi::ColorPickerClosed { tab_id: self.tab_id }];

    let engine = &mut self.interaction;
    let changed = self
      .document
      .mutate_dom(|dom| engine.set_color_input_value(dom, input_node_id, &value));
    if changed {
      out.extend(self.paint_if_needed()?);
    }

    Ok(out)
  }

  fn handle_navigation_action(
    &mut self,
    href: String,
    reason: NavigationReason,
  ) -> Result<Vec<WorkerToUi>> {
    if let Some(fragment) = same_document_fragment(&self.current_url, &href) {
      return self.navigate_to_fragment(&href, &fragment);
    }
    self.navigate(&href, reason)
  }

  fn handle_navigation_request_action(
    &mut self,
    request: FormSubmission,
    reason: NavigationReason,
  ) -> Result<Vec<WorkerToUi>> {
    match request.method {
      FormSubmissionMethod::Get => self.handle_navigation_action(request.url, reason),
      FormSubmissionMethod::Post => self.navigate_form_submission(request, reason),
    }
  }

  fn navigate_to_fragment(&mut self, href: &str, fragment: &str) -> Result<Vec<WorkerToUi>> {
    let mut out = Vec::new();
    out.push(WorkerToUi::NavigationStarted {
      tab_id: self.tab_id,
      url: href.to_string(),
    });

    let Some(prepared) = self.document.prepared() else {
      return Ok(out);
    };

    let viewport = prepared.fragment_tree().viewport_size();
    let offset = if fragment.is_empty() {
      Some(Point::ZERO)
    } else {
      crate::interaction::scroll_offset_for_fragment_target(
        self.document.dom(),
        prepared.box_tree(),
        prepared.fragment_tree(),
        fragment,
        viewport,
      )
    };

    if let Some(offset) = offset {
      let mut next = self.scroll_state.clone();
      next.viewport = offset;
      if next != self.scroll_state {
        self.scroll_state = next;
        self.document.set_scroll_state(self.scroll_state.clone());
      }
    }

    // Update visible URL state.
    self.current_url = href.to_string();
    self.base_url = strip_fragment(href);

    // Repaint (includes any DOM changes like visited state).
    out.extend(self.paint_if_needed()?);

    out.push(WorkerToUi::NavigationCommitted {
      tab_id: self.tab_id,
      url: self.current_url.clone(),
      title: find_document_title(self.document.dom()),
      can_go_back: false,
      can_go_forward: false,
    });

    Ok(out)
  }

  fn navigate(&mut self, url: &str, _reason: NavigationReason) -> Result<Vec<WorkerToUi>> {
    let url = url.trim();
    self.find_query.clear();
    self.find_case_sensitive = false;
    self.find_matches.clear();
    self.find_active_match_index = None;

    let mut out = vec![
      WorkerToUi::FindResult {
        tab_id: self.tab_id,
        query: String::new(),
        case_sensitive: false,
        match_count: 0,
        active_match_index: None,
      },
      WorkerToUi::NavigationStarted {
        tab_id: self.tab_id,
        url: url.to_string(),
      },
    ];

    let options = RenderOptions::new()
      .with_viewport(self.viewport_css.0, self.viewport_css.1)
      .with_device_pixel_ratio(self.dpr);

    let (committed_url, base_url) = if about_pages::is_about_url(url) {
      let html = about_pages::html_for_about_url(url).unwrap_or_else(|| {
        about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"), None)
      });
      self.document.navigate_html_with_options(
        url,
        &html,
        Some(about_pages::ABOUT_BASE_URL),
        options.clone(),
      )?
    } else {
      match self
        .document
        .navigate_url_with_options(url, options.clone())
      {
        Ok((committed_url, base_url)) => (committed_url, base_url),
        Err(err) => {
          out.push(WorkerToUi::NavigationFailed {
            tab_id: self.tab_id,
            url: url.to_string(),
            error: err.to_string(),
            can_go_back: false,
            can_go_forward: false,
          });
          let html = about_pages::error_page_html("Navigation failed", &err.to_string(), Some(url));
          self.document.navigate_html_with_options(
            about_pages::ABOUT_ERROR,
            &html,
            Some(about_pages::ABOUT_BASE_URL),
            options.clone(),
          )?
        }
      }
    };

    self.current_url = committed_url.clone();
    self.base_url = strip_fragment(&base_url);
    self.interaction = InteractionEngine::new();
    self.scroll_state = ScrollState::default();
    self.tick_animation_time_ms = 0.0;
    self.document.set_scroll_state(self.scroll_state.clone());

    self.apply_autofocus_if_present();

    // Paint first frame.
    out.extend(self.force_repaint()?);

    out.push(WorkerToUi::NavigationCommitted {
      tab_id: self.tab_id,
      url: self.current_url.clone(),
      title: find_document_title(self.document.dom()),
      can_go_back: false,
      can_go_forward: false,
    });

    Ok(out)
  }

  fn navigate_form_submission(
    &mut self,
    submission: FormSubmission,
    _reason: NavigationReason,
  ) -> Result<Vec<WorkerToUi>> {
    let url = submission.url.trim();
    self.find_query.clear();
    self.find_case_sensitive = false;
    self.find_matches.clear();
    self.find_active_match_index = None;

    let mut out = vec![
      WorkerToUi::FindResult {
        tab_id: self.tab_id,
        query: String::new(),
        case_sensitive: false,
        match_count: 0,
        active_match_index: None,
      },
      WorkerToUi::NavigationStarted {
        tab_id: self.tab_id,
        url: url.to_string(),
      },
    ];

    let options = RenderOptions::new()
      .with_viewport(self.viewport_css.0, self.viewport_css.1)
      .with_device_pixel_ratio(self.dpr);

    let (committed_url, base_url) = if about_pages::is_about_url(url) {
      // about: pages are internal; ignore the method/body and treat as a normal navigation.
      let html = about_pages::html_for_about_url(url).unwrap_or_else(|| {
        about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"), None)
      });
      self.document.navigate_html_with_options(
        url,
        &html,
        Some(about_pages::ABOUT_BASE_URL),
        options.clone(),
      )?
    } else {
      match self.document.navigate_http_request_with_options(
        url,
        submission.method.as_http_method(),
        &submission.headers,
        submission.body.as_deref(),
        options.clone(),
      ) {
        Ok((committed_url, base_url)) => (committed_url, base_url),
        Err(err) => {
          out.push(WorkerToUi::NavigationFailed {
            tab_id: self.tab_id,
            url: url.to_string(),
            error: err.to_string(),
            can_go_back: false,
            can_go_forward: false,
          });
          let html = about_pages::error_page_html("Navigation failed", &err.to_string(), Some(url));
          self.document.navigate_html_with_options(
            about_pages::ABOUT_ERROR,
            &html,
            Some(about_pages::ABOUT_BASE_URL),
            options.clone(),
          )?
        }
      }
    };

    self.current_url = committed_url.clone();
    self.base_url = strip_fragment(&base_url);
    self.interaction = InteractionEngine::new();
    self.scroll_state = ScrollState::default();
    self.tick_animation_time_ms = 0.0;
    self.document.set_scroll_state(self.scroll_state.clone());

    self.apply_autofocus_if_present();

    // Paint first frame.
    out.extend(self.force_repaint()?);

    out.push(WorkerToUi::NavigationCommitted {
      tab_id: self.tab_id,
      url: self.current_url.clone(),
      title: find_document_title(self.document.dom()),
      can_go_back: false,
      can_go_forward: false,
    });

    Ok(out)
  }

  fn paint_if_needed(&mut self) -> Result<Vec<WorkerToUi>> {
    let Some(frame) = self
      .document
      .render_if_needed_with_scroll_state_and_interaction_state(Some(
        self.interaction.interaction_state(),
      ))?
    else {
      return Ok(Vec::new());
    };
    Ok(self.emit_frame(frame))
  }

  fn force_repaint(&mut self) -> Result<Vec<WorkerToUi>> {
    let frame = self
      .document
      .render_frame_with_scroll_state_and_interaction_state(Some(
        self.interaction.interaction_state(),
      ))?;
    Ok(self.emit_frame(frame))
  }

  fn emit_frame(&mut self, mut frame: crate::PaintedFrame) -> Vec<WorkerToUi> {
    let mut out = Vec::new();

    self.scroll_state = frame.scroll_state.clone();
    if self.scroll_state != self.last_reported_scroll_state {
      out.push(WorkerToUi::ScrollStateUpdated {
        tab_id: self.tab_id,
        scroll: self.scroll_state.clone(),
      });
      self.last_reported_scroll_state = self.scroll_state.clone();
    }

    // Prefer the actual DPR used by the prepared document after layout.
    if let Some(prepared) = self.document.prepared() {
      self.dpr = prepared.device_pixel_ratio();
    }

    // Recompute matches after layout updates so highlight rects remain accurate across relayout.
    if !self.find_query.is_empty() {
      let prev_count = self.find_matches.len();
      let prev_active = self.find_active_match_index;
      self.rebuild_find_matches();
      if prev_count != self.find_matches.len() || prev_active != self.find_active_match_index {
        out.push(self.find_result_msg());
      }
    }

    self.apply_find_highlight(&mut frame.pixmap);

    out.push(WorkerToUi::FrameReady {
      tab_id: self.tab_id,
      frame: RenderedFrame {
        pixmap: frame.pixmap,
        viewport_css: self.viewport_css,
        dpr: self.dpr,
        scroll_state: self.scroll_state.clone(),
        scroll_metrics: {
          let viewport_css = self.viewport_css;
          let viewport_size = Size::new(viewport_css.0 as f32, viewport_css.1 as f32);
          let bounds = self
            .document
            .prepared()
            .and_then(|prepared| {
              crate::scroll::build_scroll_chain(&prepared.fragment_tree().root, viewport_size, &[])
                .last()
                .map(|s| s.bounds)
            })
            .unwrap_or(crate::scroll::ScrollBounds {
              min_x: 0.0,
              min_y: 0.0,
              max_x: 0.0,
              max_y: 0.0,
            });
          let max_scroll_x = if bounds.max_x.is_finite() {
            bounds.max_x.max(0.0)
          } else {
            0.0
          };
          let max_scroll_y = if bounds.max_y.is_finite() {
            bounds.max_y.max(0.0)
          } else {
            0.0
          };
          ScrollMetrics {
            viewport_css,
            scroll_css: (self.scroll_state.viewport.x, self.scroll_state.viewport.y),
            bounds_css: crate::scroll::ScrollBounds {
              min_x: 0.0,
              min_y: 0.0,
              max_x: max_scroll_x,
              max_y: max_scroll_y,
            },
            content_css: (
              viewport_size.width + max_scroll_x,
              viewport_size.height + max_scroll_y,
            ),
          }
        },
        next_tick: crate::ui::document_ticks::browser_document_wants_ticks(&self.document)
          .then_some(Duration::from_millis(16)),
      },
    });

    out
  }

  fn apply_find_highlight(&self, pixmap: &mut tiny_skia::Pixmap) {
    if self.find_matches.is_empty() {
      return;
    }

    let viewport_w = self.viewport_css.0 as f32;
    let viewport_h = self.viewport_css.1 as f32;
    let viewport_css = Rect::from_xywh(0.0, 0.0, viewport_w, viewport_h);
    let viewport_page = Rect::from_xywh(
      self.scroll_state.viewport.x,
      self.scroll_state.viewport.y,
      viewport_w,
      viewport_h,
    );

    let highlight = Rgba::new(255, 235, 59, 0.25);
    let highlight_active = Rgba::new(255, 193, 7, 0.35);

    let active = self.find_active_match_index;

    for (idx, m) in self.find_matches.iter().enumerate() {
      if Some(idx) == active {
        continue;
      }
      if m.rects.is_empty() || m.bounds == Rect::ZERO {
        continue;
      }
      if m.bounds.intersection(viewport_page).is_none() {
        continue;
      }

      for rect in &m.rects {
        let local = Rect::from_xywh(
          rect.x() - self.scroll_state.viewport.x,
          rect.y() - self.scroll_state.viewport.y,
          rect.width(),
          rect.height(),
        );
        let Some(visible) = local.intersection(viewport_css) else {
          continue;
        };

        let x = visible.x() * self.dpr;
        let y = visible.y() * self.dpr;
        let w = visible.width() * self.dpr;
        let h = visible.height() * self.dpr;
        fill_rect(pixmap, x, y, w, h, highlight);
      }
    }

    let Some(active) = active else {
      return;
    };
    let Some(m) = self.find_matches.get(active) else {
      return;
    };
    if m.rects.is_empty() || m.bounds == Rect::ZERO {
      return;
    }
    if m.bounds.intersection(viewport_page).is_none() {
      return;
    }

    for rect in &m.rects {
      let local = Rect::from_xywh(
        rect.x() - self.scroll_state.viewport.x,
        rect.y() - self.scroll_state.viewport.y,
        rect.width(),
        rect.height(),
      );
      let Some(visible) = local.intersection(viewport_css) else {
        continue;
      };

      let x = visible.x() * self.dpr;
      let y = visible.y() * self.dpr;
      let w = visible.width() * self.dpr;
      let h = visible.height() * self.dpr;
      fill_rect(pixmap, x, y, w, h, highlight_active);
    }
  }
}

fn same_document_fragment(current_url: &str, href: &str) -> Option<String> {
  let current = Url::parse(current_url).ok()?;
  let href = Url::parse(href).ok()?;

  let mut current_base = current.clone();
  current_base.set_fragment(None);
  let mut href_base = href.clone();
  href_base.set_fragment(None);

  (current_base == href_base).then(|| href.fragment().unwrap_or("").to_string())
}

fn strip_fragment(url: &str) -> String {
  let Ok(mut parsed) = Url::parse(url) else {
    return url.to_string();
  };
  parsed.set_fragment(None);
  parsed.to_string()
}

fn is_html_element(node: &crate::dom::DomNode) -> bool {
  matches!(
    node.namespace(),
    Some(ns) if ns.is_empty() || ns == crate::dom::HTML_NAMESPACE
  )
}

fn is_ancestor_or_self(
  index: &crate::interaction::dom_index::DomIndex,
  ancestor: usize,
  mut node: usize,
) -> bool {
  while node != 0 {
    if node == ancestor {
      return true;
    }
    node = *index.parent.get(node).unwrap_or(&0);
  }
  false
}

fn datalist_popup_options(
  dom: &mut crate::dom::DomNode,
  input_node_id: usize,
) -> Option<Vec<DatalistOption>> {
  let index = crate::interaction::dom_index::DomIndex::build(dom);

  let input = index.node(input_node_id)?;
  if !input
    .tag_name()
    .is_some_and(|t| t.eq_ignore_ascii_case("input") && is_html_element(input))
  {
    return None;
  }

  let list_id = input
    .get_attribute_ref("list")
    .map(crate::ui::url::trim_ascii_whitespace)
    .unwrap_or("");
  if list_id.is_empty() {
    return None;
  }

  let datalist_node_id = *index.id_by_element_id.get(list_id)?;
  let datalist = index.node(datalist_node_id)?;
  if !datalist
    .tag_name()
    .is_some_and(|t| t.eq_ignore_ascii_case("datalist") && is_html_element(datalist))
  {
    return None;
  }

  let query = input.get_attribute_ref("value").unwrap_or("");

  let mut options: Vec<DatalistOption> = Vec::new();
  for node_id in 1..=index.len() {
    if !is_ancestor_or_self(&index, datalist_node_id, node_id) {
      continue;
    }

    let node = index.node(node_id)?;
    if !node
      .tag_name()
      .is_some_and(|t| t.eq_ignore_ascii_case("option") && is_html_element(node))
    {
      continue;
    }

    let value = node.get_attribute_ref("value").unwrap_or("").to_string();
    if !query.is_empty() && !value.starts_with(query) {
      continue;
    }

    options.push(DatalistOption {
      option_node_id: node_id,
      value,
      disabled: node.get_attribute_ref("disabled").is_some(),
    });
  }

  (!options.is_empty()).then_some(options)
}

fn duration_to_ms_f32(delta: Duration) -> f32 {
  if delta.is_zero() {
    return 0.0;
  }
  let ms = (delta.as_secs() as f64) * 1000.0 + (delta.subsec_nanos() as f64) / 1_000_000.0;
  if !ms.is_finite() || ms <= 0.0 {
    return 0.0;
  }
  if ms > f32::MAX as f64 {
    f32::MAX
  } else {
    ms as f32
  }
}

#[cfg(test)]
mod tick_tests {
  use super::*;
  use crate::text::font_db::FontConfig;
  use crate::ui::messages::RepaintReason;
  use std::time::Duration;

  fn take_frame_bytes(msgs: Vec<WorkerToUi>) -> Option<(Vec<u8>, RenderedFrame)> {
    for msg in msgs {
      if let WorkerToUi::FrameReady { frame, .. } = msg {
        return Some((frame.pixmap.data().to_vec(), frame));
      }
    }
    None
  }

  #[test]
  fn tick_emits_new_frames_for_css_animation() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let tab_id = TabId(1);
    let viewport_css = (64, 64);
    let dpr = 1.0;

    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            html, body { background: rgb(0, 0, 0); }
            #box {
              width: 64px;
              height: 64px;
              background: rgb(255, 0, 0);
              animation: fade 100ms linear infinite;
            }
            @keyframes fade {
              from { opacity: 0; }
              to { opacity: 1; }
            }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#;

    let mut controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      html,
      "https://example.com/",
      viewport_css,
      dpr,
    )
    .expect("controller from_html_with_renderer");

    let initial = controller
      .handle_message(UiToWorker::RequestRepaint {
        tab_id,
        reason: RepaintReason::Explicit,
      })
      .expect("initial repaint");

    let (_, initial_frame) =
      take_frame_bytes(initial).expect("expected an initial FrameReady message");
    assert!(
      initial_frame.next_tick.is_some(),
      "expected animation fixture to request periodic ticks"
    );

    let tick_delta = Duration::from_millis(16);
    let out = controller
      .handle_message(UiToWorker::Tick {
        tab_id,
        delta: tick_delta,
      })
      .expect("tick 1");
    let (bytes1, _) = take_frame_bytes(out).expect("expected a FrameReady after tick 1");

    let out = controller
      .handle_message(UiToWorker::Tick {
        tab_id,
        delta: tick_delta,
      })
      .expect("tick 2");
    let (bytes2, _) = take_frame_bytes(out).expect("expected a FrameReady after tick 2");

    assert_ne!(
      bytes1, bytes2,
      "expected pixmap to change between tick-driven animation frames"
    );
  }

  #[test]
  fn tick_does_not_repaint_clean_tab() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let tab_id = TabId(1);
    let viewport_css = (32, 32);
    let dpr = 1.0;
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            body { background: rgb(0, 0, 0); }
            #box { width: 32px; height: 32px; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#;

    let mut controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      html,
      "https://example.com/",
      viewport_css,
      dpr,
    )
    .expect("controller from_html_with_renderer");

    let initial = controller
      .handle_message(UiToWorker::RequestRepaint {
        tab_id,
        reason: RepaintReason::Explicit,
      })
      .expect("initial repaint");

    let (_, initial_frame) =
      take_frame_bytes(initial).expect("expected an initial FrameReady message");
    assert!(
      initial_frame.next_tick.is_none(),
      "expected clean fixture to render without time-based effects"
    );

    let out = controller
      .handle_message(UiToWorker::Tick {
        tab_id,
        delta: Duration::from_millis(16),
      })
      .expect("tick");
    assert!(
      !out.iter().any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
      "expected no FrameReady after tick on a clean page"
    );
  }
}

#[cfg(test)]
mod autofocus_tests {
  use super::*;
  use crate::text::font_db::FontConfig;
  use crate::ui::messages::RepaintReason;

  #[test]
  fn autofocus_applies_focus_and_text_edit_state_on_first_render() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");

    let tab_id = TabId(1);
    let viewport_css = (240, 80);
    let dpr = 1.0;
    let html = r#"<!doctype html>
      <html>
        <head><meta charset="utf-8"></head>
        <body>
          <input id="a" value="abc" autofocus>
        </body>
      </html>"#;

    let mut controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      html,
      "https://example.com/",
      viewport_css,
      dpr,
    )
    .expect("controller from_html_with_renderer");

    // Trigger the first render so the controller has had a chance to wire focus state into the
    // renderer pipeline.
    let _ = controller
      .handle_message(UiToWorker::RequestRepaint {
        tab_id,
        reason: RepaintReason::Explicit,
      })
      .expect("initial repaint");

    let autofocus_id =
      crate::interaction::autofocus::autofocus_target_node_id(controller.document().dom())
        .expect("expected autofocus target node");

    let state = controller.interaction_state();
    assert_eq!(state.focused, Some(autofocus_id));
    assert!(
      state.is_focus_within(autofocus_id),
      "expected :focus-within to match the focused element"
    );
    assert!(
      state.focus_visible,
      "expected autofocus to opt into focus-visible semantics"
    );
    assert_eq!(
      state.text_edit.map(|t| t.node_id),
      Some(autofocus_id),
      "expected focused input to initialize text-edit state (caret/selection)"
    );
  }
}

fn dom_is_input(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
}

fn dom_is_textarea(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("textarea"))
}

fn dom_is_select(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
}

fn dom_is_button(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("button"))
}

#[cfg(test)]
mod find_in_page_tests {
  use super::*;
  use crate::text::font_db::FontConfig;
  use crate::ui::messages::RepaintReason;

  #[test]
  fn find_query_and_navigation_produce_results_scroll_and_highlight() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");

    let tab_id = TabId(1);
    let viewport_css = (240, 120);
    let dpr = 1.0;
    let html = r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            body { font: 16px sans-serif; }
          </style>
        </head>
        <body>
          <div>hello</div>
          <div style="height: 2000px"></div>
          <div>hello</div>
        </body>
      </html>"#;

    let mut controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      html,
      "https://example.com/",
      viewport_css,
      dpr,
    )
    .expect("controller from_html_with_renderer");

    let initial = controller
      .handle_message(UiToWorker::RequestRepaint {
        tab_id,
        reason: RepaintReason::Explicit,
      })
      .expect("initial repaint");
    let before = initial
      .into_iter()
      .rev()
      .find_map(|msg| match msg {
        WorkerToUi::FrameReady { frame, .. } => Some(frame.pixmap.data().to_vec()),
        _ => None,
      })
      .expect("expected an initial FrameReady");

    let out = controller
      .handle_message(UiToWorker::FindQuery {
        tab_id,
        query: "HELLO".to_string(),
        case_sensitive: false,
      })
      .expect("find query");

    let mut match_count = None;
    let mut active = None;
    let mut after = None;
    for msg in out {
      match msg {
        WorkerToUi::FindResult {
          match_count: got_count,
          active_match_index,
          ..
        } => {
          match_count = Some(got_count);
          active = active_match_index;
        }
        WorkerToUi::FrameReady { frame, .. } => {
          after = Some(frame.pixmap.data().to_vec());
        }
        _ => {}
      }
    }

    assert_eq!(match_count, Some(2));
    assert_eq!(active, Some(0));
    assert_ne!(
      before,
      after.expect("expected a FrameReady after FindQuery"),
      "expected find highlight overlay to affect rendered pixels"
    );

    let out = controller
      .handle_message(UiToWorker::FindNext { tab_id })
      .expect("find next");

    let mut match_count = None;
    let mut active = None;
    let mut scrolled_y = None;
    for msg in out {
      match msg {
        WorkerToUi::FindResult {
          match_count: got_count,
          active_match_index,
          ..
        } => {
          match_count = Some(got_count);
          active = active_match_index;
        }
        WorkerToUi::ScrollStateUpdated { scroll, .. } => {
          scrolled_y = Some(scroll.viewport.y);
        }
        _ => {}
      }
    }

    assert_eq!(match_count, Some(2));
    assert_eq!(active, Some(1));
    assert!(
      scrolled_y.is_some_and(|y| y > 0.0),
      "expected FindNext to scroll down to the next match (got scroll_y={scrolled_y:?})"
    );
  }
}

#[cfg(test)]
mod scroll_tests {
  use super::*;
  use crate::text::font_db::FontConfig;
  use crate::ui::messages::RepaintReason;

  #[test]
  fn negative_pointer_scroll_is_treated_as_viewport_scroll() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");

    let tab_id = TabId(1);
    let viewport_css = (200, 100);
    let dpr = 1.0;
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #scroller {
              width: 120px;
              height: 60px;
              overflow-y: scroll;
              border: 1px solid black;
            }
            #scroller > .content { height: 400px; }
            .wide { width: 1000px; height: 1px; }
            .spacer { height: 2000px; }
          </style>
        </head>
        <body>
          <div id="scroller"><div class="content"></div></div>
          <div class="wide"></div>
          <div class="spacer"></div>
        </body>
      </html>"#;

    let mut controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      html,
      "https://example.com/",
      viewport_css,
      dpr,
    )
    .expect("controller from_html_with_renderer");

    let _ = controller
      .handle_message(UiToWorker::RequestRepaint {
        tab_id,
        reason: RepaintReason::Explicit,
      })
      .expect("initial repaint");

    let _ = controller
      .handle_message(UiToWorker::ScrollTo {
        tab_id,
        pos_css: (11.0, 11.0),
      })
      .expect("scroll to");

    let after_scroll_to = controller.scroll_state().clone();
    assert!(
      (after_scroll_to.viewport.x - 11.0).abs() < 1e-3
        && (after_scroll_to.viewport.y - 11.0).abs() < 1e-3,
      "expected ScrollTo to set viewport scroll to (11,11), got {:?}",
      after_scroll_to.viewport
    );
    assert!(
      after_scroll_to.elements.is_empty(),
      "expected no element scroll offsets after ScrollTo, got {:?}",
      after_scroll_to.elements
    );

    let _ = controller
      .handle_message(UiToWorker::Scroll {
        tab_id,
        delta_css: (0.0, 40.0),
        pointer_css: Some((-1.0, -1.0)),
      })
      .expect("scroll with negative pointer");

    let after_scroll = controller.scroll_state().clone();
    assert!(
      after_scroll.elements.is_empty(),
      "expected negative pointer scroll to avoid element scroll offsets, got {:?}",
      after_scroll.elements
    );
    assert!(
      (after_scroll.viewport.y - (after_scroll_to.viewport.y + 40.0)).abs() < 1e-3,
      "expected negative pointer scroll to apply delta to viewport scroll (expected y≈{} got {})",
      after_scroll_to.viewport.y + 40.0,
      after_scroll.viewport.y
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod select_dropdown_messages_tests {
  use super::*;
  use crate::dom::{enumerate_dom_ids, DomNode};
  use crate::text::font_db::FontConfig;
  use crate::tree::box_tree::SelectItem;
  use crate::ui::messages::{PointerModifiers, RepaintReason};

  fn node_id_by_id_attr(root: &DomNode, id_attr: &str) -> usize {
    let ids = enumerate_dom_ids(root);
    let mut stack: Vec<&DomNode> = vec![root];
    while let Some(node) = stack.pop() {
      if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
        return *ids
          .get(&(node as *const DomNode))
          .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    panic!("no element with id attribute {id_attr:?}");
  }

  #[test]
  fn select_dropdown_open_and_choose_roundtrip() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");

    let tab_id = TabId(1);
    let viewport_css = (240, 160);
    let dpr = 1.0;
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            select { position: absolute; left: 0; top: 0; width: 200px; height: 28px; }
          </style>
        </head>
        <body>
          <select id="sel">
            <option id="opt_disabled" disabled>Disabled</option>
            <optgroup label="Group A">
              <option id="opt_one" value="one">One</option>
              <option id="opt_two" value="two" selected>Two</option>
            </optgroup>
            <optgroup label="Group B" disabled>
              <option id="opt_three" value="three">Three</option>
            </optgroup>
            <option id="opt_four" value="four">Four</option>
          </select>
        </body>
      </html>
    "#;

    let mut controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      html,
      "https://example.com/",
      viewport_css,
      dpr,
    )
    .expect("controller from_html_with_renderer");

    // Ensure a prepared tree exists for hit-testing and geometry queries.
    let _ = controller
      .handle_message(UiToWorker::RequestRepaint {
        tab_id,
        reason: RepaintReason::Explicit,
      })
      .expect("request repaint");

    let select_node_id = node_id_by_id_attr(controller.document().dom(), "sel");
    let option_one_id = node_id_by_id_attr(controller.document().dom(), "opt_one");

    // Click inside the select control.
    let _ = controller
      .handle_message(UiToWorker::PointerDown {
        tab_id,
        pos_css: (10.0, 10.0),
        button: PointerButton::Primary,
        modifiers: PointerModifiers::NONE,
        click_count: 1,
      })
      .expect("pointer down");

    let out = controller
      .handle_message(UiToWorker::PointerUp {
        tab_id,
        pos_css: (10.0, 10.0),
        button: PointerButton::Primary,
        modifiers: PointerModifiers::NONE,
      })
      .expect("pointer up");

    let opened = out.into_iter().find_map(|msg| match msg {
      WorkerToUi::SelectDropdownOpened {
        tab_id: got_tab,
        select_node_id: got_select,
        control,
        anchor_css,
      } if got_tab == tab_id => Some((got_select, control, anchor_css)),
      _ => None,
    });

    let (got_select_id, control, anchor_css) =
      opened.expect("expected SelectDropdownOpened message");
    assert_eq!(got_select_id, select_node_id);

    let labels: Vec<(String, bool)> = control
      .items
      .iter()
      .map(|item| match item {
        SelectItem::OptGroupLabel { label, disabled } => (format!("optgroup:{label}"), *disabled),
        SelectItem::Option {
          label, disabled, ..
        } => (format!("option:{label}"), *disabled),
      })
      .collect();

    assert_eq!(
      labels,
      vec![
        ("option:Disabled".to_string(), true),
        ("optgroup:Group A".to_string(), false),
        ("option:One".to_string(), false),
        ("option:Two".to_string(), false),
        ("optgroup:Group B".to_string(), true),
        ("option:Three".to_string(), true),
        ("option:Four".to_string(), false),
      ]
    );

    assert!(
      anchor_css.x().is_finite()
        && anchor_css.y().is_finite()
        && anchor_css.width().is_finite()
        && anchor_css.height().is_finite(),
      "anchor_css must be finite (got {anchor_css:?})"
    );
    assert!(
      anchor_css.min_x() >= 0.0
        && anchor_css.min_y() >= 0.0
        && anchor_css.max_x() <= viewport_css.0 as f32
        && anchor_css.max_y() <= viewport_css.1 as f32,
      "anchor_css should be within viewport (viewport={viewport_css:?}, anchor={anchor_css:?})"
    );

    // Choose an enabled option.
    let _ = controller
      .handle_message(UiToWorker::select_dropdown_choose(
        tab_id,
        select_node_id,
        option_one_id,
      ))
      .expect("select dropdown choose");

    let dom = controller.document().dom();
    let ids = enumerate_dom_ids(dom);

    let mut select_ptr: Option<*const DomNode> = None;
    let mut option_one_ptr: Option<*const DomNode> = None;
    let mut stack: Vec<&DomNode> = vec![dom];
    while let Some(node) = stack.pop() {
      if ids.get(&(node as *const DomNode)).copied() == Some(select_node_id) {
        select_ptr = Some(node as *const DomNode);
      }
      if ids.get(&(node as *const DomNode)).copied() == Some(option_one_id) {
        option_one_ptr = Some(node as *const DomNode);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }

    let select_node = unsafe { &*select_ptr.expect("select node pointer") };
    assert_eq!(
      select_node.get_attribute_ref("data-fastr-user-validity"),
      None,
      "renderer must not inject data-fastr-user-validity onto the DOM"
    );
    assert!(
      controller
        .interaction_state()
        .has_user_validity(select_node_id),
      "expected select to flip internal user validity state after choosing an option"
    );

    let option_one_node = unsafe { &*option_one_ptr.expect("option node pointer") };
    assert!(
      option_one_node.get_attribute_ref("selected").is_some(),
      "expected chosen option to have selected attribute"
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod accesskit_focus_scroll_tests {
  use super::*;
  use crate::dom::{enumerate_dom_ids, DomNode};
  use crate::interaction::element_geometry::element_geometry_for_styled_node_id;
  use crate::text::font_db::FontConfig;
  use crate::ui::messages::RepaintReason;

  fn node_id_by_id_attr(root: &DomNode, id_attr: &str) -> usize {
    let ids = enumerate_dom_ids(root);
    let mut stack: Vec<&DomNode> = vec![root];
    while let Some(node) = stack.pop() {
      if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
        return *ids
          .get(&(node as *const DomNode))
          .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    panic!("no element with id attribute {id_attr:?}");
  }

  fn border_box_for(controller: &BrowserTabController, node_id: usize) -> Rect {
    let prepared = controller
      .document()
      .prepared()
      .expect("expected prepared layout artifacts");
    let (geom, _style) = element_geometry_for_styled_node_id(
      prepared.box_tree(),
      prepared.fragment_tree(),
      node_id,
    )
    .unwrap_or_else(|| panic!("expected geometry for node_id={node_id}"));
    geom.border_box
  }

  #[test]
  fn accesskit_focus_scrolls_to_reveal_target() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");

    let tab_id = TabId(1);
    let viewport_css = (240, 120);
    let dpr = 1.0;
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #spacer { height: 2000px; }
            button { font: 16px sans-serif; }
          </style>
        </head>
        <body>
          <div id="spacer"></div>
          <button id="target">Target</button>
        </body>
      </html>"#;

    let mut controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      html,
      "https://example.com/",
      viewport_css,
      dpr,
    )
    .expect("controller from_html_with_renderer");

    // Prime layout artifacts and ensure initial scroll starts at the top.
    let _ = controller
      .handle_message(UiToWorker::RequestRepaint {
        tab_id,
        reason: RepaintReason::Explicit,
      })
      .expect("request repaint");
    assert_eq!(controller.scroll_state().viewport.y, 0.0);

    let target_node_id = node_id_by_id_attr(controller.document().dom(), "target");

    let before_bounds = border_box_for(&controller, target_node_id);
    let before_viewport = Rect::from_xywh(
      controller.scroll_state().viewport.x,
      controller.scroll_state().viewport.y,
      viewport_css.0 as f32,
      viewport_css.1 as f32,
    );
    assert!(
      before_bounds.intersection(before_viewport).is_none(),
      "expected target to start below the fold (bounds={before_bounds:?}, viewport={before_viewport:?})"
    );
    assert!(
      before_bounds.min_y() > viewport_css.1 as f32,
      "expected target to be far below the viewport (min_y={}, viewport_h={})",
      before_bounds.min_y(),
      viewport_css.1
    );

    let _ = controller
      .handle_accesskit_action(target_node_id, accesskit::Action::Focus)
      .expect("handle accesskit focus");

    assert!(
      controller.scroll_state().viewport.y > 0.0,
      "expected focus to scroll down (scroll_y={})",
      controller.scroll_state().viewport.y
    );
    assert!(
      controller.interaction_state().is_focused(target_node_id),
      "expected focus state to update for node_id={target_node_id}"
    );

    let after_bounds = border_box_for(&controller, target_node_id);
    let after_viewport = Rect::from_xywh(
      controller.scroll_state().viewport.x,
      controller.scroll_state().viewport.y,
      viewport_css.0 as f32,
      viewport_css.1 as f32,
    );
    assert!(
      after_bounds.intersection(after_viewport).is_some(),
      "expected focus to scroll target into view (bounds={after_bounds:?}, viewport={after_viewport:?})"
    );

    let before_distance = before_bounds.min_y() - 0.0;
    let after_distance = after_bounds.min_y() - controller.scroll_state().viewport.y;
    assert!(
      after_distance < before_distance,
      "expected target bounds to move closer to viewport (before_y={before_distance}, after_y={after_distance})"
    );
  }

  #[test]
  fn accesskit_scroll_into_view_scrolls_without_changing_focus() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");

    let tab_id = TabId(1);
    let viewport_css = (240, 120);
    let dpr = 1.0;
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #spacer { height: 2000px; }
            button { font: 16px sans-serif; }
          </style>
        </head>
        <body>
          <div id="spacer"></div>
          <button id="target">Target</button>
        </body>
      </html>"#;

    let mut controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      html,
      "https://example.com/",
      viewport_css,
      dpr,
    )
    .expect("controller from_html_with_renderer");

    let _ = controller
      .handle_message(UiToWorker::RequestRepaint {
        tab_id,
        reason: RepaintReason::Explicit,
      })
      .expect("request repaint");
    assert_eq!(controller.scroll_state().viewport.y, 0.0);

    let target_node_id = node_id_by_id_attr(controller.document().dom(), "target");
    assert!(
      controller.interaction_state().focused.is_none(),
      "expected no initial focus"
    );

    let before_bounds = border_box_for(&controller, target_node_id);
    let before_viewport = Rect::from_xywh(0.0, 0.0, viewport_css.0 as f32, viewport_css.1 as f32);
    assert!(
      before_bounds.intersection(before_viewport).is_none(),
      "expected target to start below the fold"
    );

    let _ = controller
      .handle_accesskit_action(target_node_id, accesskit::Action::ScrollIntoView)
      .expect("handle accesskit scroll-into-view");

    assert!(
      controller.scroll_state().viewport.y > 0.0,
      "expected ScrollIntoView to scroll down (scroll_y={})",
      controller.scroll_state().viewport.y
    );
    assert!(
      controller.interaction_state().focused.is_none(),
      "expected ScrollIntoView not to change focus"
    );

    let after_bounds = border_box_for(&controller, target_node_id);
    let after_viewport = Rect::from_xywh(
      controller.scroll_state().viewport.x,
      controller.scroll_state().viewport.y,
      viewport_css.0 as f32,
      viewport_css.1 as f32,
    );
    assert!(
      after_bounds.intersection(after_viewport).is_some(),
      "expected ScrollIntoView to reveal target"
    );
  }
}
