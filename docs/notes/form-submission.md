# HTML form submission (GET + POST)

FastRender implements a spec-shaped subset of the HTML form submission algorithm in
`src/interaction/form_submit.rs`.

## Supported

- `method=get|post` (case-insensitive, default `get`).
- `enctype` for `post`:
  - `application/x-www-form-urlencoded`
  - `multipart/form-data` (text controls; file inputs are currently treated as “no files selected”)
  - `text/plain`
- Successful controls collection (tree order + `form=`-associated controls):
  - `<input>` text-like types, `<textarea>`, `<select>`
  - checked `checkbox`/`radio`
  - submitter `name=value` pair when present
  - disabled / inert / `<template>` subtrees excluded
- Submitter overrides:
  - `formaction`
  - `formmethod`
  - `formenctype`
- Action URL resolution against the document base URL and fragment stripping.

## Integration

The interaction engine returns:

- `InteractionAction::Navigate { href }` for GET submissions, with the serialized query applied.
- `InteractionAction::NavigateRequest { request }` for POST submissions, carrying the method,
  headers (notably `Content-Type`), and body bytes.

The UI worker/navigation layer performs POST document navigations via
`FastRender::prepare_http_request` / `BrowserDocument::navigate_http_request_with_options`.

