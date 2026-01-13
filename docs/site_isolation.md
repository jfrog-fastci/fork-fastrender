# FastRender site isolation (process model + iframe semantics)

This document is the **normative spec** for FastRender’s multiprocess *site isolation* model.
It defines the identifiers and algorithms that decide:

1. Which renderer process a document runs in (process-per-origin).
2. When an iframe becomes an out-of-process iframe (OOPIF).
3. How the browser composites multiple frame surfaces into one viewport.

The goal is that contributors can implement/extend site isolation without “filling in gaps”.

Related:
- Workstream overview: [`instructions/multiprocess_security.md`](../instructions/multiprocess_security.md)
- OS sandbox policy overview (seccomp/AppContainer/etc): [sandboxing.md](sandboxing.md)
- Inline iframe rendering (single-process + fallback path): [`src/paint/iframe.rs`](../src/paint/iframe.rs)
- Current iframe depth limit knobs: `FastRenderConfig::with_max_iframe_depth` (default `DEFAULT_MAX_IFRAME_DEPTH` in `src/api.rs`)

**Status / repo reality (today):**

FastRender supports both a single-process embedding (library-style) and a multiprocess model.

- **Single-process / in-process embedding**: iframes can be rendered by recursively rendering nested
  documents into images (see `src/paint/iframe.rs`). This mode is still used for:
  - one-shot rendering APIs, and
  - “inline iframe” fallback when OOPIF compositing is not supported for a particular embedding
    (see `SubframeEffects` in `crates/fastrender-ipc`).
- **Multiprocess + site isolation**: the browser process owns a `FrameTree` and assigns each frame
  to a site-locked renderer process (`process-per-SiteKey`, defaulting to per-origin).
  - Cross-site iframes become out-of-process iframes (OOPIF).
  - Cross-site navigations may swap a frame into a different renderer process.
  - Defense-in-depth is provided by `SiteLock` (renderer rejects navigations that would commit a
    different site).

Core implementation entry points (helpful for contributors):

- Browser-side frame tree + process registry: `src/multiprocess/{frame_tree.rs,registry.rs,subframes.rs}`
- Site key derivation + isolation policy helpers: `src/site_isolation/`
- IPC schema for OOPIF + compositing: `crates/fastrender-ipc` (`SiteKey`, `SiteLock`, `SubframeInfo`,
  `FramePaintPlan`)
- Multiprocess renderer binary/library: `crates/fastrender-renderer`

## Policy overview (site isolation modes)

The browser’s site isolation behavior is controlled by `FASTR_SITE_ISOLATION_MODE`
(see `src/site_isolation/policy.rs`).

### Mode: `off` (no site isolation)

- All frames for a tab are hosted in one renderer context (single-process or process-per-tab,
  depending on the embedding).
- No out-of-process iframes (OOPIF); cross-origin iframes are rendered inline.
- Navigations do not swap renderer processes.

This mode is useful for debugging and for incremental bring-up, but it is not the intended secure
default.

### Mode: `per-origin` (default)

- The browser derives a `SiteKey` for every committed document (see §2).
- **Frames with the same `SiteKey` may share a renderer process** (`process-per-SiteKey`).
- **Navigating to a different `SiteKey` may swap the frame into a different renderer process.**
  - Once process swaps exist, **session history must live in the browser process** (not only in the
    renderer), otherwise back/forward cannot survive renderer replacement.
- Cross-origin (cross-`SiteKey`) iframes become OOPIFs.
- Each renderer process is configured with a defense-in-depth `SiteLock` derived from the assigned
  `SiteKey`.

### Mode: `per-site` (future / coarse site grouping)

This mode exists so embedders can start plumbing configuration early.

Today:
- Browser-side process assignment still behaves like `per-origin`.
- `SiteLock` derivation may choose a schemeful-site lock (eTLD+1) for `SiteKey::Origin(...)`.

### How this differs from Chrome’s “SiteInstance”

Chrome’s model is more complex than “`SiteKey` → process”:

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
- Compromised renderer A **must not** read memory/state belonging to a different origin / `SiteKey`.
- The browser process is trusted and is responsible for:
  - navigation decisions,
  - process creation/lifecycle,
  - compositing,
  - mediation of network/storage access.

Non-goals for this doc:
- OS sandbox policy details (seccomp/AppContainer/etc). See [sandboxing.md](sandboxing.md).
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

### Defense-in-depth: renderer process `SiteLock`

Site isolation relies on the browser making correct process assignment decisions, but we also want a
**fail-closed guardrail inside the renderer** so a compromised renderer (or a browser bug) cannot
silently co-host cross-site documents inside one OS process.

