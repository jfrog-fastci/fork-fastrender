use crate::api::{BrowserDocument, FastRenderFactory, RenderOptions};
use crate::error::{Error, RenderError};
use crate::geometry::Point;
use crate::interaction::{InteractionAction, InteractionEngine};
use crate::render_control::{DeadlineGuard, GlobalStageListenerGuard, RenderDeadline, StageHeartbeat};
use crate::scroll::ScrollState;
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::ui::about_pages;
use crate::ui::cancel::CancelGens;
use crate::ui::history::TabHistory;
use crate::ui::messages::{NavigationReason, PointerButton, RenderedFrame, RepaintReason, TabId, UiToWorker, WorkerToUi};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

struct TabState {
  cancel: CancelGens,
  history: TabHistory,
  document: Option<BrowserDocument>,
  interaction: InteractionEngine,
  url: Option<String>,
  base_url: Option<String>,
  title: Option<String>,
  loading: bool,
  viewport_css: (u32, u32),
  dpr: f32,
}

impl TabState {
  fn new(cancel: CancelGens) -> Self {
    Self {
      cancel,
      history: TabHistory::new(),
      document: None,
      interaction: InteractionEngine::new(),
      url: None,
      base_url: None,
      title: None,
      loading: false,
      viewport_css: (800, 600),
      dpr: 1.0,
    }
  }
}

pub fn spawn_browser_render_thread(
  factory: FastRenderFactory,
) -> std::io::Result<(Sender<UiToWorker>, Receiver<WorkerToUi>, JoinHandle<()>)> {
  let (ui_tx, worker_rx) = std::sync::mpsc::channel::<UiToWorker>();
  let (worker_tx, ui_rx) = std::sync::mpsc::channel::<WorkerToUi>();

  let handle = std::thread::Builder::new()
    .name("browser_render_worker".to_string())
    .stack_size(DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || {
      let mut worker = BrowserRenderThread {
        factory,
        ui_rx: worker_rx,
        ui_tx: worker_tx,
        tabs: HashMap::new(),
        active_tab: None,
      };
      worker.run();
    })?;

  Ok((ui_tx, ui_rx, handle))
}

struct BrowserRenderThread {
  factory: FastRenderFactory,
  ui_rx: Receiver<UiToWorker>,
  ui_tx: Sender<WorkerToUi>,
  tabs: HashMap<TabId, TabState>,
  active_tab: Option<TabId>,
}

impl BrowserRenderThread {
  fn run(&mut self) {
    while let Ok(msg) = self.ui_rx.recv() {
      self.handle_message(msg);
    }
  }

