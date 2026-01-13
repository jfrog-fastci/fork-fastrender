# FastRender site isolation (process model + iframe semantics)

This document is the **normative spec** for FastRender’s multiprocess *site isolation* model.
It defines the identifiers and algorithms that decide:

1. Which renderer process a document runs in (process-per-origin).
2. When an iframe becomes an out-of-process iframe (OOPIF).
3. How the browser composites multiple frame surfaces into one viewport.

The goal is that contributors can implement/extend site isolation without “filling in gaps”.

Related:
- Workstream overview: [`instructions/multiprocess_security.md`](../instructions/multiprocess_security.md)
- Current single-process iframe rendering (recursive): [`src/paint/iframe.rs`](../src/paint/iframe.rs)
- Current iframe depth limit knobs: `FastRenderConfig::with_max_iframe_depth` (default `DEFAULT_MAX_IFRAME_DEPTH` in `src/api.rs`)

**Status / repo reality (today):**

- FastRender is currently **single-process**; iframes are rendered by recursively rendering nested
  documents into images (see `src/paint/iframe.rs`).
- The windowed `browser` app currently runs the “renderer” on a dedicated worker **thread**, not a
  separate OS process (see `docs/browser.md` + `docs/multiprocess_threat_model.md`).
- This document specifies the **target process model** for the multiprocess workstream and is
  intended to be treated as **normative** once site isolation lands (so future work stays
  consistent).

## Policy overview (MVP → site isolation)

This repo will likely evolve through two related policies:

### Current MVP: process-per-tab

- **New tab → new renderer process.**
- All navigations and iframes for that tab remain in that one process (no process swaps, no OOPIF).
- Goal: simplest multiprocess boundary with strong cross-tab crash/security isolation.

### Planned: process-per-`SiteKey`

- The browser derives a `SiteKey` for every committed document.
- **Tabs/frames with the same `SiteKey` may share a renderer process.**
- **Navigating to a different `SiteKey` may swap the tab/frame into a different renderer process.**
  - Once process swaps exist, **session history must live in the browser process** (not only in the renderer),
    otherwise back/forward cannot survive renderer replacement.
- Special URL handling is security-critical:
  - `about:` internal pages must not share processes with arbitrary web content.
  - `about:blank` / `about:srcdoc` inherit from their initiator.
  - `data:` uses an opaque key (unique; never shared).
  - `file:` is isolated from network sites and is expected to be brokered by the browser process.

### How this differs from Chrome’s “SiteInstance”

Chrome’s model is more complex than “site → process”:

- Chrome uses **SiteInstance** objects scoped to a **BrowsingInstance** (session history/opener graph), not a single global site key.
- Chrome supports **out-of-process iframes (OOPIF)** as a mature feature set.

FastRender’s initial model is intentionally simpler:

- `SiteKey` is derived directly from the destination URL (plus an initiator for inheriting URLs).
- A browser-owned `FrameTree` is the routing authority.
- A `RendererProcessRegistry` maps `SiteKey -> process`.

If FastRender later needs Chrome-like opener/session-history semantics, we may need to introduce a BrowsingInstance/SiteInstance concept. Until then, `SiteKey` is the source of truth for process assignment.

---

## 0) Scope and threat model

Site isolation is a **security boundary**. Assume:

- A renderer process can be compromised by hostile content.
- Compromised renderer A **must not** read memory/state belonging to site B.
- The browser process is trusted and is responsible for:
  - navigation decisions,
  - process creation/lifecycle,
  - compositing,
  - mediation of network/storage access.

Non-goals for this doc:
- OS sandbox policy details (seccomp/AppContainer/etc). That is handled elsewhere in the multiprocess workstream.
- Perfect Chrome parity (we’re specifying a simpler model that is still coherent and secure).

---

## 1) Definitions

### `Tab`

A **tab** is a browser-UI concept: a handle to a top-level browsing context.

In a multiprocess model with process swaps, a tab’s **session history** (back/forward list) must live in the **browser process**.
The renderer process should be treated as replaceable.

### Renderer process

A **renderer process** is an OS process that runs *untrusted* web content (HTML/CSS/JS) and produces pixels.

It is expected to be sandboxed and to communicate with the browser process via IPC.

One renderer process may host:

