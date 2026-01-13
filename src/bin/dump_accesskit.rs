#[cfg(not(feature = "browser_ui"))]
fn main() {
  eprintln!(
    "The `dump_accesskit` binary requires the `browser_ui` feature.\n\
Run:\n\
  bash scripts/run_limited.sh --as 64G -- \\\n\
    bash scripts/cargo_agent.sh run --features browser_ui --bin dump_accesskit -- --help"
  );
  std::process::exit(2);
}

#[cfg(feature = "browser_ui")]
mod enabled {
  use clap::Parser;
  use fastrender::cli_utils as common;
  use serde::Serialize;

  use common::args::parse_viewport;
  use fastrender::ui::browser_app::{BrowserAppState, BrowserTabState};
  use fastrender::ui::chrome::chrome_ui;
  use fastrender::ui::{menu_bar_ui, MenuBarState};
  use fastrender::ui::messages::TabId;

  /// Snapshot-friendly summary of a single AccessKit node.
  #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
  struct AccessKitNodeSnapshot {
    /// AccessKit's `NodeId` is a `NonZeroU128`.
    ///
    /// We keep it as a string to avoid JSON number portability issues and to keep diffs stable.
    id: String,
    role: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expanded: Option<bool>,
    /// Whether the node explicitly supports the Expand action.
    ///
    /// This is only populated for debugging; most chrome widgets rely on default actions.
    #[serde(skip_serializing_if = "is_false")]
    supports_expand: bool,
    /// Whether the node explicitly supports the Collapse action.
    #[serde(skip_serializing_if = "is_false")]
    supports_collapse: bool,
  }

  fn is_false(value: &bool) -> bool {
    !*value
  }

  /// Snapshot-friendly representation of an AccessKit update.
  #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
  struct AccessKitSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    root_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_id: Option<String>,
    nodes: Vec<AccessKitNodeSnapshot>,
  }

  /// Dump the AccessKit tree update emitted by the egui-based browser chrome UI.
  ///
  /// This is intended as a debugging companion to `dump_a11y`:
  ///
  /// - `dump_a11y` prints FastRender's internal accessibility tree (`src/accessibility.rs`)
  /// - `dump_accesskit` prints the OS-facing AccessKit update emitted by egui (`browser_ui` only)
  #[derive(Parser, Debug)]
  #[command(name = "dump_accesskit", version, about)]
  struct Args {
    /// Window size in egui points as WxH (e.g. 1200x200)
    #[arg(long, value_parser = parse_viewport, default_value = "1200x200")]
    viewport: (u32, u32),

    /// Scale factor from egui points to physical pixels.
    ///
    /// This is usually `Window::scale_factor()` (or `egui_ctx.pixels_per_point()` when the UI
    /// applies its own scaling).
    #[arg(long, default_value = "1.0")]
    pixels_per_point: f32,

    /// Put the chrome in "address bar editing" mode by focusing the address bar.
    #[arg(long)]
    focus_address_bar: bool,

    /// Show the in-window menu bar.
    #[arg(long)]
    show_menu_bar: bool,

    /// Only include nodes with non-empty accessible names.
    #[arg(long)]
    named_only: bool,

    /// Output compact JSON instead of pretty-printing.
    #[arg(long)]
    compact: bool,
  }

  pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );
    app.chrome.show_menu_bar = args.show_menu_bar;
    if args.focus_address_bar {
      app.chrome.request_focus_address_bar = true;
    }

    let ctx = egui::Context::default();
    // AccessKit output is typically enabled/disabled by the platform adapter (egui-winit).
    // In this headless tool we force it on so egui emits an update.
    ctx.enable_accesskit();

    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::pos2(0.0, 0.0),
      egui::vec2(args.viewport.0 as f32, args.viewport.1 as f32),
    ));
    raw.time = Some(0.0);
    raw.focused = true;
    raw.pixels_per_point = Some(args.pixels_per_point);
    ctx.begin_frame(raw);

    // Mirror the real windowed `browser` app: the menu bar is rendered before `chrome_ui` so it can
    // inject egui input events for text fields and so the chrome panel is laid out beneath it.
    if args.show_menu_bar {
      let _commands = menu_bar_ui(
        &ctx,
        &app,
        MenuBarState {
          debug_log_open: false,
          history_panel_open: false,
          bookmarks_panel_open: false,
          page_bookmarked: false,
        },
      );
    }

    let _actions = chrome_ui(&ctx, &mut app, true, |_| None);
    let output = ctx.end_frame();

    let update = output.platform_output.accesskit_update.as_ref().ok_or_else(|| {
      "egui did not emit an AccessKit update. Ensure `ctx.enable_accesskit()` was called."
    })?;

    let root_id = update.tree.as_ref().map(|t| t.root.0.get().to_string());
    let focus_id = update.focus.map(|id| id.0.get().to_string());

    let mut nodes: Vec<AccessKitNodeSnapshot> = update
      .nodes
      .iter()
      .filter_map(|(id, node)| {
        let name = node.name().unwrap_or("").trim().to_string();
        if args.named_only && name.is_empty() {
          return None;
        }
        Some(AccessKitNodeSnapshot {
          id: id.0.get().to_string(),
          role: format!("{:?}", node.role()),
          name,
          expanded: node.is_expanded(),
          supports_expand: node.supports_action(accesskit::Action::Expand),
          supports_collapse: node.supports_action(accesskit::Action::Collapse),
        })
      })
      .collect();

    // Sort for deterministic output: role → name → id.
    nodes.sort_by(|a, b| (&a.role, &a.name, &a.id).cmp(&(&b.role, &b.name, &b.id)));

    let snapshot = AccessKitSnapshot {
      root_id,
      focus_id,
      nodes,
    };

    if args.compact {
      println!("{}", serde_json::to_string(&snapshot)?);
    } else {
      println!("{}", serde_json::to_string_pretty(&snapshot)?);
    }

    Ok(())
  }
}

#[cfg(feature = "browser_ui")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
  enabled::main()
}