  fn handle_message(&mut self, msg: UiToWorker) {
    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
        cancel,
      } => {
        self.tabs.insert(tab_id, TabState::new(cancel));
        if self.active_tab.is_none() {
          self.active_tab = Some(tab_id);
        }
        if let Some(url) = initial_url {
          self.navigate(tab_id, url, NavigationReason::TypedUrl);
        }
      }
      UiToWorker::NewTab { tab_id, initial_url } => {
        // `NewTab` is an optional protocol alias; treat it the same as `CreateTab` but create our
        // own cancel generations.
        self.tabs.insert(tab_id, TabState::new(CancelGens::new()));
        if self.active_tab.is_none() {
          self.active_tab = Some(tab_id);
        }
        if let Some(url) = initial_url {
          self.navigate(tab_id, url, NavigationReason::TypedUrl);
        }
      }
      UiToWorker::CloseTab { tab_id } => {
        self.tabs.remove(&tab_id);
        if self.active_tab == Some(tab_id) {
          self.active_tab = self.tabs.keys().next().copied();
        }
      }
      UiToWorker::SetActiveTab { tab_id } => {
        self.active_tab = Some(tab_id);
      }
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        self.navigate(tab_id, url, reason);
      }
      UiToWorker::Tick { .. } => {
        // This worker currently does not embed a JS/event loop; ticks are a no-op.
      }
      UiToWorker::GoBack { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          let Some(entry) = tab.history.go_back() else {
            return;
          };
          entry.url.clone()
        };
        self.navigate(tab_id, url, NavigationReason::Reload);
      }
      UiToWorker::GoForward { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          let Some(entry) = tab.history.go_forward() else {
            return;
          };
          entry.url.clone()
        };
        self.navigate(tab_id, url, NavigationReason::Reload);
      }
      UiToWorker::Reload { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          let Some(entry) = tab.history.reload_target() else {
            return;
          };
          entry.url.clone()
        };
        self.navigate(tab_id, url, NavigationReason::Reload);
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        self.viewport_changed(tab_id, viewport_css, dpr);
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        self.scroll(tab_id, delta_css, pointer_css);
      }
      UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button: _,
      } => {
        self.pointer_move(tab_id, pos_css);
      }
      UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button,
      } => {
        self.pointer_down(tab_id, pos_css, button);
      }
      UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button,
      } => {
        self.handle_pointer_up(tab_id, pos_css, button);
      }
      UiToWorker::SelectDropdownChoose {
        tab_id,
        select_node_id,
        option_node_id,
      } => {
        self.select_dropdown_choose(tab_id, select_node_id, option_node_id);
      }
      UiToWorker::SelectDropdownPick { .. } => {
        // Dropdown popup UI is handled by the window UI and the interaction-capable worker.
      }
      UiToWorker::TextInput { tab_id, text } => {
        self.text_input(tab_id, text);
      }
      UiToWorker::KeyAction { tab_id, key } => {
        self.key_action(tab_id, key);
      }
      UiToWorker::RequestRepaint { tab_id, reason } => {
        self.request_repaint(tab_id, reason);
      }
    }
  }

  fn viewport_changed(&mut self, tab_id: TabId, viewport_css: (u32, u32), dpr: f32) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    let viewport_css = (viewport_css.0.max(1), viewport_css.1.max(1));
    tab.viewport_css = viewport_css;
    tab.dpr = sanitize_dpr(dpr);

    if let Some(doc) = tab.document.as_mut() {
      doc.set_viewport(viewport_css.0, viewport_css.1);
      doc.set_device_pixel_ratio(tab.dpr);
    }

    tab.cancel.bump_paint();
    repaint_tab(
      tab_id,
      tab,
      self.ui_tx.clone(),
      RepaintReason::ViewportChanged,
    );
  }

  fn navigate(&mut self, tab_id: TabId, url: String, reason: NavigationReason) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    let requested_url = url.trim().to_string();

    // Cancel any in-flight navigation/paint work immediately so deadline callbacks can stop it.
    tab.cancel.bump_nav();
    let snapshot_prepare = tab.cancel.snapshot_prepare();
    let prepare_cancel_cb = snapshot_prepare.cancel_callback_for_prepare(&tab.cancel);

    let _ = self.ui_tx.send(WorkerToUi::NavigationStarted {
      tab_id,
      url: requested_url.clone(),
    });
    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: true,
    });
    tab.loading = true;

    // History updates.
    let mut restored_scroll: Option<(f32, f32)> = None;
    match reason {
      NavigationReason::TypedUrl | NavigationReason::LinkClick => {
        tab.history.push(requested_url.clone());
      }
      NavigationReason::Reload => {
        if let Some(entry) = tab.history.reload_target() {
          restored_scroll = Some((entry.scroll_x, entry.scroll_y));
        }
      }
      NavigationReason::BackForward => {
        if let Some(entry) = tab.history.go_back_forward_to(&requested_url) {
          restored_scroll = Some((entry.scroll_x, entry.scroll_y));
        } else {
          tab.history.push(requested_url.clone());
        }
      }
    }

    let viewport_css = tab.viewport_css;
    let dpr = tab.dpr;
    let (scroll_x, scroll_y) = restored_scroll.unwrap_or((0.0, 0.0));

    let mut options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr)
      .with_scroll(scroll_x, scroll_y)
      .with_cancel_callback(Some(prepare_cancel_cb));

    let mut final_url = requested_url.clone();
    // Filled once the navigation has either committed or produced an error page.
    let base_url: Option<String>;
    let mut navigation_error: Option<String> = None;

    // Ensure we have a long-lived per-tab `BrowserDocument` so we can keep the internal renderer
    // (and its caches/fetcher) across navigations.
    if tab.document.is_none() {
      let renderer = match self.factory.build_renderer() {
        Ok(renderer) => renderer,
        Err(err) => {
          let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
            tab_id,
            url: requested_url.clone(),
            error: err.to_string(),
            can_go_back: tab.history.can_go_back(),
            can_go_forward: tab.history.can_go_forward(),
          });
          tab.loading = false;
          let _ = self.ui_tx.send(WorkerToUi::LoadingState {
            tab_id,
            loading: false,
          });
          return;
        }
      };

      let init_options = RenderOptions::default()
        .with_viewport(viewport_css.0, viewport_css.1)
        .with_device_pixel_ratio(dpr);
      let document = match BrowserDocument::new(renderer, "<!doctype html><html></html>", init_options) {
        Ok(doc) => doc,
        Err(err) => {
          let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
            tab_id,
            url: requested_url.clone(),
            error: err.to_string(),
            can_go_back: tab.history.can_go_back(),
            can_go_forward: tab.history.can_go_forward(),
          });
          tab.loading = false;
          let _ = self.ui_tx.send(WorkerToUi::LoadingState {
            tab_id,
            loading: false,
          });
          return;
        }
      };
      tab.document = Some(document);
    }

    let Some(document) = tab.document.as_mut() else {
      return;
    };

    if about_pages::is_about_url(&requested_url) {
      let html = about_pages::html_for_about_url(&requested_url).unwrap_or_else(|| {
        about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {requested_url}"))
      });
      base_url = Some(about_pages::ABOUT_BASE_URL.to_string());

      options.cancel_callback = None;
      if let Err(err) = document.reset_with_html(&html, options.clone()) {
        let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: requested_url.clone(),
          error: err.to_string(),
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        });
        tab.loading = false;
        let _ = self.ui_tx.send(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });
        return;
      }
      document.set_navigation_urls(Some(requested_url.clone()), base_url.clone());
      document.set_document_url_without_invalidation(Some(requested_url.clone()));
    } else {
      let _stage_guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());

      match document.navigate_url(&requested_url, options.clone()) {
        Ok(report) => {
          final_url = report.final_url.clone().unwrap_or_else(|| requested_url.clone());
          base_url = report
            .base_url
            .clone()
            .or_else(|| report.final_url.clone())
            .or_else(|| Some(final_url.clone()));
          // Clear the per-navigation cancel callback before leaving the document live; each
          // repaint installs a fresh snapshot.
          document.set_cancel_callback(None);
        }
        Err(err) => {
          if is_cancel_timeout(&err) {
            // Navigation was cancelled by a newer request. Leave state updates to the latest
            // navigation.
            return;
          }

          navigation_error = Some(err.to_string());
          let html = about_pages::error_page_html("Navigation failed", &err.to_string());
          base_url = Some(about_pages::ABOUT_BASE_URL.to_string());

          options.cancel_callback = None;
          if let Err(err2) = document.reset_with_html(&html, options.clone()) {
            let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
              tab_id,
              url: requested_url.clone(),
              error: err2.to_string(),
              can_go_back: tab.history.can_go_back(),
              can_go_forward: tab.history.can_go_forward(),
            });
            tab.loading = false;
            let _ = self.ui_tx.send(WorkerToUi::LoadingState {
              tab_id,
              loading: false,
            });
            return;
          }
          document.set_navigation_urls(
            Some(requested_url.clone()),
            Some(about_pages::ABOUT_BASE_URL.to_string()),
          );
          document.set_document_url_without_invalidation(Some(requested_url.clone()));
        }
      }
    }

    if let Some(entry) = tab
      .history
      .commit_navigation(&requested_url, Some(&final_url))
    {
      final_url = entry.url.clone();
    }

    let title = crate::html::title::find_document_title(document.dom());
    if let Some(t) = title.clone() {
      tab.history.set_title(t);
    }

    tab.url = Some(final_url.clone());
    tab.base_url = base_url.clone();
    tab.title = title.clone();
    tab.interaction = InteractionEngine::new();

    let _ = self.ui_tx.send(WorkerToUi::NavigationCommitted {
      tab_id,
      url: final_url.clone(),
      title,
      can_go_back: tab.history.can_go_back(),
      can_go_forward: tab.history.can_go_forward(),
    });

    if let Some(error) = navigation_error {
      let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
        tab_id,
        url: requested_url.clone(),
        error,
        can_go_back: tab.history.can_go_back(),
        can_go_forward: tab.history.can_go_forward(),
      });
    }

    tab.loading = false;
    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: false,
    });

    repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Navigation);
  }

  fn request_repaint(&mut self, tab_id: TabId, _reason: RepaintReason) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    tab.cancel.bump_paint();
    repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Explicit);
  }

  fn scroll(&mut self, tab_id: TabId, delta_css: (f32, f32), pointer_css: Option<(f32, f32)>) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let delta_x = sanitize_delta(delta_css.0);
    let delta_y = sanitize_delta(delta_css.1);
    let delta = (delta_x, delta_y);

    if let Some((x, y)) = pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite()) {
      // Prefer pointer-based wheel scrolling when we have a cached layout; this enables nested
      // overflow container scrolling, scroll chaining, and viewport fallback.
      if doc
        .wheel_scroll_at_viewport_point(Point::new(x, y), delta)
        .is_err()
      {
        let mut next = doc.scroll_state();
        next.viewport.x = (next.viewport.x + delta_x).max(0.0);
        next.viewport.y = (next.viewport.y + delta_y).max(0.0);
        doc.set_scroll_state(next);
      }
    } else {
      let mut next = doc.scroll_state();
      next.viewport.x = (next.viewport.x + delta_x).max(0.0);
      next.viewport.y = (next.viewport.y + delta_y).max(0.0);
      doc.set_scroll_state(next);
    }
    tab.cancel.bump_paint();
    repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Scroll);
  }

  fn pointer_move(&mut self, tab_id: TabId, pos_css: (f32, f32)) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let Some(prepared) = doc.prepared() else {
      return;
    };

    let scroll_state = doc.scroll_state();
    let fragments = prepared.fragment_tree().clone();

    // Avoid borrow conflicts with `doc.mutate_dom`.
    let box_tree = prepared.box_tree().clone();

    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let changed = doc.mutate_dom(|dom| {
      tab
        .interaction
        .pointer_move(dom, &box_tree, &fragments, &scroll_state, viewport_point)
    });
    if changed {
      tab.cancel.bump_paint();
      repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Input);
    }
  }

  fn pointer_down(&mut self, tab_id: TabId, pos_css: (f32, f32), button: PointerButton) {
    if !matches!(button, PointerButton::Primary) {
      return;
    }
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let Some(prepared) = doc.prepared() else {
      return;
    };

    let scroll_state = doc.scroll_state();
    let fragments = prepared.fragment_tree().clone();
    let box_tree = prepared.box_tree().clone();

    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let changed = doc.mutate_dom(|dom| {
      tab
        .interaction
        .pointer_down(dom, &box_tree, &fragments, &scroll_state, viewport_point)
    });
    if changed {
      tab.cancel.bump_paint();
      repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Input);
    }
  }

  fn handle_pointer_up(&mut self, tab_id: TabId, pos_css: (f32, f32), button: PointerButton) {
    if !matches!(button, PointerButton::Primary) {
      return;
    }
    let mut navigate_to: Option<String> = None;
    let mut dropdown_opened: Option<(usize, crate::tree::box_tree::SelectControl, crate::geometry::Rect)> =
      None;

    {
      let Some(tab) = self.tabs.get_mut(&tab_id) else {
        return;
      };
      let Some(doc) = tab.document.as_mut() else {
        return;
      };
      let Some(prepared) = doc.prepared() else {
        return;
      };

      let scroll_state = doc.scroll_state();
      let fragments = prepared.fragment_tree().clone();
      let box_tree = prepared.box_tree().clone();

      let viewport_point = Point::new(pos_css.0, pos_css.1);
      let document_url = tab.url.as_deref().unwrap_or("");
      let base_url = tab
        .base_url
        .as_deref()
        .or_else(|| tab.url.as_deref())
        .unwrap_or("");
      let document_url = tab.url.as_deref().unwrap_or("");

      let mut action = InteractionAction::None;
      let changed = doc.mutate_dom(|dom| {
        let (dom_changed, act) = tab.interaction.pointer_up_with_scroll(
          dom,
          &box_tree,
          &fragments,
          &scroll_state,
          viewport_point,
          document_url,
          base_url,
        );
        action = act;
        dom_changed
      });

      match action {
        InteractionAction::Navigate { href } => {
          navigate_to = Some(href);
        }
        InteractionAction::OpenSelectDropdown {
          select_node_id,
          control,
        } => {
          let anchor_css = select_anchor_css(&box_tree, &fragments, &scroll_state, select_node_id)
            .filter(|rect| {
              rect.origin.x.is_finite()
                && rect.origin.y.is_finite()
                && rect.size.width.is_finite()
                && rect.size.height.is_finite()
            })
            .unwrap_or_else(|| {
              crate::geometry::Rect::from_xywh(
                if viewport_point.x.is_finite() { viewport_point.x } else { 0.0 },
                if viewport_point.y.is_finite() { viewport_point.y } else { 0.0 },
                0.0,
                0.0,
              )
            });
          dropdown_opened = Some((select_node_id, control, anchor_css));
        }
        _ => {}
      }

      if changed && navigate_to.is_none() {
        tab.cancel.bump_paint();
        repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Input);
      }
    }

    if let Some(href) = navigate_to {
      self.navigate(tab_id, href, NavigationReason::LinkClick);
    }

    if let Some((select_node_id, control, anchor_css)) = dropdown_opened {
      let _ = self.ui_tx.send(WorkerToUi::SelectDropdownOpened {
        tab_id,
        select_node_id,
        control,
        anchor_css,
      });
    }
  }

  fn text_input(&mut self, tab_id: TabId, text: String) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let changed = doc.mutate_dom(|dom| tab.interaction.text_input(dom, &text));
    if changed {
      tab.cancel.bump_paint();
      repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Input);
    }
  }

  fn key_action(&mut self, tab_id: TabId, key: crate::interaction::KeyAction) {
    let mut navigate_to: Option<String> = None;

    {
      let Some(tab) = self.tabs.get_mut(&tab_id) else {
        return;
      };
      let Some(doc) = tab.document.as_mut() else {
        return;
      };

      let base_url = tab
        .base_url
        .as_deref()
        .or_else(|| tab.url.as_deref())
        .unwrap_or("");
      let document_url = tab.url.as_deref().unwrap_or("");

      let result = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, _fragment_tree| {
        let (dom_changed, act) = tab.interaction.key_activate_with_box_tree(
          dom,
          Some(box_tree),
          key,
          document_url,
          base_url,
        );
        (dom_changed, (dom_changed, act))
      });
      let (changed, action) = match result {
        Ok(result) => result,
        Err(_) => {
          let mut action = InteractionAction::None;
          let changed = doc.mutate_dom(|dom| {
            let (dom_changed, act) = tab
              .interaction
              .key_activate(dom, key, document_url, base_url);
            action = act;
            dom_changed
          });
          (changed, action)
        }
      };

      if let InteractionAction::Navigate { href } = action {
        navigate_to = Some(href);
      } else if changed {
        tab.cancel.bump_paint();
        repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Input);
      }
    }

    if let Some(href) = navigate_to {
      self.navigate(tab_id, href, NavigationReason::LinkClick);
    }
  }
  fn select_dropdown_choose(
    &mut self,
    tab_id: TabId,
    select_node_id: usize,
    option_node_id: usize,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let changed = doc.mutate_dom(|dom| {
      crate::interaction::dom_mutation::activate_select_option(
        dom,
        select_node_id,
        option_node_id,
        /*toggle_for_multiple=*/ false,
      )
    });
    if changed {
      tab.cancel.bump_paint();
      repaint_tab(tab_id, tab, self.ui_tx.clone(), RepaintReason::Input);
    }
  }
}

