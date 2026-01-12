# Workstream: JavaScript DOM Bindings

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

---

This workstream owns the **DOM API surface exposed to JavaScript**: Document, Element, Node, events, and related APIs.

## The job

Expose a **complete, spec-compliant DOM API** to JavaScript. Scripts should be able to query elements, modify the DOM, handle events, and manipulate styles—just like in a real browser.

## What counts

A change counts if it lands at least one of:

- **API coverage**: A missing DOM API is implemented.
- **Spec compliance**: An API now matches WHATWG DOM/HTML spec.
- **Bug fix**: A DOM API that returned wrong results now works.
- **WPT progress**: More WPT DOM tests pass.

## Scope

### Owned by this workstream

**Core DOM (WHATWG DOM Standard):**
- `Node`: nodeType, nodeName, parentNode, childNodes, appendChild, removeChild, insertBefore, cloneNode, textContent
- `Element`: tagName, id, className, classList, getAttribute/setAttribute/removeAttribute, querySelector/querySelectorAll, innerHTML, outerHTML, children, append, prepend, remove
- `Document`: documentElement, head, body, getElementById, getElementsByClassName, getElementsByTagName, createElement, createTextNode, createDocumentFragment
- `CharacterData`, `Text`, `Comment`
- `Attr`, `NamedNodeMap`
- `NodeList`, `HTMLCollection`
- `DOMTokenList` (classList)

**HTML Elements (WHATWG HTML Standard):**
- `HTMLElement`: hidden, title, lang, dir, style
- Form elements: `HTMLInputElement`, `HTMLTextAreaElement`, `HTMLSelectElement`, `HTMLFormElement`
- Media elements: `HTMLImageElement`, `HTMLVideoElement`, `HTMLAudioElement`
- Link elements: `HTMLAnchorElement`, `HTMLLinkElement`
- Other: `HTMLDivElement`, `HTMLSpanElement`, `HTMLParagraphElement`, etc.

**Events (WHATWG DOM Standard):**
- `Event`: type, target, currentTarget, eventPhase, bubbles, cancelable, defaultPrevented, stopPropagation, preventDefault
- `EventTarget`: addEventListener, removeEventListener, dispatchEvent
- Specific events: `MouseEvent`, `KeyboardEvent`, `FocusEvent`, `InputEvent`, `UIEvent`
- Event bubbling and capturing

**Styles:**
- `element.style` (CSSStyleDeclaration)
- `getComputedStyle(element)`
- `element.getBoundingClientRect()`
- `element.offsetWidth/Height/Left/Top`

**Window (partial — see js_web_apis.md for more):**
- `document` property
- `getComputedStyle()`
- `getSelection()`

### NOT owned (see other workstreams)

- JavaScript language features → `js_engine.md`
- Web APIs (fetch, URL, timers) → `js_web_apis.md`
- Script loading and execution → `js_html_integration.md`

## Priority order (P0 → P1 → P2)

### P0: Query and read (scripts can find elements)

1. **Document queries**
   - `document.getElementById()`
   - `document.getElementsByClassName()`
   - `document.getElementsByTagName()`
   - `document.querySelector()`
   - `document.querySelectorAll()`

2. **Element properties (read)**
   - `element.id`, `element.className`, `element.classList`
   - `element.getAttribute()`
   - `element.tagName`, `element.nodeName`
   - `element.parentNode`, `element.children`, `element.childNodes`
   - `element.textContent`, `element.innerHTML` (getter)

3. **Node traversal**
   - `parentNode`, `parentElement`
   - `childNodes`, `children`, `firstChild`, `lastChild`
   - `nextSibling`, `previousSibling`, `nextElementSibling`, `previousElementSibling`

### P1: Modify DOM (scripts can change the page)

4. **Element properties (write)**
   - `element.id = ...`, `element.className = ...`
   - `element.setAttribute()`, `element.removeAttribute()`
   - `element.classList.add()`, `.remove()`, `.toggle()`, `.contains()`
   - `element.textContent = ...`, `element.innerHTML = ...`