When the browser spawns/initializes a renderer process, it should provide a **process-level lock**
derived from the `SiteKey` assigned to that process. The renderer stores this lock and rejects any
navigation that would commit a different site.

In code, this is represented by:

- `fastrender_ipc::SiteLock` + `fastrender_ipc::SiteIsolationMode`
- `fastrender_ipc::BrowserToRenderer::SetSiteLock { lock }`

The multiprocess renderer (`crates/fastrender-renderer`) enforces this by sending a deterministic
`RendererToBrowser::NavigationFailed { error: "site lock violation" }` response when a `Navigate`
violates the lock (optionally aborting under a feature flag).

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

- For `http`/`https`: `SiteKey` is the canonical origin tuple `(scheme, host, port)` using the
  effective port (i.e. `https://example.com` and `https://example.com:443` are the same key).

This is intentionally **per-origin** (not per-registrable-domain) so cross-origin iframes isolate
strictly without needing eTLD+1 reasoning.

For URLs/schemes that do not have a stable, shared origin (or where we intentionally avoid
co-hosting unrelated documents), the browser derives an **opaque** site key.

In code, `SiteKey` is implemented (on both sides of IPC) as:

```rust
/// The grouping key used for renderer process assignment (browser + IPC).
///
/// See:
/// - `src/site_isolation/site_key.rs`
/// - `crates/fastrender-ipc` (`SiteKey`, `SiteLock`)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SiteKey {
    /// Regular origin-based site key (HTTP/HTTPS/File).
    Origin(DocumentOrigin),
    /// Opaque site key for documents with an opaque origin (`data:`) or for schemes we do not
    /// model as origin-bearing navigations.
    Opaque(u64),
}
```

Opaque IDs are used in two ways (important for contributors):

- **Fresh opaque**: new unique key per navigation (e.g. `data:`; also `about:blank`/`about:srcdoc`
  when no parent/initiator exists). These are typically monotonic IDs allocated by a `SiteKeyFactory`.
- **Stable opaque**: deterministic hash-derived keys for URLs that should stay isolated but should
  not churn processes on trivial URL changes:
  - `file:` URLs (hashed by absolute path / canonical URL string; see §2.2)
  - internal `about:*` pages (hashed by *page identifier* only; query/fragment ignored; see §2.6)

Important invariants:

- **Different `SiteKey`s MUST NOT share a renderer process** (unless site isolation is explicitly
  turned off via a build/runtime flag for debugging).
- `SiteKey::Opaque(_)` values are **never equal** unless they have the same ID.
  - For `data:` specifically, identical `data:` URLs still produce different `SiteKey`s (fresh
    opaque key per navigation).

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
///
/// `force_opaque_origin` is used when the resulting document must have an opaque origin even if
/// the URL would normally be origin-bearing (e.g. `<iframe sandbox>` without `allow-same-origin`).
fn derive_site_key(url: &str, initiator_site: Option<&SiteKey>, force_opaque_origin: bool) -> SiteKey
```

### 2.0 Summary table

| URL kind | Example | `SiteKey` | Inherits `initiator_site`? |
|---|---|---|---|
| Network origin | `https://a.test/` | `SiteKey::Origin(a.test)` | N/A |
| `file:` | `file:///tmp/a.html` | `SiteKey::Opaque(stable_hash(file_url))` | No |
| `about:blank` | `about:blank` | `initiator_site` if present, else `Opaque(new)` | Yes (if present) |
| `about:srcdoc` | `about:srcdoc` | `initiator_site` if present, else `Opaque(new)` | Yes (if present) |
| Internal `about:*` | `about:history?q=rust` | `SiteKey::Opaque(stable_hash(\"history\"))` | No |
| `data:` | `data:text/html,hi` | `SiteKey::Opaque(new)` | No |
| `blob:` | `blob:https://a.test/uuid` | `SiteKey::Origin(a.test)` (from embedded URL) | No |
| Other/unknown | `foo:bar` | `SiteKey::Opaque(new)` | No |

### 2.1 Network origins (`http`, `https`)

Rule (normative):

- `SiteKey::Origin(DocumentOrigin::from_url(url))`, where `DocumentOrigin` canonicalizes:
  - host casing,
  - default ports, and
  - scheme.

### 2.2 `file:`

`file:` URLs are treated as *opaque* for process assignment so unrelated local files do not share a
renderer process.

**Rule (default; normative):**