- **MVP**: one tab (process-per-tab).
- **Future**: multiple tabs/frames for the same `SiteKey` (process-per-`SiteKey`).

### `OriginKey` (origin key)

An **OriginKey** identifies a document’s security origin (same-origin policy / storage partitioning boundary).

For “network” schemes (`http`, `https`, `ws`, `wss`) this is the tuple:

```
(scheme, host, port)
```

Some documents have an **opaque origin** (unique, not equal to any other origin).

### 1.1 `SiteKey`

`SiteKey` is the *process assignment key*: all documents with the same `SiteKey` are allowed to
share a renderer process.

**Normative model (origin-keyed / per-origin):**

- For `http`/`https`: `SiteKey` is the tuple `(scheme, host, port)` using the effective port
  (i.e. `https://example.com` and `https://example.com:443` are the same key).

This is intentionally **per-origin** (not per-registrable-domain) so cross-origin iframes isolate
strictly without needing eTLD+1 reasoning.

For other schemes we either:

- map to a stable bucket (`file://` → `SiteKey::File`, `about:*` internal pages → `SiteKey::Internal`), or
- use an *opaque* key (`SiteKey::Opaque(_)`) when the document has an opaque origin.

Recommended representation:

```rust
/// The grouping key used for renderer process assignment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SiteKey {
    /// Network origins: https://example.com:443
    Origin { scheme: String, host: String, port: u16 },

    /// All file:// documents currently share one site bucket.
    ///
    /// (This matches FastRender's existing `DocumentOrigin::same_origin` behavior for file:// in
    /// `src/resource.rs`. Tightening file:// origin semantics can be done later, but must update
    /// this doc.)
    File,

    /// Unique (opaque) site keys used for documents with opaque origins.
    Opaque(OpaqueSiteId),

    /// Browser-internal documents that must never share a process with untrusted web content.
    ///
    /// This is used for built-in `about:*` pages like `about:newtab` and `about:history` (excluding
    /// inheriting URLs like `about:blank` and `about:srcdoc`). See §2.6.
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpaqueSiteId(u64);
```

Important invariants:

- **Different `SiteKey`s MUST NOT share a renderer process** (unless site isolation is explicitly
  turned off via a build/runtime flag for debugging).
- `SiteKey::Opaque(_)` values are **never equal** unless they are literally the same `OpaqueSiteId`.
  - i.e. identical `data:` URLs still produce different `SiteKey`s.

### 1.2 `FrameId`

`FrameId` identifies a browsing context (a frame) across processes and across navigations.

Properties:

- Generated by the **browser process**.
- Globally unique for the lifetime of the browser session (never reused).
- Stable across cross-origin / cross-`SiteKey` navigations (the `FrameId` stays the same; only the committed document
  and its `SiteKey`/process may change).

Recommended representation:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameId(u64);
```

### 1.3 `FrameTree`

`FrameTree` is the browser-owned model of nested browsing contexts.

It is *not* the DOM tree. It tracks iframe/embedded browsing contexts at the navigation/process
level and is the single source of truth for routing IPC (input events, navigation commits, etc).

Minimal shape:

```rust
pub struct FrameTree {
    pub root: FrameId,
    pub nodes: HashMap<FrameId, FrameNode>,
}

pub struct FrameNode {
    pub id: FrameId,
    pub parent: Option<FrameId>,
    pub children: Vec<FrameId>,

    /// The SiteKey of the *currently committed* document in this frame.
    pub site: SiteKey,
    pub process: RendererProcessId,

    /// The last committed URL string (for UI/history/debugging). Not trusted input.
    pub current_url: String,
}
```

### 1.4 `RendererProcessRegistry`

`RendererProcessRegistry` is the browser-owned component that creates, tracks, and reuses renderer
processes.

Responsibilities:

- Maintain a mapping `SiteKey -> RendererProcess` (1 process per SiteKey, unless explicitly
  configured otherwise).
- Ensure `SiteKey::Internal` is routed to a **trusted** renderer context (browser process or a
  dedicated privileged renderer) and never co-hosted with untrusted web content.
- Enforce global/per-tab process limits (see §5).
- Reference-count processes by the number of live frames currently assigned to them.
- Handle crashes (mark dead; notify `FrameTree` owners; allow reload to respawn).

Scope (important):

- The registry is **global to the browser process**, not per-tab.
- Therefore, different tabs/windows that navigate to the same `SiteKey` are expected to **reuse the
  same renderer process** (SiteKey-per-process, not tab-per-process).
  - With the per-origin `SiteKey` model in this document, that effectively means “one process per
    origin”.
  - Embeddings may optionally provide a debug mode that forces process-per-tab, but it must be
    opt-in and must not be used as the default site isolation semantics.

Minimal API shape:

```rust
pub struct RendererProcessRegistry { /* ... */ }