5. **DOM mutation**
   - `document.createElement()`, `document.createTextNode()`
   - `element.appendChild()`, `element.insertBefore()`, `element.removeChild()`
   - `element.append()`, `element.prepend()`, `element.remove()`
   - `element.replaceChild()`, `element.replaceWith()`
   - `element.cloneNode()`

6. **Form element access**
   - `input.value`, `input.checked`, `input.disabled`
   - `select.value`, `select.selectedIndex`, `select.options`
   - `textarea.value`
   - `form.elements`, `form.submit()`, `form.reset()`

### P2: Events (scripts can respond to user actions)

7. **Event listeners**
   - `addEventListener(type, handler, options)`
   - `removeEventListener(type, handler, options)`
   - Event bubbling (capture, target, bubble phases)
   - `stopPropagation()`, `preventDefault()`

8. **Event dispatch**
   - `dispatchEvent(new Event(...))`
   - Custom events
   - Synthetic events

9. **Common events**
   - `click`, `mousedown`, `mouseup`, `mousemove`, `mouseenter`, `mouseleave`
   - `keydown`, `keyup`, `keypress`
   - `focus`, `blur`, `focusin`, `focusout`
   - `input`, `change`, `submit`
   - `load`, `DOMContentLoaded`

### P3: Layout and styles

10. **Computed styles**
    - `getComputedStyle(element)`
    - `element.style` (inline styles)
    - `element.style.setProperty()`, `.getPropertyValue()`

11. **Layout information**
    - `element.getBoundingClientRect()`
    - `element.offsetWidth`, `element.offsetHeight`
    - `element.offsetLeft`, `element.offsetTop`
    - `element.clientWidth`, `element.clientHeight`
    - `element.scrollWidth`, `element.scrollHeight`
    - `element.scrollTop`, `element.scrollLeft`

12. **Mutation observers**
    - `MutationObserver`
    - `IntersectionObserver`
    - `ResizeObserver`

## Implementation notes

### Architecture

```
src/js/webidl/           — WebIDL binding infrastructure
  bindings/              — Generated and hand-written bindings
    generated/mod.rs     — Auto-generated from WebIDL
    document.rs          — Document bindings
    host.rs              — Host function dispatch
  conversions.rs         — Type conversions
  
src/dom2/                — Mutable DOM implementation
  mod.rs                 — Document, Node, Element types
  traversal.rs           — Tree traversal
  mutation.rs            — DOM mutation
  
crates/js-dom-bindings/  — DOM binding helpers
crates/webidl-vm-js/     — WebIDL runtime for vm-js
```

### WebIDL workflow

DOM APIs are defined in WebIDL. The binding generator extracts shapes from specs and generates Rust glue.

```bash
# Update WebIDL snapshot
timeout -k 10 300 bash scripts/cargo_agent.sh xtask webidl

# Regenerate bindings
timeout -k 10 300 bash scripts/cargo_agent.sh xtask webidl-bindings
```

See `docs/webidl_bindings.md` for details.

### Testing

```bash
# Run DOM binding tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::webidl

# Run WPT DOM tests
timeout -k 10 600 bash scripts/cargo_agent.sh xtask js wpt-dom
```

### Key invariants

- **Mutations trigger invalidation**: DOM changes must mark style/layout dirty
- **GC safety**: JavaScript references to DOM nodes must be properly rooted
- **Spec compliance**: Behavior matches WHATWG specs, not browser quirks

## WPT as oracle

WPT (Web Platform Tests) DOM tests are the conformance target.

```bash
# Run curated WPT DOM suite
timeout -k 10 600 bash scripts/cargo_agent.sh xtask js wpt-dom

# Run specific test
timeout -k 10 300 bash scripts/cargo_agent.sh xtask js wpt-dom --filter "querySelector"
```

## Success criteria

DOM bindings are **done** when:
- All P0/P1/P2 APIs are implemented and spec-compliant
- WPT DOM test pass rate exceeds 90%
- Real-world scripts using jQuery, React, Vue can manipulate the DOM
- No crashes from valid DOM operations
- Mutations correctly trigger re-render
