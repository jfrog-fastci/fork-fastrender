use crate::geometry::{Point, Size};
use crate::html::title::find_document_title;
use crate::interaction::scroll_wheel::{apply_wheel_scroll_at_point, ScrollWheelInput};
use crate::interaction::{InteractionAction, InteractionEngine};
use crate::scroll::ScrollState;
use crate::ui::about_pages;
use crate::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use crate::{BrowserDocument, FastRender, RenderOptions, Result};
use percent_encoding::percent_decode_str;
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
  viewport_css: (u32, u32),
  dpr: f32,
}

impl BrowserTabController {
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
    let options = RenderOptions::new()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);

    let mut document = BrowserDocument::from_html(html, options)?;
    document.set_navigation_urls(Some(document_url.to_string()), Some(document_url.to_string()));

    Ok(Self {
      tab_id,
      document,
      interaction: InteractionEngine::new(),
      current_url: document_url.to_string(),
      base_url: strip_fragment(document_url),
      scroll_state: ScrollState::default(),
      last_reported_scroll_state: ScrollState::default(),
      viewport_css,
      dpr,
    })
  }

  pub fn tab_id(&self) -> TabId {
    self.tab_id
  }

  pub fn document(&self) -> &BrowserDocument {
    &self.document
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
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } if tab_id == self.tab_id => self.handle_scroll(delta_css, pointer_css),
      UiToWorker::PointerMove {
        tab_id,
        pos_css,
        ..
      } if tab_id == self.tab_id => self.handle_pointer_move(pos_css),
      UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button,
      } if tab_id == self.tab_id => self.handle_pointer_down(pos_css, button),
      UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button,
      } if tab_id == self.tab_id => self.handle_pointer_up(pos_css, button),
      UiToWorker::TextInput { tab_id, text } if tab_id == self.tab_id => self.handle_text_input(&text),
      UiToWorker::KeyAction { tab_id, key } if tab_id == self.tab_id => self.handle_key_action(key),
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } if tab_id == self.tab_id => self.navigate(&url, reason),
      UiToWorker::RequestRepaint { tab_id, .. } if tab_id == self.tab_id => self.force_repaint(),
      _ => Ok(Vec::new()),
    }
  }

  fn handle_viewport_changed(&mut self, viewport_css: (u32, u32), dpr: f32) -> Result<Vec<WorkerToUi>> {
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

    let Some(prepared) = self.document.prepared() else {
      return Ok(Vec::new());
    };

    let mut next_state = self.scroll_state.clone();

    if let Some(pointer_css) = pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite()) {
      let page_point = Point::new(pointer_css.0, pointer_css.1).translate(self.scroll_state.viewport);
      next_state = apply_wheel_scroll_at_point(
        prepared.fragment_tree(),
        &self.scroll_state,
        Size::new(self.viewport_css.0 as f32, self.viewport_css.1 as f32),
        page_point,
        ScrollWheelInput {
          delta_x: delta_css.0,
          delta_y: delta_css.1,
        },
      );
    } else {
      // No pointer location: treat this as a viewport scroll.
      let mut viewport_scroll = next_state.viewport;

      let delta = Point::new(
        if delta_css.0.is_finite() { delta_css.0 } else { 0.0 },
        if delta_css.1.is_finite() { delta_css.1 } else { 0.0 },
      );
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
    let (box_tree_ptr, fragment_tree_ptr) = {
      let Some(prepared) = self.document.prepared() else {
        return Ok(Vec::new());
      };
      let box_tree_ptr = prepared.box_tree() as *const crate::BoxTree;
      let fragment_tree_ptr = prepared.fragment_tree() as *const crate::tree::fragment_tree::FragmentTree;
      (box_tree_ptr, fragment_tree_ptr)
    };

    let viewport_point = Point::new(pos_css.0, pos_css.1);

    let changed = self.document.mutate_dom(|dom| {
      self
        .interaction
        .pointer_move(
          dom,
          unsafe { &*box_tree_ptr },
          unsafe { &*fragment_tree_ptr },
          &self.scroll_state,
          viewport_point,
        )
    });
    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_pointer_down(&mut self, pos_css: (f32, f32), button: PointerButton) -> Result<Vec<WorkerToUi>> {
    if button != PointerButton::Primary {
      return Ok(Vec::new());
    }

    let (box_tree_ptr, fragment_tree_ptr) = {
      let Some(prepared) = self.document.prepared() else {
        return Ok(Vec::new());
      };
      let box_tree_ptr = prepared.box_tree() as *const crate::BoxTree;
      let fragment_tree_ptr = prepared.fragment_tree() as *const crate::tree::fragment_tree::FragmentTree;
      (box_tree_ptr, fragment_tree_ptr)
    };

    let viewport_point = Point::new(pos_css.0, pos_css.1);

    let changed = self.document.mutate_dom(|dom| {
      self
        .interaction
        .pointer_down(
          dom,
          unsafe { &*box_tree_ptr },
          unsafe { &*fragment_tree_ptr },
          &self.scroll_state,
          viewport_point,
        )
    });
    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_pointer_up(&mut self, pos_css: (f32, f32), button: PointerButton) -> Result<Vec<WorkerToUi>> {
    if button != PointerButton::Primary {
      return Ok(Vec::new());
    }

    let (box_tree_ptr, fragment_tree_ptr) = {
      let Some(prepared) = self.document.prepared() else {
        return Ok(Vec::new());
      };
      let box_tree_ptr = prepared.box_tree() as *const crate::BoxTree;
      let fragment_tree_ptr = prepared.fragment_tree() as *const crate::tree::fragment_tree::FragmentTree;
      (box_tree_ptr, fragment_tree_ptr)
    };

    let viewport_point = Point::new(pos_css.0, pos_css.1);

    let mut action = InteractionAction::None;
    let changed = self.document.mutate_dom(|dom| {
      let (dom_changed, next_action) = self.interaction.pointer_up(
        dom,
        unsafe { &*box_tree_ptr },
        unsafe { &*fragment_tree_ptr },
        &self.scroll_state,
        viewport_point,
        &self.current_url,
        &self.base_url,
      );
      action = next_action;
      dom_changed
    });

    if matches!(action, InteractionAction::Navigate { .. }) {
      if let InteractionAction::Navigate { href } = action {
        // Link click navigation.
        return self.handle_navigation_action(href, NavigationReason::LinkClick);
      }
    }

    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_text_input(&mut self, text: &str) -> Result<Vec<WorkerToUi>> {
    let changed = self
      .document
      .mutate_dom(|dom| self.interaction.text_input(dom, text));
    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_key_action(&mut self, key: crate::interaction::KeyAction) -> Result<Vec<WorkerToUi>> {
    let result = self
      .document
      .mutate_dom_with_layout_artifacts(|dom, box_tree, _fragment_tree| {
        let (dom_changed, action) = self.interaction.key_activate_with_box_tree(
          dom,
          Some(box_tree),
          key,
          &self.current_url,
          &self.base_url,
        );
        (dom_changed, (dom_changed, action))
      });
    let (changed, action) = match result {
      Ok(result) => result,
      Err(_) => {
        let mut action = InteractionAction::None;
        let changed = self.document.mutate_dom(|dom| {
          let (dom_changed, next_action) =
            self
              .interaction
              .key_activate(dom, key, &self.current_url, &self.base_url);
          action = next_action;
          dom_changed
        });
        (changed, action)
      }
    };

    if let InteractionAction::Navigate { href } = action {
      return self.handle_navigation_action(href, NavigationReason::LinkClick);
    }

    if changed {
      self.paint_if_needed()
    } else {
      Ok(Vec::new())
    }
  }

  fn handle_navigation_action(&mut self, href: String, reason: NavigationReason) -> Result<Vec<WorkerToUi>> {
    if let Some(fragment) = same_document_fragment(&self.current_url, &href) {
      return self.navigate_to_fragment(&href, &fragment);
    }
    self.navigate(&href, reason)
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
    let mut out = vec![WorkerToUi::NavigationStarted {
      tab_id: self.tab_id,
      url: url.to_string(),
    }];

    let options = RenderOptions::new()
      .with_viewport(self.viewport_css.0, self.viewport_css.1)
      .with_device_pixel_ratio(self.dpr);

    let mut renderer = FastRender::new()?;

    let report = if about_pages::is_about_url(url) {
      let html = about_pages::html_for_about_url(url).unwrap_or_else(|| {
        about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
      });
      renderer.set_base_url(about_pages::ABOUT_BASE_URL);
      let dom = renderer.parse_html(&html)?;
      renderer.prepare_dom_with_options(dom, Some(url), options.clone())?
    } else {
      match renderer.prepare_url(url, options.clone()) {
        Ok(report) => report,
        Err(err) => {
          out.push(WorkerToUi::NavigationFailed {
            tab_id: self.tab_id,
            url: url.to_string(),
            error: err.to_string(),
          });
          let html = about_pages::error_page_html("Navigation failed", &err.to_string());
          renderer.set_base_url(about_pages::ABOUT_BASE_URL);
          let dom = renderer.parse_html(&html)?;
          renderer.prepare_dom_with_options(dom, Some(about_pages::ABOUT_ERROR), options.clone())?
        }
      }
    };

    let final_url = report.final_url.clone().unwrap_or_else(|| url.to_string());
    self.current_url = final_url.clone();
    self.base_url = strip_fragment(report.base_url.as_deref().unwrap_or(&final_url));

    // Replace document + interaction state.
    self.document = BrowserDocument::from_prepared(renderer, report.document, options)?;
    self
      .document
      .set_navigation_urls(Some(final_url.clone()), Some(self.base_url.clone()));
    self.interaction = InteractionEngine::new();
    self.scroll_state = ScrollState::default();
    self.document.set_scroll_state(self.scroll_state.clone());

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
    let Some(frame) = self.document.render_if_needed_with_scroll_state()? else {
      return Ok(Vec::new());
    };
    Ok(self.emit_frame(frame))
  }

  fn force_repaint(&mut self) -> Result<Vec<WorkerToUi>> {
    let frame = self.document.render_frame_with_scroll_state()?;
    Ok(self.emit_frame(frame))
  }

  fn emit_frame(&mut self, frame: crate::PaintedFrame) -> Vec<WorkerToUi> {
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

    out.push(WorkerToUi::FrameReady {
      tab_id: self.tab_id,
      frame: RenderedFrame {
        pixmap: frame.pixmap,
        viewport_css: self.viewport_css,
        dpr: self.dpr,
        scroll_state: self.scroll_state.clone(),
      },
    });

    out
  }
}

fn same_document_fragment(current_url: &str, href: &str) -> Option<String> {
  let current = Url::parse(current_url).ok()?;
  let href = Url::parse(href).ok()?;

  let mut current_base = current.clone();
  current_base.set_fragment(None);
  let mut href_base = href.clone();
  href_base.set_fragment(None);

  (current_base == href_base).then(|| {
    let raw = href.fragment().unwrap_or("");
    percent_decode_str(raw).decode_utf8_lossy().into_owned()
  })
}

fn strip_fragment(url: &str) -> String {
  let Ok(mut parsed) = Url::parse(url) else {
    return url.to_string();
  };
  parsed.set_fragment(None);
  parsed.to_string()
}