- Parse the URL with `url::Url`.
- Derive a stable opaque site ID from the absolute file path (equivalently: from the normalized
  `file://` URL string produced by `url::Url`).
  - Same file URL ⇒ same `SiteKey`
  - Different absolute file paths ⇒ different `SiteKey`

Optional modes (for test/perf tuning; less normative):

- **Per-directory**: hash only the parent directory so `file:///tmp/a.html` and `file:///tmp/b.html`
  share a `SiteKey`, but `file:///home/user/x.html` does not.
- **Single bucket (legacy)**: all `file:` URLs share one `SiteKey` (less secure; reduces process
  churn).

Security notes / rationale:

- `file:` content often represents local secrets, and file reads should be brokered by the browser process.
- `file:` site keys must **never** share a renderer process with network `SiteKey`s (`http(s)`).
- This `SiteKey` policy is intentionally stricter than FastRender’s current `DocumentOrigin` semantics
  for `file:` URLs (see `DocumentOrigin::same_origin` in `src/resource.rs`).

### 2.3 `about:blank`

`about:blank` is special because it is frequently used as:

- The default iframe document when `src` is missing/empty.
- The initial empty document for new browsing contexts.

**Rule (normative):**

- If there is an `initiator_site` (e.g. iframe creation) **and** `force_opaque_origin == false`,
  `about:blank` **inherits** it:
  - `derive_site_key("about:blank", Some(parent_site), false) == parent_site.clone()`
- Otherwise (`initiator_site == None`), `about:blank` is treated as an opaque origin:
  - `SiteKey::Opaque(new_opaque_id())`
  - (Also applies when `force_opaque_origin == true`, e.g. sandboxed iframes without
    `allow-same-origin`.)

This matches the “creator origin” intuition: about:blank created by a document is same-origin with
that document; about:blank created by the browser UI is a fresh opaque origin.

Implementation detail (normative for matching browser behavior):

- `about:blank` with a query or fragment (e.g. `about:blank#foo`, `about:blank?x=1`) is still treated
  as `about:blank` for `SiteKey` derivation and inheritance.

### 2.4 `about:srcdoc`

`about:srcdoc` is the internal URL for iframe `srcdoc` documents.

**Rule (normative):**

- If `initiator_site` is present **and** `force_opaque_origin == false`, `about:srcdoc` inherits
  `initiator_site`.
- Otherwise, treat it as opaque (defensive; this should not happen in normal iframe creation flows
  except for sandboxed/opaque-origin iframes).

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

- `about:*` URLs other than `about:blank` and `about:srcdoc` map to a **stable opaque** site key:
  - `SiteKey::Opaque(stable_hash(about_page_id))`
  - where `about_page_id` is the case-insensitive page identifier (the `about:` path component),
    and query/fragment are ignored for site-key purposes.
- They **do not inherit** `initiator_site`.

Rationale:
- Internal pages must not share a renderer process with arbitrary web content, otherwise a renderer
  compromise in a web page could read the internal page’s in-memory state after navigating.
  - Using stable opaque keys avoids process churn for pages like `about:history?q=...` while still
    preventing co-hosting with unrelated origins.

Implementation note:
- The in-tree implementation uses a domain-separated hash of the `about_page_id` in
  `src/site_isolation/site_key.rs` so query/fragment state does not create new processes.

Repo reality note:
- The in-tree browser has a concrete set of built-in about pages defined in
  [`src/ui/about_pages.rs`](../src/ui/about_pages.rs) (`ABOUT_*` constants + `ABOUT_PAGE_URLS`).
  - See [`docs/about_pages.md`](about_pages.md) for the user-facing list and per-page expectations.

Navigation policy note:

- Deriving a stable-opaque `SiteKey` for internal pages does **not** imply that untrusted web
  content is allowed to navigate to (or embed) those pages.
  - In particular, embedding privileged internal pages in cross-origin iframes can be a UI/security
    risk even if same-origin scripting prevents direct DOM access.
- The browser process should enforce a policy such as:
  - only allow internal `about:*` navigations that are initiated by the browser UI / trusted code,
  - reject renderer-initiated navigations to internal `about:*` pages (and reject `about:*` as an
    iframe `src` unless explicitly intended).

### 2.7 Other schemes (`blob:`, `javascript:`, unknown)

Navigation policy (URL allowlists) should reject dangerous schemes like `javascript:` *before*
process assignment is considered (see `docs/multiprocess_threat_model.md` for the “treat URLs as
capabilities” rule).

For the site isolation process model, we still need an explicit fallback rule:

**Rule (normative):**

- `blob:` URLs derive their site key from the **embedded URL** (`blob:https://a.test/uuid` → same
  site key as `https://a.test/`). They do **not** inherit `initiator_site`.
  - `blob:null/...` and unparseable embedded URLs are treated as opaque.
