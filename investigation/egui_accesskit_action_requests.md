# egui-winit 0.23 AccessKit action requests (and how page nodes handle them)

## Where the raw requests live

In egui/egui-winit 0.23 (with `egui-winit` built using its `accesskit` feature), the platform
accessibility adapter is `accesskit_winit::Adapter`.

Incoming assistive-tech actions arrive **first** as `accesskit_winit::ActionRequestEvent` values,
delivered by winit as a user event. They carry the raw [`accesskit::ActionRequest`] (including its
target [`accesskit::NodeId`]).

For egui integration, those raw requests are then fed into egui via
`egui_winit::State::on_accesskit_action_request(request)`. Once forwarded, they appear in:

- `egui::RawInput::events` as `egui::Event::AccessKitActionRequest(...)`

Egui’s per-widget convenience API (`egui::InputState::has_accesskit_action_request(egui::Id, Action)`)
is built on top of those raw-input events, but it only works for nodes that egui created for real
widgets (because it maps `egui::Id` → AccessKit `NodeId` internally).

## Why this matters for injected page nodes

Rendered page content is *not* an egui widget tree. When we inject page nodes into the AccessKit
tree (so screen readers can traverse the DOM), those nodes won’t have an `egui::Id`, so
`has_accesskit_action_request` can’t see them.

## Chosen routing strategy

We route page action requests using the **raw** `accesskit::ActionRequest` delivered by
`accesskit_winit` (the winit user event), before the request is forwarded into egui:

1. Decode the target `NodeId` using `ui::decode_page_node_id`, which encodes:
   `(tab_id, document_generation, dom_preorder_node_id)`.
2. Reject stale requests by requiring the encoded `document_generation` to match the tab’s current
   page accessibility generation.
3. Translate the AccessKit action into a backend-agnostic UI↔worker message:
   `UiToWorker::{A11ySetFocus,A11yActivate,A11yScrollIntoView,A11yShowContextMenu}`.
4. For non-page targets, forward the request to egui via
   `egui_winit::State::on_accesskit_action_request` so egui widgets continue to handle their own
   focus/click actions through `has_accesskit_action_request`.

This avoids relying on egui-internal `NodeId → egui::Id` mappings and ensures custom injected page
nodes can reliably receive actions even though they are not native egui widgets.
