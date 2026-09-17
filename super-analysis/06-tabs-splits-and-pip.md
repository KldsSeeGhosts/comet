# 06 — Tabs, Splits & PiP

## Container hierarchy (exactly three levels + PiP)

```
Workspace ── Projects ── Worktree ── Tab strip ── Tab ── split tree ── Pane(s)
                                                       (full split views = extra
                                                        top-level columns/rows
                                                        each with a tab strip)
```

- **Pane split** (⌘D right / ⌘⇧D down): splits *inside one tab*.
- **View split** (⌥⌘D right / ⌥⌘⇧D down): splits the content area into a new
  independent column/row with its own tab bar. Close: ⌥⌘W.
- Docs guidance: View = "top-level work areas side by side"; Tab = "several
  full sessions collected in one view"; Pane = "closely related sessions
  visible within one tab".

## Serialization format (verbatim from `reference/session.json`)

Each worktree persists `tabs[]` + `active_tab` + `split_layouts[]`:

```json
{
  "tabs": [
    { "tab_uuid": "522129b3-…", "kind": "api-chat", "provider_key": "codex",
      "provider_profile_selection": {"mode": "cli_default"},
      "preferred_model_id": "devin/swe-2",
      "session_id": "codex-17896269968694",
      "conversation_id": "conv:codex:codex-17896269968694",
      "messages_snapshot": [], "thinking_enabled": true,
      "permission_mode": "bypass" },
    { "kind": "terminal", "preset_key": "terminal",
      "working_directory_path": "/…/open-design-project", "title": "Terminal" }
  ],
  "active_tab": 0,
  "split_layouts": [
    { "root": {
        "kind": "split", "axis": "horizontal", "ratio": 0.5,
        "first":  { "kind": "split", "axis": "vertical", "ratio": 0.5,
                    "first":  {"kind":"leaf","leaf":{"pane_id":4,
                               "content":{"kind":"primary-tab"}}},
                    "second": {"kind":"leaf","leaf":{"pane_id":7,
                               "content":{"kind":"tab","tab":{…full tab object…
                               "title":"Mac Storage Cleanup",
                               "title_sc_owned":true,
                               "messages_snapshot":[…]}}}} },
        "second": { …leaf… } } }
  ]
}
```

Key facts to clone:
- `content.kind = "primary-tab"` means "the tab that owns this tab-strip"
  (it renders whichever `tabs[active_tab]` is) — one pane is special.
- Ratios are floats per split node; axes `horizontal|vertical`; leaves have
  stable `pane_id`s referenced elsewhere (right panel resize weights).
- `messages_snapshot` keeps recent transcript lines inside the layout file so
  restored panes show content instantly.
- Pi session ids show reattachment to *existing provider session files* by
  path.

## Interactions

- Drag a tab to a pane edge → split preview; center drop → move; outer
  **drop ring** → full view split. Drag pane headers to re-dock as tabs.
- Double-click divider → equalize ratios. ⌥⌘W closes the focused view.
- Pane header context menu (`07-pane-context-menu.png`): Split pane right ⌘D,
  Split pane down ⌘⇧D, Split view right ⌥⌘D, Split view down ⌥⌘⇧D,
  **Tab layout ▸** (arrangement presets; also expose even "Tile" presets
  matching Window ▸ Move & Resize semantics).
- New pane after split opens the **tool picker dropdown** (same list as
  launcher) — verified live.
- Empty view (after closing last tab) shows launcher cards + Recent.
- Layouts restore with the worktree; widths/weights persist
  (`stacked_shell_weight`, `bottom_terminal_weight`).

## PiP (⌘⇧P)

- `Toggle Picture-in-Picture` moves the active compatible tab into a floating
  mini-window; toggling again restores it (`17-pip-window.png`).
- PiP renders the tab's full split tree scaled; has pin (green traffic light /
  pin fab), expand, close; captures source workspace/tab at open time.
- Elements: `pip-pin-fab`, `pip-tab-bar`, `pip-open-fab-group`,
  `pip-open-picker-anchor/overlay/fab`, `pip-top-glass-bar`,
  `pip-bottom-glass-bar`, `workspace::OpenInApp`.
- Pinning the app window (window controls menu) is independent of PiP pin.

## `sc` CLI equivalents (also the cleanest spec of split semantics)

```
sc tab split --direction up|down|left|right [--active new|keep]
sc tab split-view --direction up|down|left|right [--active new|keep]
sc layout set --target workspace|tab --count N --arrangement grid|vertical|horizontal|SPEC
sc layout insert/close/move   # view:N / tab:N / pane:N addressing
sc layout save|apply|list NAME --scope user|worktree
sc layout views|state|capabilities
```
(`reference/sc-instructions-layout.txt` has the agent-facing semantics.)