impl RendererProcessRegistry {
    /// Return an existing live process for `site`, or spawn a new one.
    pub fn get_or_spawn(&mut self, site: &SiteKey) -> Result<RendererProcessId, SpawnError>;

    /// Decrement refcount; potentially terminate idle processes (policy-driven).
    pub fn release_frame(&mut self, process: RendererProcessId, frame: FrameId);
}
```

---

## 2) Deriving `SiteKey` from a navigation

Site isolation decisions must be deterministic and centralized.

Define a browser-side helper:

```rust
/// `initiator_site` is the SiteKey of the frame that is creating/navigating this document.
/// For top-level user-typed navigations, it can be None.
fn derive_site_key(url: &Url, initiator_site: Option<&SiteKey>) -> SiteKey
```

### 2.1 Network origins (`http`, `https`)

```
SiteKey::Origin {
  scheme: url.scheme().lowercase(),
  host: url.host_str().lowercase(),
  port: url.port_or_known_default(),
}
```

### 2.2 `file:`

All `file://` documents currently map to `SiteKey::File`.

This is intentionally simple and matches FastRender’s current resource-origin behavior (see
`DocumentOrigin::same_origin` in `src/resource.rs`).

Security notes / rationale:

- `file:` content often represents local secrets, and file reads should be brokered by the browser process.
- `SiteKey::File` must **never** share a renderer process with network `SiteKey`s (`http(s)`).
- If we later tighten `file:` origin semantics (e.g. make each `file:` navigation opaque/unique), update this section
  and add tests so we don’t accidentally re-enable `file:`/web co-hosting.

### 2.3 `about:blank`

`about:blank` is special because it is frequently used as:

- The default iframe document when `src` is missing/empty.
- The initial empty document for new browsing contexts.

**Rule (normative):**

- If there is an `initiator_site` (e.g. iframe creation), `about:blank` **inherits** it:
  - `derive_site_key(about:blank, Some(parent_site)) == parent_site.clone()`
- Otherwise (`initiator_site == None`), `about:blank` is treated as an opaque origin:
  - `SiteKey::Opaque(new_opaque_id())`

This matches the “creator origin” intuition: about:blank created by a document is same-origin with
that document; about:blank created by the browser UI is a fresh opaque origin.

### 2.4 `about:srcdoc`

`about:srcdoc` is the internal URL for iframe `srcdoc` documents.

**Rule (normative):**

- `about:srcdoc` always inherits `initiator_site`.
- If `initiator_site == None`, treat it as opaque (defensive; this should not happen in normal
  iframe creation flows).

### 2.5 `data:`

`data:` documents have *opaque origins* (unique, not same-origin with each other).

**Rule (normative):**

- Every committed `data:` navigation gets a fresh `SiteKey::Opaque(new_opaque_id())`.
- `data:` **never** inherits the initiator `SiteKey`.

Rationale:
- If a `data:` frame shared a process with its initiator origin, then a renderer compromise inside
  the `data:` frame would have memory access to the initiator’s origin, defeating the point of site
  isolation.

Operational note:
- This can cause process growth for adversarial pages that generate many `data:` iframes; quotas
  (see §5) must make that safe.

### 2.6 Other `about:*` pages (internal pages)

FastRender supports several internal pages under `about:*` (e.g. `about:newtab`, `about:history`,
`about:bookmarks`). These are browser-generated documents and may embed **privileged browser-owned
state snapshots**.

**Rule (normative):**

- `about:*` URLs other than `about:blank` and `about:srcdoc` map to `SiteKey::Internal`.
- They **do not inherit** `initiator_site`.

Rationale:
- Internal pages must not share a renderer process with arbitrary web content, otherwise a renderer
  compromise in a web page could read the internal page’s in-memory state after navigating.
  - Internal pages may safely share a process with each other (they are all browser-generated and
    belong to the same trust bucket).

