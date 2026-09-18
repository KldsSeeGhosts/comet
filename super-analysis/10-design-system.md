# 10 — Design System

Derived from live screenshots + settings.json + themes docs. The look is
"Zed one-dark derived": near-black layered surfaces, hairline borders, one
accent at a time, generous radius on floating chrome, serif accents in empty
states.

## Color

- Dark monochrome surface stack (page → panel → card), white-alpha hairlines,
  low-alpha hover washes. Focus ring: soft blue (visible on focused pane and
  composer underline, `10-provider-picker.png`).
- Provider brand colors as functional accents: Codex blue/violet, Pi amber,
  Claude coral/sunburst, OpenCode white-on-dark, Oh My Pi violet "π",
  Antigravity blue/orange. Provider glyphs are small rounded logos (SVG).
- **Per-workspace theme**: three continuous HSL roles — **Chrome** (sidebar,
  panel borders, frame), **Accent** (tabs, links, ⌘-key affordances, active
  workspace marker), **Highlight** (selection/emphasis) — each stored for
  light and dark; plus **background transparency** and **tint intensity**
  sliders (glass-like chrome). `ui_state.color: "210"` = project hue chip.
- Terminal palette independent of app theme (`terminal_color_scheme`).
- Semantic status colors for states (working/awaiting/ok/errored) in dots,
  tab badges, Checks panel; PR state colors (draft/merged/closed) in sidebar
  (`icons/pull-request-*.svg`).
- Light + dark themes both maintained (`theme_mode: system`).

## Typography

| Use | Face | Size |
|---|---|---|
| UI body/labels | system-ish sans (SF/GPUI default) | ~12–13px @1x |
| Code, diffs, inline code, transcripts | **Lilex** (`mono_font_family`) | 12px / 16px line |
| Terminal | **Lilex Nerd Font** | independent scale |
| Empty-state taglines ("Hooray, Ermin!") | **serif display** | ~28px |
| Serif tagline + hairline divider is the app's signature empty-state move | | |

Two installable font slots (mono + terminal) in Settings → Appearance;
per-surface scales (chat / editor+diff / terminal / right-panel terminal)
and app zoom (`ui_scale`).

## Metrics & shape (from default settings.json)

- Left sidebar width **260**, right panel width **260**; both auto-hide by
  default with edge-peek.
- Vertical tab rail mode width **196**; workspace tabs default **top**.
- Right-panel stacked layout weights: stacked shell **0.4**, bottom terminal
  **0.35**.
- Window default: fill display minus menu bar; remembers per-display bounds.
- Corner radii: floating menus/popovers ~10–12px; composer input ~10px with
  1px hairline + focus glow; panes are flat rectangles separated by ~4–6px
  gutters (see `00-initial-window.png`); PiP has heavy rounded corners +
  glass bars.
- Iconography: single-weight line SVGs (`assets: icons/close.svg`,
  `chevron-down.svg`, `external-link.svg`, `pull-request[-draft|-merged|
  -filled].svg`, `git-branch.svg`); provider logos are full-color marks.

## Motion

- GPUI FLIP-style resort animations for lists; shimmer (`ShimmerView`) for
  loading rows; `LoadingIndicatorView` (spinner style selectable: spinner /
  others) app-wide; `number_flow` animated number transitions (cost pill,
  counters); `glitch_text` (AI title generation flourish); streaming-edge
  highlight fade; composer ember gradient on send button; heartbeat-driven
  pulses (`animation_heartbeat`). Hover reveals: sidebar row actions, pane
  header controls, window-edge sidebar peek with backdrop fade.

## Voice & microcopy

Playful serif one-liners on empty chats; everywhere else terse utilitarian
labels ("Queue is clear", "No run script configured", "Click to focus chat").
