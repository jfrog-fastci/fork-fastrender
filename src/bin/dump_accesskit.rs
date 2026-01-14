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

  fn checked_state_to_string(value: accesskit::CheckedState) -> &'static str {
    match value {
      accesskit::CheckedState::True => "true",
      accesskit::CheckedState::False => "false",
      accesskit::CheckedState::Mixed => "mixed",
    }
  }

  fn checked_fields_for_node(
    role: accesskit::Role,
    node: &accesskit::Node,
  ) -> (Option<String>, Option<String>, Option<String>) {
    let Some(value) = node.checked_state() else {
      return (None, None, None);
    };
    let value = checked_state_to_string(value).to_string();
    match role {
      // AccessKit 0.11 uses a single `CheckedState` value for multiple control types. We fan it out
      // into separate debug fields based on role so `dump_accesskit` output more closely resembles
      // the ARIA terminology used throughout the codebase/docs.
      accesskit::Role::ToggleButton => (None, None, Some(value)),
      accesskit::Role::Switch => (None, Some(value), None),
      _ => (Some(value), None, None),
    }
  }

  /// Snapshot-friendly summary of a single AccessKit node.
  #[derive(Debug, Clone, PartialEq, Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    selected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    toggled: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pressed: Option<String>,
    /// Whether the node is disabled.
    #[serde(skip_serializing_if = "is_false")]
    disabled: bool,
    /// The accessible value for value-bearing nodes (text fields, combo boxes, etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    /// Numeric value for range-like controls (sliders, progress indicators, etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    numeric_value: Option<f64>,
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
  #[derive(Debug, Clone, PartialEq, Serialize)]
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
        ctx.wants_keyboard_input(),
      );
    }

    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
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
        let role = node.role();
        let (checked, toggled, pressed) = checked_fields_for_node(role, node);
        Some(AccessKitNodeSnapshot {
          id: id.0.get().to_string(),
          role: format!("{:?}", role),
          name,
          expanded: node.is_expanded(),
          selected: node.is_selected(),
          checked,
          toggled,
          pressed,
          disabled: node.is_disabled(),
          value: node.value().map(|value| value.to_string()),
          numeric_value: node.numeric_value(),
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

  #[cfg(test)]
  mod tests {
    use super::*;
    use accesskit::{CheckedState, NodeBuilder, NodeClassSet, Role};

    #[test]
    fn checked_state_is_emitted_as_checked_toggled_or_pressed_based_on_role() {
      let mut classes = NodeClassSet::new();

      let mut checkbox = NodeBuilder::new(Role::CheckBox);
      checkbox.set_checked_state(CheckedState::True);
      let node = checkbox.build(&mut classes);
      let (checked, toggled, pressed) = checked_fields_for_node(Role::CheckBox, &node);
      assert_eq!(checked.as_deref(), Some("true"));
      assert!(toggled.is_none());
      assert!(pressed.is_none());

      let mut toggle_button = NodeBuilder::new(Role::ToggleButton);
      toggle_button.set_checked_state(CheckedState::False);
      let node = toggle_button.build(&mut classes);
      let (checked, toggled, pressed) = checked_fields_for_node(Role::ToggleButton, &node);
      assert!(checked.is_none());
      assert!(toggled.is_none());
      assert_eq!(pressed.as_deref(), Some("false"));

      let mut switch = NodeBuilder::new(Role::Switch);
      switch.set_checked_state(CheckedState::Mixed);
      let node = switch.build(&mut classes);
      let (checked, toggled, pressed) = checked_fields_for_node(Role::Switch, &node);
      assert!(checked.is_none());
      assert_eq!(toggled.as_deref(), Some("mixed"));
      assert!(pressed.is_none());
    }

    #[test]
    fn checked_state_is_omitted_when_not_present() {
      let mut classes = NodeClassSet::new();
      let builder = NodeBuilder::new(Role::Button);
      let node = builder.build(&mut classes);
      let (checked, toggled, pressed) = checked_fields_for_node(Role::Button, &node);
      assert!(checked.is_none());
      assert!(toggled.is_none());
      assert!(pressed.is_none());
    }

    #[test]
    fn snapshot_serialization_skips_optional_state_fields_when_empty() {
      let snapshot = AccessKitNodeSnapshot {
        id: "1".to_string(),
        role: "Button".to_string(),
        name: "".to_string(),
        expanded: None,
        selected: None,
        checked: None,
        toggled: None,
        pressed: None,
        disabled: false,
        value: None,
        numeric_value: None,
        supports_expand: false,
        supports_collapse: false,
      };

      let json = serde_json::to_value(&snapshot).expect("snapshot should serialize");
      let obj = json.as_object().expect("expected JSON object");
      assert!(obj.contains_key("id"));
      assert!(obj.contains_key("role"));
      assert!(!obj.contains_key("name"));
      assert!(!obj.contains_key("expanded"));
      assert!(!obj.contains_key("selected"));
      assert!(!obj.contains_key("checked"));
      assert!(!obj.contains_key("toggled"));
      assert!(!obj.contains_key("pressed"));
      assert!(!obj.contains_key("disabled"));
      assert!(!obj.contains_key("value"));
      assert!(!obj.contains_key("numeric_value"));
      assert!(!obj.contains_key("supports_expand"));
      assert!(!obj.contains_key("supports_collapse"));
    }

    #[test]
    fn snapshot_serialization_includes_state_fields_when_set() {
      let snapshot = AccessKitNodeSnapshot {
        id: "1".to_string(),
        role: "CheckBox".to_string(),
        name: "Example".to_string(),
        expanded: Some(true),
        selected: Some(false),
        checked: Some("mixed".to_string()),
        toggled: Some("true".to_string()),
        pressed: Some("false".to_string()),
        disabled: true,
        value: Some("hello".to_string()),
        numeric_value: Some(1.5),
        supports_expand: true,
        supports_collapse: true,
      };

      let json = serde_json::to_value(&snapshot).expect("snapshot should serialize");
      let obj = json.as_object().expect("expected JSON object");
      assert_eq!(obj.get("expanded").and_then(|v| v.as_bool()), Some(true));
      assert_eq!(obj.get("selected").and_then(|v| v.as_bool()), Some(false));
      assert_eq!(obj.get("checked").and_then(|v| v.as_str()), Some("mixed"));
      assert_eq!(obj.get("toggled").and_then(|v| v.as_str()), Some("true"));
      assert_eq!(obj.get("pressed").and_then(|v| v.as_str()), Some("false"));
      assert_eq!(obj.get("disabled").and_then(|v| v.as_bool()), Some(true));
      assert_eq!(obj.get("value").and_then(|v| v.as_str()), Some("hello"));
      assert_eq!(obj.get("numeric_value").and_then(|v| v.as_f64()), Some(1.5));
      assert_eq!(
        obj.get("supports_expand").and_then(|v| v.as_bool()),
        Some(true)
      );
      assert_eq!(
        obj.get("supports_collapse").and_then(|v| v.as_bool()),
        Some(true)
      );
    }
  }
}

#[cfg(feature = "browser_ui")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
  enabled::main()
}
