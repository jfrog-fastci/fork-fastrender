//! Public API integration tests.
//!
//! These tests exercise `fastrender` as an external consumer would (via `FastRender`), without
//! reaching into internal modules.
mod clip_mask_diagnostics;
mod color_calc;
mod csp_img_data_url;
mod css_double_fetch;
mod css_empty_url_no_fetch;
mod css_import_referrer_semantics;
mod diagnostics;
mod dom2_geometry_scrollport;
mod fragmentation;
mod js_exports;
mod incremental_paint_backend;
mod meta_viewport;
mod prepared;
mod prepared_document_geometry_tree;
mod public_api;
mod quirks_body_percent_height;
mod resource;
mod root_font_size_percent;
mod smoke;
mod svg_document_css;
mod svg_mask_image;
mod threading;
mod ua_smoke;
mod render_control;
mod viewport_bounds_dom_node_ids;
mod scroll_blit_resource_epoch;