Implementation note:
- `SiteKey::Internal` is expected to run in a **trusted** context (browser process or a dedicated
  privileged renderer), since internal pages can embed browser-owned state snapshots (history,
  bookmarks, downloads).

### 2.7 Other schemes (`blob:`, `javascript:`, unknown)

Navigation policy (URL allowlists) should reject dangerous schemes like `javascript:` *before*
process assignment is considered (see `docs/multiprocess_threat_model.md` for the “treat URLs as
capabilities” rule).

For the site isolation process model, we still need an explicit fallback rule:

**Rule (normative):**

- If a URL’s scheme is not covered above (`http(s)` origins, `file:`, `about:blank`, `about:srcdoc`, internal `about:*`, `data:`),
  treat it as a fresh opaque site: `SiteKey::Opaque(new_opaque_id())`.
- Do not attempt to “guess” an origin for schemes we don’t fully model yet. Add explicit handling
  plus tests when a new scheme is introduced (e.g. `blob:` origin tracking).

---

## 3) Process assignment algorithm

This section is the core spec: given a navigation, decide **which process** hosts the committed
document.

### 3.1 Common helper: “commit a navigation”

All navigations (top-level or subframe) use the same flow:

Inputs:
- `frame_id`: the frame being navigated.
- `commit_url`: the URL that will actually be committed for the new document.
  - For network navigations this is the **final URL after redirects** (i.e. the origin-bearing URL
    that the response/body should be associated with).
  - For non-network URLs (`about:*`, `data:`, etc.) this is the URL itself.
- `initiator_site`: site of the frame that initiated the navigation (usually the current site of
  `frame_id` itself; for iframe creation it’s the parent frame site).

Algorithm (normative):

1. `target_site = derive_site_key(commit_url, initiator_site)`
2. If `frame.current_site == target_site` **and** the current renderer process is alive:
   - Stay in the same process (`no process swap`).
3. Else:
   - Ask `RendererProcessRegistry::get_or_spawn(&target_site)`.
     - For `SiteKey::Internal`, this must route to the browser’s trusted internal renderer context
       (not a sandboxed content renderer).
   - If allocation fails due to quota (see §5), the navigation **must not** silently fall back to a
     different `SiteKey`’s process. Instead:
     - treat the navigation as **failed** and do **not** commit a new document that would require a
       different `SiteKey`/process.
       - The frame remains on its previously committed document (often an initial `about:blank`).
       - The browser may surface an error via chrome UI and/or render a non-document placeholder in
         the frame rectangle (for subframes).
     - Security invariant: **never merge `SiteKey`s** (run `target_site` content in some other
       process) to satisfy quotas.
4. Update `FrameTree` node:
   - `node.site = target_site`
   - `node.process = assigned_process`
   - `node.current_url = commit_url` (after redirects)

### 3.1.1 Redirects and process assignment

Redirects are a common source of ambiguity; this section is **normative** to avoid “guessing”.

Rule (normative):

- Process assignment is based on the **committed URL**, not the initial requested URL.
- A cross-origin redirect (i.e. redirect where `derive_site_key(final_url, initiator_site)` differs
  from the initially-requested `SiteKey`) must result in a **process swap before commit**.

Implementation guidance:

- In the target architecture where renderers are sandboxed and cannot perform network fetches, the
  browser/network layer will already observe redirects and can choose the correct process before the
  renderer sees any response bytes.
- If an implementation performs any provisional work in an initial process (discouraged), it must
  restart the navigation in the correct process on cross-origin redirects; it must not commit/load
  origin B bytes in an origin A process.

### 3.2 Top-level navigations

Top-level navigations are navigations of the root frame of a tab/window.

Rule (normative):

- Top-level navigations use `initiator_site = None` when initiated by the browser UI (omnibox,
  bookmarks, history, etc.).
- If a top-level navigation is initiated by an existing document (e.g. `window.open`), it may pass
  an `initiator_site` (out of scope for this doc; the key point is that `derive_site_key` handles
  inherited about:blank correctly when `initiator_site` is provided).

Effects:

