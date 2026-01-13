# Docs

The canonical entry point for this repo’s internal documentation is [`docs/index.md`](index.md).

Browser UI development:

- Running + architecture overview: [`docs/browser_ui.md`](browser_ui.md)
- Desktop browser app overview: [`docs/browser.md`](browser.md)

Multiprocess architecture & security:

- Renderer IPC trust boundary: [`docs/multiprocess_threat_model.md`](multiprocess_threat_model.md)
- Sandboxing overview (renderer process): [`docs/sandboxing.md`](sandboxing.md)
- Site isolation process model (per-origin + OOPIF): [`docs/site_isolation.md`](site_isolation.md)
- Chrome JS bridge (trusted UI pages): [`docs/chrome_js_bridge.md`](chrome_js_bridge.md)
- Renderer-chrome internal schemes (`chrome://` assets, `chrome-action:` actions): [`docs/renderer_chrome_schemes.md`](renderer_chrome_schemes.md)
- Chrome accessibility (AccessKit): [`docs/chrome_accessibility.md`](chrome_accessibility.md)
