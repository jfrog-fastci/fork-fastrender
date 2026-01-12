# Workstream: Browser UX & Visual Design

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

This workstream owns the **visual design, polish, and user experience** of the browser application.

## The job

Make the browser **beautiful, responsive, and delightful to use**. Users should feel like they're using a modern, polished application—not a developer tool or prototype.

## What counts

A change counts if it lands at least one of:

- **Visual improvement**: UI looks more modern, consistent, or polished.
- **Responsiveness improvement**: Interactions feel faster or smoother.
- **Loading experience**: Users understand what's happening during slow operations.
- **Accessibility**: UI is usable with screen readers, keyboard-only, high contrast, etc.
- **Platform integration**: Browser feels native on each OS (macOS, Windows, Linux).

## Scope

### Owned by this workstream

- **Visual design language**: Colors, typography, spacing, iconography
- **Theming**: Light mode, dark mode, system theme following, accent colors
- **Loading states**: Progress indicators, skeleton screens, spinners
- **Animations & transitions**: Tab switches, panel open/close, hover effects
- **Responsive layout**: Chrome adapts to window size (compact mode, overflow handling)
- **Error states**: Error pages, connection errors, certificate warnings
- **Empty states**: New tab page, no results, empty history
- **Accessibility**: Keyboard navigation, screen reader support, high contrast
- **Platform-native feel**: macOS traffic lights, Windows title bar, Linux integration

### NOT owned (see other workstreams)

- Chrome functionality (tabs, navigation) → `browser_chrome.md`
- Page interaction (forms, focus) → `browser_interaction.md`
- Page rendering quality → `capability_buildout.md`

## Design principles

### 1. **Modern & clean**
- Flat design with subtle depth (shadows, layers)
- Generous whitespace
- Clear visual hierarchy
- High-quality iconography (consider SF Symbols on macOS, Fluent on Windows)

### 2. **Fast & responsive**
- Interactions respond in <16ms (60fps)
- Window resize is smooth, not janky
- Loading indicators appear within 100ms of action
- No "frozen" states—always show progress or allow cancel

### 3. **Informative, not intrusive**
- Loading state is visible but not distracting
- Errors are clear and actionable
- Progress shows meaningful information (not just spinner)
- Warnings don't block workflow unnecessarily

### 4. **Accessible by default**
- All interactive elements keyboard-focusable
- Color is not the only indicator
- Text is readable (contrast, size)
- Screen reader announcements for state changes

## Priority order (P0 → P1 → P2)

### P0: Fundamental UX (stop looking broken)

1. **Loading indicators**
   - Tab loading spinner
   - Address bar progress indicator (like Safari/Chrome blue line)
   - Page loading skeleton or blank state that's clearly "loading"
   - Network error states with retry button

2. **Responsive resize**
   - Window resize must be smooth (<16ms frame time)
   - Tab bar overflow handling when window narrows
   - Address bar truncation for long URLs
   - Toolbar compact mode for small windows

3. **Basic visual cleanup**
   - Consistent spacing and alignment
   - Readable typography (proper font, size, weight)
   - Clear visual separation between chrome and content
   - Sensible default colors (not garish, not invisible)

### P1: Polish (looks good)

4. **Theming**
   - Light theme (default, clean)
   - Dark theme (follows system preference)
   - Proper contrast in both themes
   - Consistent color palette

5. **Iconography**
   - Navigation icons (back, forward, reload, stop, home)
   - Tab icons (close, new tab, loading spinner)
   - Address bar icons (secure, insecure, bookmark)
   - Consider platform-native icon sets

6. **Micro-interactions**
   - Button hover/active states
   - Tab hover effects
   - Smooth tab close animation
   - Address bar focus animation

7. **Typography**
   - System font stack (SF Pro on macOS, Segoe on Windows, etc.)
   - Proper hierarchy (URL vs page title vs UI labels)
   - Monospace for URLs/code when appropriate

### P2: Delight (feels premium)

8. **Animations**
   - Tab open/close animations
   - Panel slide in/out
   - Page transition effects (optional, tasteful)

9. **New tab page**
   - Clean design
   - Frequently visited sites
   - Search bar integration
   - Customizable background (optional)

10. **Error pages**
    - Friendly, helpful error messages
    - Suggestions for common issues
    - Retry and diagnostic actions

11. **Advanced accessibility**
    - Reduced motion preference
    - High contrast mode
    - Font size scaling
    - Screen reader optimization

## Implementation notes

### Technology stack

- **egui 0.23** for UI widgets
- **wgpu 0.17** for rendering
- **tiny-skia** for page content (not chrome)

### Key files

```
src/bin/browser.rs      — Main UI rendering
src/ui/chrome.rs        — Chrome widget helpers
src/ui/browser_app.rs   — State management
```

### egui styling

egui can be styled via `egui::Style` and `egui::Visuals`. Create a consistent style definition:

```rust
fn apply_browser_theme(ctx: &egui::Context, dark_mode: bool) {
    let mut style = (*ctx.style()).clone();
    // ... customize colors, spacing, fonts
    ctx.set_style(style);
}
```

### Performance considerations

- Avoid layout recalculation every frame
- Cache rendered text
- Use `egui::Area` for floating elements, not re-rendering everything
- Profile with `FASTR_PERF_LOG=1`

## Metrics

Track these to measure progress:

- **Frame time during resize**: Should be <16ms
- **Time to first pixel after navigation**: Should show loading state <100ms
- **Visual consistency score**: Manual audit checklist

## Success criteria

The browser UX is **done** when:
- Users describe the browser as "clean" or "modern" (not "ugly" or "prototype")
- Window resize is smooth on all platforms
- Loading states are clear and informative
- Both light and dark themes look polished
- No jarring visual inconsistencies