- Navigating from `https://a.test/` → `https://b.test/` (different `SiteKey`) triggers a **process swap**.
- Navigating within the same `SiteKey` (including fragment-only changes) stays in-process.

History note:

- Once cross-origin / cross-`SiteKey` process swaps exist, the browser process must own session history; renderers can crash or be replaced
  as part of navigation.

### 3.3 Iframe creation and navigation

When the renderer parses `<iframe>` and determines a new browsing context should exist, the browser
creates a `FrameId` and inserts a node into `FrameTree`.

#### 3.3.1 Determining the iframe’s initial URL

This doc cares about the mapping to process assignment; HTML parsing details are elsewhere.
Process assignment must observe these URL rules:

- If `srcdoc` attribute is present: the iframe document URL is `about:srcdoc`.
  - (This matches current single-process behavior in `src/paint/iframe.rs`.)
- Else if `src` is missing or ASCII-whitespace-only: treat as `about:blank`.
  - (Also matches current behavior; see `display_list_iframe_missing_src_defaults_to_about_blank`
    in `src/paint/display_list_renderer/tests/display_list/iframe.rs`.)
- Else: resolve `src` relative to the parent document base URL.

#### 3.3.2 Same-`SiteKey` iframe (in-process)

Definition:

- “Same-`SiteKey` iframe” means `derive_site_key(iframe_url, Some(parent_site)) == parent_site`.
  - This includes `about:blank` and `about:srcdoc` iframes (both inherit the parent).

Rule (normative):

- Same-`SiteKey` iframes **must be in the same renderer process** as the parent frame.

Reason:
- This is the direct expression of “process-per-`SiteKey`”: if two frames have the same `SiteKey`, they co-reside.
- Note: same-origin scripting is determined by `OriginKey`. In this per-origin `SiteKey` model, `OriginKey` typically
  matches `SiteKey` for network documents. Sharing a process still does not grant same-origin privileges; it only
  affects memory co-residency.

Implementation note:
- The browser still assigns a distinct `FrameId` (it’s a separate browsing context), but
  `FrameTree.process` for the child equals the parent’s process.

#### 3.3.3 Cross-`SiteKey` iframe (OOPIF)

Definition:

- “Cross-`SiteKey` iframe” means `derive_site_key(iframe_url, Some(parent_site)) != parent_site`.
  - This includes `data:` iframes, which are always opaque and therefore cross-origin / cross-`SiteKey`.

Rule (normative):

- Cross-`SiteKey` iframes **must be placed in a different renderer process**, assigned by the target
  `SiteKey`.
- Multiple cross-`SiteKey` iframes with the same `SiteKey` may share the same renderer process (one
  process per SiteKey).

Lifecycle note:
- A cross-`SiteKey` iframe can later navigate such that its derived `SiteKey` equals the parent’s, at which point it
  becomes same-`SiteKey` and may transition to the parent process (a subframe process swap).

---

## 4) Compositing model (how frames become pixels)

Site isolation introduces a practical requirement: **the browser must composite multiple renderer
outputs** into a single window/tab viewport.

### 4.1 Conceptual model

- Each frame (`FrameId`) is rendered into a pixel surface owned by its renderer process.
- The browser process composites those surfaces according to:
  - geometry (position/size),
  - clip (rounded corners, overflow clipping),
  - visual effects (opacity/transforms),
  - paint order (stacking context order).

### 4.2 Minimum data the browser must have

For each *embedder* frame, the browser needs a list of “subframe embeddings” that describe where a
child frame’s surface should appear.

Conceptual structure:

```rust
pub struct SubframeInfo {
    pub child: FrameId,

    /// Affine transform from subframe-local space into the embedder's coordinate space.
    pub transform: AffineTransform,

    /// Clip stack to apply in the embedder's space before drawing the child surface (intersection).
    /// At minimum this should include overflow clipping + rounded corners (border-radius).
    pub clip_stack: Vec<ClipItem>,

    /// Stable key that defines z-order between subframes.
    pub z_index: u64,
}
```

This mirrors how the single-process renderer currently treats iframes as “render-to-image then draw
as replaced content” (see `src/paint/iframe.rs`), but with the critical difference that the *pixels*
come from a different process.

### 4.2.1 Trust boundary: validating subframe embedding data