- For schemes not covered above (`http(s)`, `file:`, `about:*`, `data:`, `blob:`):
  - treat them as a fresh opaque site: `SiteKey::Opaque(new_opaque_id())`.
  - do not attempt to “guess” an origin for schemes we don’t fully model yet. Add explicit
    handling plus tests when a new scheme is introduced.

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
- `force_opaque_origin`: true when the resulting document must have an opaque origin even if the
  URL would normally be origin-bearing (e.g. `<iframe sandbox>` without `allow-same-origin`).

Algorithm (normative):

1. `target_site = derive_site_key(commit_url, initiator_site, force_opaque_origin)`
2. If `frame.current_site == target_site` **and** the current renderer process is alive:
   - Stay in the same process (`no process swap`).
3. Else:
   - Ask `RendererProcessRegistry::get_or_spawn(&target_site)`.
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
- A cross-origin redirect (i.e. redirect where
  `derive_site_key(final_url, initiator_site, force_opaque_origin)` differs from the
  initially-requested `SiteKey`) must result in a **process swap before commit**.

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
- Else if `src` is missing: treat as `about:blank`.
  - (This matches current behavior; see `display_list_iframe_missing_src_defaults_to_about_blank`
    in `src/paint/display_list_renderer/tests/display_list/iframe.rs`.)
- Else if `src` is ASCII-whitespace-only: do not perform a navigation and keep the initial
  `about:blank` document.
- Else: resolve `src` relative to the parent document base URL.

#### 3.3.2 Same-`SiteKey` iframe (in-process)

Definition:

- “Same-`SiteKey` iframe” means
  `derive_site_key(iframe_url, Some(parent_site), iframe_force_opaque_origin) == parent_site`.
  - This includes `about:blank` and `about:srcdoc` iframes **when** `iframe_force_opaque_origin ==
    false` (inherit the parent site).
  - If the iframe is sandboxed without `allow-same-origin`, `iframe_force_opaque_origin == true`
    and even `about:blank`/`about:srcdoc` become cross-site (opaque).

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

- “Cross-`SiteKey` iframe” means
  `derive_site_key(iframe_url, Some(parent_site), iframe_force_opaque_origin) != parent_site`.
  - This includes:
    - `data:` iframes (always opaque),
    - sandboxed opaque-origin iframes (even if the URL is `about:blank`/`about:srcdoc`), and
    - regular cross-origin iframe URLs.

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

For each *embedder* frame, the browser needs enough information to:

- place each child frame surface (OOPIF) into the embedder’s coordinate space, and
- preserve **paint order** between embedder content and child frame content.

In IPC, this is represented by a *layered paint plan* (`fastrender_ipc::FramePaintPlan`), delivered
by the renderer via `RendererToBrowser::FramePaintPlan`.

Conceptual structure (normative shape):

```rust
pub struct FramePaintPlan {
    pub frame_id: FrameId,

    /// Embedder layers. Each layer must be RGBA8 premultiplied with a transparent background.
    pub layers: Vec<FrameBuffer>,

    /// Out-of-process iframe slots in stable paint order.
    pub slots: Vec<SubframeInfo>,
}
// Expected invariant: layers.len() == slots.len() + 1.
```

Composition order is:

```
layers[0] -> slots[0] -> layers[1] -> slots[1] -> ... -> layers[N]
```