fn select_anchor_css(
  box_tree: &crate::BoxTree,
  fragment_tree: &crate::FragmentTree,
  scroll_state: &ScrollState,
  select_node_id: usize,
) -> Option<crate::geometry::Rect> {
  let select_box_id = {
    let mut stack: Vec<&crate::BoxNode> = vec![&box_tree.root];
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
    found?
  };

  let mut fragment_tree_scrolled = fragment_tree.clone();
  crate::scroll::apply_scroll_offsets(&mut fragment_tree_scrolled, scroll_state);
  let page_rect =
    crate::interaction::absolute_bounds_for_box_id(&fragment_tree_scrolled, select_box_id)?;
  Some(page_rect.translate(crate::geometry::Point::new(
    -scroll_state.viewport.x,
    -scroll_state.viewport.y,
  )))
}

fn sanitize_delta(value: f32) -> f32 {
  if value.is_finite() { value } else { 0.0 }
}

fn sanitize_dpr(value: f32) -> f32 {
  if value.is_finite() && value > 0.0 { value } else { 1.0 }
}

fn is_cancel_timeout(err: &Error) -> bool {
  matches!(err, Error::Render(RenderError::Timeout { .. }))
}

fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> GlobalStageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  GlobalStageListenerGuard::new(listener)
}

