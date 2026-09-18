# 13 — Interaction truth pass (live computer-use capture)

Live verification against the installed nightly (`com.zarifpour.superconductor`,
pid observed on this machine), supplementing the screenshot-based docs.
Captured 2026-09-17. Layout was restored to its original state after the pass.
These findings gate WS4 (drag/drop) and WS6 (chrome) details of
`docs/plans/2026-09-17-split-pane-workspace.md`.

## 1. Divider behavior — VERIFIED

- Drag anywhere along a split divider resizes **continuously** (no ghost
  needed); moving the root vertical divider resizes the whole column set
  (all rows of the narrowed side shrink together — the divider belongs to
  its split node, not to individual panes).
- **Double-click a divider → equalize that split node to 0.5/0.5** (verified:
  dragged root divider from ~0.49 to ~0.39, double-click sprang it back).
- Divider hit zone engages at the divider line; visual gap between panes is
  ~2–4 px (logical) with ~6–8 px rounded pane corners. Recommend an 8 px
  logical hit area straddling the divider for the port.

## 2. ⌘D / tool-picker contract — REFINED vs doc 06

- ⌘D on a focused pane opens the **tool-picker dropdown anchored to the
  focused pane's top-left**; the split does **not** visibly materialize first.
  **Escape cancels with zero layout change.** The pane split + new pane
  appear when a tool is selected. (Doc 06 said "new pane after split opens
  the tool picker" — observable contract is: picker first, split on pick;
  Esc = clean cancel. Implement the observable contract.)
- Picker anatomy: rows Super ⌘1, Codex ⌘2, Claude Code ⌘3, OpenCode ⌘4,
  Pi ⌘5, Oh My Pi ⌘6, Grok ⌘7, Cursor ⌘8, Antigravity ⌘9, then Terminal
  (badge T); footer "Hold ⌘ for Terminal" + gear.
- Number keys select directly; same list serves as the tab-strip launcher.

## 3. Drag-to-split previews — VERIFIED (the key dynamic truth)

Held-drag of a tab chip over the content area, three zones observed:

| Cursor zone | Preview rendered |
|---|---|
| Pane center | None visible (drop = move tab into that pane's tab group; no indicator at this sample) |
| **Interior edge** of a pane (within outer ~20%) | **Half-pane highlight**: the half of the target pane adjacent to the hovered edge gets a blue-violet wash + 1px rounded ring → pane-level split inside the active tab |
| **Workspace-outer edge** of a pane (outer ~20% of the boundary pane) | **Full-column/full-row highlight**: the entire adjacent top-level region washes + rings → view split (new top-level column/row with own tab strip) |

- Zone width matches the engine's `edge_zone` outer-20% rule — keep it.
- Wash ≈ accent at ~30–40% alpha, 1px accent border, ~8px corner radius.
- A floating drag ghost of the tab (icon + title chip) trails the cursor;
  with synthetic events its position lags — real-pointer behavior is smooth.
- Drop back on the tab strip = reorder/restore (used as drag cancel; layout
  returned intact).

## 4. Split view right — VERIFIED anatomy

- Creates a **second top-level column with its own tab strip**; the new view
  contains one new default-provider chat tab (live composer, focused).
- **Single-pane tabs render NO pane header** — pane headers (provider icon +
  title + hover controls) appear only when a tab has ≥2 panes. Port must
  follow this rule (header chrome is conditional).
- Original panes compress to half width; focus moves to the new view's pane
  (previous focused pane's composer becomes ghost).

## 5. Empty view launcher — VERIFIED (full anatomy)

Closing a view's last tab leaves the view open showing the launcher:

- Brand title ("super.engineering") + tagline "Choose a tool to get started".
- Featured default-tool card (icon, name, model id, pop-out affordance).
- "All tools" grid: the ⌘1–9 provider cards + Browser (no badge) +
  Terminal (badge T), icon-over-label cards.
- "Recent" list: provider icon + session title + relative time (2h/10h/1d).
- Footer: "Hold ⌘ for Terminal" (left) + "⚙ Configure" (right).
- The empty view's tab-strip area shows a **× (close view)** + new-tab
  split controls; clicking × closes the view and the remaining view expands
  to full width (ratios re-normalized).

## 6. Pane header chrome — VERIFIED

- Header: status-dot + title left; hover reveals **pop-out ⧉, maximize □,
  close ×** right-aligned.
- Right-click header → context menu (light & dark verified):
  Split pane right ⌘D · Split pane down ⌘⇧D · Split view right ⌥⌘D ·
  Split view down ⌥⌘⇧D · Tab layout ▸ (submenu; presets per doc 06).
- Right-click also focuses the pane (composer returns from ghost).
- Focused pane ring: 1px blue-violet border around the pane (clear in dark
  theme, subtle gray-blue in light); unfocused panes plain hairline.

## 7. Ghost composers — VERIFIED

Unfocused chat panes render a static composer-shaped strip: "Click to focus
chat" placeholder text, muted provider/model pills row beneath (same pill
layout as the live composer: provider/model id, effort badge, tool toggles,
+ and ↑ buttons), all non-interactive until the pane is focused. Click
anywhere in the pane focuses it and swaps in the live composer.

## 8. Misc confirmed facts

- Single tab strip per view spans the content width; active tab has colored
  underline + provider avatar; `+` and split-menu chevron at strip end.
- Bottom status bar persists below the content area (splits never overlap it).
- Theme-independent layout: all of the above verified in both dark (doc
  screenshots) and light (live) themes.

## Port implications (quick list for WS2/WS4/WS6)

1. Tool picker is the ⌘D response; split commits on selection (Esc-safe).
2. Drag previews: half-pane (interior edge) vs full-region (outer edge);
   20% edge zone; accent wash + ring; center = move, no indicator.
3. Pane header only when tab has ≥2 panes.
4. Empty view = launcher state, distinct close control for the view.
5. Divider: continuous drag on node ratio; double-click equalizes node.
6. Focus: click/right-click focuses pane; ghost composer swaps to live.
