# Specs (offline references)

This folder contains optional git submodules with upstream spec sources so agents can grep/search them locally.

Submodules:
- `specs/whatwg-html/` — WHATWG HTML Living Standard source (`https://github.com/whatwg/html`)
- `specs/csswg-drafts/` — W3C CSSWG drafts (`https://github.com/w3c/csswg-drafts`)
- `specs/tc39-ecma262/` — ECMAScript spec source (`https://github.com/tc39/ecma262`)
- `specs/whatwg-dom/` — WHATWG DOM Standard source (`https://github.com/whatwg/dom`)
- `specs/whatwg-webidl/` — WHATWG Web IDL source (`https://github.com/whatwg/webidl`)
- `specs/whatwg-url/` — WHATWG URL Standard source (`https://github.com/whatwg/url`)
- `specs/whatwg-fetch/` — WHATWG Fetch Standard source (`https://github.com/whatwg/fetch`)

If your checkout did not initialize submodules, run:
```bash
git submodule update --init
```

Note: `--recursive` will also initialize any **nested** submodules inside other submodules (for example, `engines/ecma-rs` has optional large corpora submodules). Only use `--recursive` when you explicitly want those.

Tips:
- Search within specs using ripgrep, e.g. `rg "shrink-to-fit" specs/csswg-drafts`.
- Prefer referencing normative text from spec sources over blog posts when implementing behavior.