Each `SubframeInfo` describes where the child frame surface should appear and how it participates
in hit testing / navigation policy:

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

    /// Whether the iframe should receive pointer events / be hit-testable.
    pub hit_testable: bool,

    /// `<iframe sandbox>` derived flags and whether the resulting origin is forced opaque.
    pub sandbox_flags: SandboxFlags,
    pub opaque_origin: bool,

    /// Summary of visual effects at the embedding point (used for OOPIF eligibility decisions).
    pub effects: SubframeEffects,
}
```

This mirrors how the single-process renderer treats iframes as “render-to-image then draw as
replaced content” (see `src/paint/iframe.rs`), but with the critical difference that the *pixels*
come from a different process and must be interleaved with embedder content in paint order.

Note: `RendererToBrowser::FrameReady { buffer, subframes }` is a legacy/simpler shape that cannot
express interleaving; `FramePaintPlan` is the normative shape for correct OOPIF compositing.

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
  - clip stacks must have a hard maximum depth (see `MAX_SUBFRAME_CLIP_STACK_DEPTH` in
    `crates/fastrender-ipc`),
  - rectangles/sizes must be clamped to reasonable limits (see §5.3),
  - the number of embedded subframes per frame must be capped (see `MAX_SUBFRAMES_PER_FRAME` in
    `crates/fastrender-ipc`, plus frame-tree limits in §5.1).
- The browser must treat embedder-provided ordering keys (`z_index`, stacking keys, etc.) as
  *relative ordering hints* only; the compositor is responsible for enforcing correct paint order
  rules and preventing pathological ordering values from causing resource abuse.

Rationale:
- Without validation, a compromised renderer could attempt to embed a frame it does not own (UI
  spoofing / confusion) or provide degenerate geometry that DoS’es the compositor.

### 4.3 Current compositor model: layered paint plans (stacking-order correct)

FastRender’s site isolation compositor preserves stacking/paint order by using `FramePaintPlan`
layers:

- The embedder renderer splits its output into `N+1` layers around `N` OOPIF slots.
- The browser composites in the plan’s order, treating missing child buffers as transparent (skip).

Remaining limitations (current code; important for contributors):

- The browser compositor/hit tester currently supports only a limited subset of effects for OOPIF:
  - axis-aligned affine transforms (translate/scale),
  - axis-aligned rectangular/rounded clipping (via `clip_stack`),
  - stable z-order keys for deterministic ordering.
- If an embedding point has unsupported effects (opacity groups, blend modes, filters/masks,
  non-axis-aligned transforms), the embedder should conservatively fall back to **inline iframe**
  rendering and avoid emitting an OOPIF slot. The renderer communicates this via `SubframeEffects`.

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

Site key derivation and iframe isolation decisions should be unit-tested close to the helper code:

- `src/site_isolation/site_key.rs` (`site_key_for_navigation` / `SiteKeyFactory`)
- `src/site_isolation/policy.rs` (`should_isolate_child_frame*`)
- `crates/fastrender-ipc` (`SiteLock::matches_url`, `FrameHitTester`, `composite_paint_plan`, etc.)

Minimum coverage for `derive_site_key`/`site_key_for_navigation`:

- `https://a.test/` vs `https://a.test:443/` normalize to the same `SiteKey`.
- `about:blank` / `about:srcdoc` inherit initiator site when `force_opaque_origin == false`.
- `about:blank` / `about:srcdoc` become fresh `Opaque` when `force_opaque_origin == true`
  (sandboxed opaque-origin iframes).
- Internal `about:*` pages map to **stable opaque** keys by page id (query/fragment ignored).
- `data:` always produces fresh `Opaque` and never inherits.
- `blob:https://a.test/...` derives `SiteKey` from the embedded URL (does not inherit parent site).

Notes:
- Follow the repo’s test organization rules in [`docs/test_architecture.md`](test_architecture.md):
  pure helpers should be unit-tested in `src/`, and integration/process tests should live in the
  existing integration harness (avoid introducing new `tests/*.rs` binaries).

Add unit tests for process assignment decisions:

- Same-`SiteKey` navigation keeps process.
- Cross-`SiteKey` navigation swaps process.
- Two frames with the same `SiteKey` reuse the same process (registry reuse).

### 6.2 Integration tests (multiprocess harness)

FastRender already has a multiprocess integration harness. Prefer extending existing tests over
inventing new ad-hoc binaries.

Where to add integration coverage:

- Renderer↔browser IPC/OOPIF semantics: `crates/fastrender-renderer/tests/oopif_*.rs`
- Browser-side process model: `tests/multiprocess_registry.rs` and `tests/multiprocess/*`
- Crash containment: `tests/iframe/crash_isolation.rs`
- Sandbox/opaque-origin iframe cases: `tests/site_isolation_sandbox_iframe.rs`

Must-have cases:

- Top-level `https://a` with cross-origin iframe `https://b` → different renderer processes.
- Same-`SiteKey` iframe `https://a` inside `https://a` → same renderer process.
- `srcdoc` / missing-`src` (`about:blank`) iframes inherit parent site unless forced opaque by sandbox.
- `data:` iframe gets its own `Opaque` site and therefore a separate process.

### 6.3 Compositing tests

Add pixel-level tests that confirm the compositor correctly embeds subframe pixels with clipping:

- Basic rectangle positioning.
- Rounded-corner clipping / overflow clipping for the iframe content box.
- Stacking/paint-order interleaving (embedder layers above/below child surfaces).

Existing coverage to build on:

- `crates/fastrender-ipc` unit tests for `composite_paint_plan` and hit testing.
- `src/paint/tests/paint/remote_iframe_stacking_order.rs` for paint-order correctness.

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
