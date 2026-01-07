# Specs (offline references)

This folder contains optional git submodules with upstream spec sources so agents can grep/search them locally.

Submodules:
- `specs/whatwg-html/` — WHATWG HTML Living Standard source (`https://github.com/whatwg/html`)
- `specs/csswg-drafts/` — W3C CSSWG drafts (`https://github.com/w3c/csswg-drafts`)

If your checkout did not initialize submodules, run:
```bash
git submodule update --init --recursive
```

Tips:
- Search within specs using ripgrep, e.g. `rg "shrink-to-fit" specs/csswg-drafts`.
- Prefer referencing normative text from spec sources over blog posts when implementing behavior.

