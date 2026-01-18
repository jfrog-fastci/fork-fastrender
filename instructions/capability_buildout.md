# Rendering engine capability buildout (`capability_buildout`)

This workstream focuses on implementing **spec-first rendering primitives** (CSS parsing/cascade, box generation, layout algorithms, paint correctness) without page-specific hacks.

## Owns

- Core rendering pipeline: parse → style → box tree → layout → paint
- CSS/HTML spec compliance improvements that generalize
- Unit tests under `src/**` that pin behavior

## Does NOT own

- Page-by-page fixes or pageset triage loops (see `instructions/pageset_page_loop.md`)
- Browser UI chrome or interaction features

## Invariants / constraints

- No site-specific hacks (no hostname/selector special-cases).
- No pixel nudging after layout; keep the pipeline staged.
- Add regressions for every correctness/stability change.