fn repaint_tab(tab_id: TabId, tab: &mut TabState, ui_tx: Sender<WorkerToUi>, _reason: RepaintReason) {
  let _stage_guard = forward_stage_heartbeats(tab_id, ui_tx.clone());

  let (scroll_state, pixmap, frame_dpr) = {
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    if doc.needs_layout() {
      let snapshot = tab.cancel.snapshot_prepare();
      let cancel_cb = snapshot.cancel_callback_for_prepare(&tab.cancel);
      doc.set_cancel_callback(Some(cancel_cb.clone()));
      let deadline = RenderDeadline::new(None, Some(cancel_cb));
      let result = {
        let _deadline_guard = DeadlineGuard::install(Some(&deadline));
        doc.render_frame_with_scroll_state()
      };
      doc.set_cancel_callback(None);

      let frame = match result {
        Ok(frame) => frame,
        Err(err) => {
          if is_cancel_timeout(&err) {
            return;
          }
          let _ = ui_tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("repaint failed: {err}"),
          });
          return;
        }
      };

      if snapshot != tab.cancel.snapshot_prepare() {
        return;
      }

      let dpr = doc
        .prepared()
        .map(|p| p.device_pixel_ratio())
        .unwrap_or(tab.dpr);
      (frame.scroll_state, frame.pixmap, dpr)
    } else {
      let snapshot = tab.cancel.snapshot_paint();
      let cancel_cb = snapshot.cancel_callback_for_paint(&tab.cancel);
      let deadline = RenderDeadline::new(None, Some(cancel_cb));

      let frame = match doc.paint_from_cache_frame_with_deadline(Some(&deadline)) {
        Ok(frame) => frame,
        Err(err) => {
          if is_cancel_timeout(&err) {
            return;
          }
          let _ = ui_tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("repaint failed: {err}"),
          });
          return;
        }
      };

      if snapshot != tab.cancel.snapshot_paint() {
        return;
      }

      let dpr = doc
        .prepared()
        .map(|p| p.device_pixel_ratio())
        .unwrap_or(tab.dpr);
      (frame.scroll_state, frame.pixmap, dpr)
    }
  };

  send_frame(tab_id, tab, ui_tx, scroll_state, pixmap, frame_dpr);
}

fn send_frame(
  tab_id: TabId,
  tab: &mut TabState,
  ui_tx: Sender<WorkerToUi>,
  scroll_state: ScrollState,
  pixmap: crate::Pixmap,
  dpr: f32,
) {
  let frame = RenderedFrame {
    pixmap,
    viewport_css: tab.viewport_css,
    dpr,
    scroll_state: scroll_state.clone(),
  };

  tab
    .history
    .update_scroll(scroll_state.viewport.x, scroll_state.viewport.y);

  let _ = ui_tx.send(WorkerToUi::FrameReady { tab_id, frame });
  let _ = ui_tx.send(WorkerToUi::ScrollStateUpdated {
    tab_id,
    scroll: scroll_state,
  });
}