In a multiprocess model, the **embedder renderer process is untrusted**, so any “subframe embed”
data it produces (explicit `DrawSubframe(FrameId)` items, `SubframeInfo` lists, etc.) must be
treated as attacker-controlled.

**Rules (normative):**

- The browser compositor must validate that every referenced `FrameId` is a **real child** of the
  embedder frame in the browser-owned `FrameTree` (preferably a *direct* child).
  - If a renderer references an unknown/unrelated `FrameId`, the browser must ignore the embed and
    may treat it as a protocol violation (kill the renderer / mark the tab crashed).
- Geometry and effect data must be **bounded and finite**:
  - transforms must contain only finite numbers,
  - clip stacks must have a hard maximum depth,
  - rectangles/sizes must be clamped to reasonable limits (see §5.3),
  - the number of embedded subframes per frame must be capped (frame-tree limits, §5.1).
- The browser must treat embedder-provided ordering keys (`z_index`, stacking keys, etc.) as
  *relative ordering hints* only; the compositor is responsible for enforcing correct paint order
  rules and preventing pathological ordering values from causing resource abuse.

Rationale:
- Without validation, a compromised renderer could attempt to embed a frame it does not own (UI
  spoofing / confusion) or provide degenerate geometry that DoS’es the compositor.

### 4.3 Current limitation: stacking-order assumptions (MVP compositor)

An initial multiprocess implementation often starts with a simple compositor:

1. Render the embedder frame to a single bitmap.
2. Render each child frame to a bitmap.
3. Blit child bitmaps into the embedder bitmap.

This is **not fully correct** because it cannot interleave child-frame content with embedder content
that paints *above* the iframe (e.g. overlays, fixed headers, popups).

Therefore, if the compositor is implemented as “one bitmap per frame + blit children”, it must
declare and enforce a limitation:

- **Assumption (temporary):** iframe rectangles do not overlap embedder content that must appear
  above them (no cross-frame interleaving).

If this limitation is unacceptable for accuracy, the compositor must evolve to a layer-based model:

- The embedder renderer outputs a layer tree / display list that contains explicit
  `DrawSubframe(FrameId)` commands at the correct paint order.
- The browser compositor executes that ordering while treating subframes as opaque layers.

This doc does not mandate *which* of these two compositor implementations ships first, but it does
mandate that the behavior is documented and tested, and that we do not silently get layering wrong.

---

## 5) Limits / quota behavior

Site isolation creates new denial-of-service surfaces (many origins → many processes; many iframes →
many surfaces). Limits must be explicit and must fail safely.

### 5.1 Frame tree limits

Enforce hard caps in the browser process:

- Maximum total frames in a `FrameTree` (e.g. to prevent pathological iframe explosions).
- Maximum iframe depth (nesting).

Notes:
- FastRender already has `max_iframe_depth` (default 3) for single-process recursive iframe
  rendering. Multiprocess site isolation should keep an equivalent limit even though it no longer
  recurses on the call stack; the limit is still important for memory and process-count bounds.

### 5.2 Renderer process limits

Enforce hard caps in `RendererProcessRegistry`:

- Maximum total live renderer processes.
- Maximum total live renderer processes *per tab* (optional but recommended).

Policy (normative):

- **Do not merge different `SiteKey`s into the same process to satisfy quota.**
- When the limit is hit, attempt to reclaim idle processes (processes with no live frames).
- If no reclamation is possible, fail the navigation and surface a structured error.

This preserves the security boundary: “process limit reached” may break a page, but it must not
silently disable site isolation.

### 5.3 Surface size limits

Each renderer and the browser compositor must enforce:

- Maximum frame surface size in CSS pixels and device pixels (prevents huge shared-memory buffers).
- Maximum device pixel ratio and maximum total pixel count per surface.

The existing browser UI already has pixel caps for safety (see `FASTR_BROWSER_MAX_PIXELS` etc in
`docs/browser.md` / `docs/env-vars.md`). Site isolation must apply similar caps per-frame.

---

## 6) Testing strategy

Site isolation is easy to “mostly implement” and accidentally break later. Tests must encode the
process model.

### 6.1 Unit tests (pure, fast)

Add browser-side unit tests for `derive_site_key`:

- `https://a.test/` vs `https://a.test:443/` normalize to the same `SiteKey`.
- `about:blank` inherits initiator site.
- `about:blank` with no initiator is `Opaque`.
- `about:srcdoc` inherits initiator site.
- `about:newtab` / `about:history` map to `SiteKey::Internal` (do not inherit).
- `data:` always produces `Opaque` and never inherits.

Notes:
- Follow the repo’s test organization rules in [`docs/test_architecture.md`](test_architecture.md):
  pure helpers should be unit-tested in `src/`, and integration/process tests should live in the
  existing integration harness (avoid introducing new `tests/*.rs` binaries).

Add unit tests for process assignment decisions:

- Same-`SiteKey` navigation keeps process.
- Cross-`SiteKey` navigation swaps process.
- Two frames with the same `SiteKey` reuse the same process (registry reuse).

### 6.2 Integration tests (multiprocess harness)

Write an integration harness that can:

1. Start a browser process in headless mode.
2. Start N renderer processes.
3. Serve test pages from multiple origins (host and/or port differences).
   - Since `SiteKey` is origin-keyed in this spec, distinct ports are sufficient to force different processes.
4. Assert observable state:
   - frame tree shape (FrameId parent/child),
   - `SiteKey` per frame,
   - renderer process id per frame.

Must-have cases:

- Top-level `https://a` with cross-origin iframe `https://b` → different renderer processes.
- Same-`SiteKey` iframe `https://a` inside `https://a` → same renderer process.
- `srcdoc` iframe inherits parent process.
- `data:` iframe gets its own `Opaque` site and therefore a separate process.

### 6.3 Compositing tests

Add pixel-level tests that confirm the compositor correctly embeds subframe pixels with clipping:

- Basic rectangle positioning.
- Rounded-corner clipping / overflow clipping for the iframe content box.
- (If MVP compositor uses “blit children at end”): tests that demonstrate the known limitation and
  are marked accordingly, or tests that enforce the limitation does not regress once fixed.

### 6.4 Crash/isolation tests

Add tests that:

- Kill a renderer process for one `SiteKey` and verify other frames continue rendering.
- Verify that a compromised renderer cannot request assignment to a different `SiteKey` (browser
  validates all `SiteKey` derivations from URLs itself).

---

## 7) Worked examples (sanity checks)

### Example A: basic OOPIF

```
Tab root navigates to https://a.test/
Document inserts <iframe src="https://b.test/">
```

Expected:
- Root frame `SiteKey = a.test` → process `P(a.test)`
- Child frame `SiteKey = b.test` → process `P(b.test)`
- Browser compositor embeds `P(b.test)` surface into `P(a.test)` output at the iframe content box.

### Example B: missing `src` defaults to `about:blank`

```
https://a.test/ contains <iframe></iframe>
```

Expected:
- Iframe URL is `about:blank`
- `about:blank` inherits `a.test` → iframe runs in `P(a.test)` (same process as parent)

### Example C: `data:` iframe

```
https://a.test/ contains <iframe src="data:text/html,hello"></iframe>
```

Expected:
- Iframe `SiteKey = Opaque(x)` → process `P(Opaque(x))`
- Cross-`SiteKey` relative to parent, therefore OOPIF
- If process quota prevents creating `P(Opaque(x))`, navigation fails; do not reuse `P(a.test)`.

---

## 8) Security notes (what this protects / what it doesn’t)

### Protects against

- **Renderer compromise containment**:
  - process-per-tab (MVP) prevents a compromised renderer from reading memory/state belonging to other tabs.
  - process-per-`SiteKey` (future) additionally prevents cross-origin memory observation within a single tab over time
    (via process swaps).
- **Spectre-style cross-origin attacks** (once per-origin isolation is implemented): reducing cross-origin co-residency reduces
  the value of microarchitectural side channels.
- **Stability**: a renderer crash only takes down the frames/tabs hosted in that renderer process.

### Does not protect against

- Bugs in the **browser process** or in IPC validation (the browser remains trusted and high-value).
- Same-origin attacks: if multiple documents share a `SiteKey` (i.e. the same origin), they can still observe each
  other via shared-process memory if co-hosted.
- Side channels that cross process boundaries (timing attacks, shared OS resources) without additional mitigations.
- Network/transport-layer attacks (TLS validation, request smuggling, etc.)—handled by the network stack and the
  browser’s security checks.
