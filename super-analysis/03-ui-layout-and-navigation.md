# 03 — UI Layout & Navigation

All regions visible in `screenshots/00-initial-window.png` and
`18-workspace-switcher.png`. Window: borderless-looking dark chrome, hidden
titlebar integrated with app UI; traffic lights top-left; "zoom" button has a
context menu. The window fills the display minus menu bar when zoomed.

## 1. Title bar strip (top, ~56px)

Left → right:
1. Traffic lights (close/minimize/zoom; long-press green = window controls
   incl. **Pin Window** — keeps app above other windows across Spaces).
2. Sidebar toggle icon, Search icon (opens palette, ⌘K), back/forward chevrons
   (navigation history of worktrees/views).
3. Center-left status pill: `◌ Queue is clear` — global agent queue state.
4. Right-side pill group (see `12-new-worktree-modal.png`):
   - `$0.00` conversation cost pill (per worktree scope; `show_conversation_cost`)
   - history/restore icon, **lightning** (Automations rail),
     external-link (Open In App), and a green **Run ▷** button with chevron
     (run scripts / action flights).
5. Far right: notification-bell / clock icon (session history popover).

## 2. Left sidebar (default 260px, `left_sidebar_width_design`)

- Top: **workspace name** ("Default") — the workspace switcher label.
- `Projects` header + project rows: icon chip (project color hue, e.g. "210"),
  name, chevron; `+` to add a project.
- **Worktree rows** under each project (two layouts, `left_sidebar_layout`):
  - *Detailed* (default): branch icon + label (branch name or AI feature
    label) + relative time ("44m ago"); primary worktree has a **★**.
  - *Compact*: feature label only; hover reveals branch, change counts (+/−),
    PR/MR context, current git action.
  - Hover shows quick actions: Run ▷, terminal, Finder, copy path
    (`13-worktree-context-menu.png`).
  - Unnamed new worktrees show italic placeholder "*my new worktree*".
- Rows can live in **sections** (manual "Pinned" + filter sections driven by
  PR state / checks / agent state / branch glob; see doc 08).
- Auto-hide mode (`auto_hide_sidebars: true`): sidebar slides away, pointer at
  screen edge reveals it (`SidebarPeekBackdrop`, `horizontal_reveal`).
- Bottom strip (see zoom in `screenshots`): gear (Settings, ⌘,), folder+ (add
  project), a blue status dot, extra pane icons, `+`.

## 3. Tab bar (per worktree, top of content area)

- Tabs belong to the **worktree**; switching worktree swaps the whole tab set
  (verified live). Placement `top` (setting; also a vertical rail mode with
  `workspace_vertical_tab_rail_width: 196`).
- Tab content: provider avatar stack (up to ~3 + "+1"), title. Title is
  AI-generated from the first prompt (`title_sc_owned: true`), or the provider
  name when unfocused ("Codex", "Pi"), or "Terminal" with a terminal icon.
  Multi-pane tabs show a stacked-panes glyph. Activity state shows in the tab.
- `+` button (new default-provider chat) with a **⌄** chevron opening the
  **launcher dropdown** (`04-tab-launcher-dropdown.png`):
  `1 Super · 2 Codex · 3 Claude Code · 4 OpenCode · 5 Pi · 6 Oh My Pi ·
  7 Grok · 8 Cursor · 9 Antigravity · Browser · Terminal` + footer hint
  "Hold ⌘ for Terminal" + gear. Pinned-tab styles: compact icon / full width.
- Closing the last tab leaves an **empty view launcher**: tool cards (default
  agent, another provider, terminal, browser) + Recent list.

## 4. Content area — panes, views, splits

- A tab contains a **split tree of panes**; each pane has a header row:
  provider icon + name ("Codex", "Pi", "Mac Storage Cleanup", "Terminal"),
  hover controls (pop-out, maximize, close ×).
- The **focused pane** has a blue focus ring; its chat shows the composer.
  Unfocused chat panes show "Click to focus chat" ghost composers (see
  `00-initial-window.png`).
- **Split views** split the whole content area into independent columns/rows
  (each with its own tab strip) — distinct from pane splits inside one tab.
- Divider interactions: drag to resize; double-click to equalize; drag tabs
  onto edges to create splits (drop ring = full-height/width view split);
  pane headers drag as tabs elsewhere.

## 5. Right panel (260px, ⌘E) — `09-right-panel.png`

- Tabs: **Files · Changes 0 · Review 0 · Checks** (badge counts).
- Files: worktree file tree (`FileTree`, copy-path menus).
- Changes: changed-file list with +/− counts; git status attached to the
  worktree even when collapsed.
- Review: review threads (`RemoteThreadCard`s).
- Checks: CI checks panel.
- Bottom cluster: `Commits | All` filter pills, eye/mode toggles, layout
  switcher (≡), then `Setup | Run | Terminal | +` row (worktree tools:
  setup scripts, run scripts, panel terminal ⌃`), and empty state
  "No run script configured — Configure a run script in Project Settings or
  add a `superconductor/config.json` file" with a **Configure run script**
  button. Layout modes: `stacked` (default) vs bottom-terminal docked
  (`stacked_shell_weight 0.4`, `bottom_terminal_weight 0.35`).

## 6. Bottom bar

- Center: adaptive **git action bar** for the active worktree — label morphs
  commit → review → merge → cleanup ("main" with branch icon in captures).
  Hide via context menu; edge-reveal when hidden (`center_bottom_bar_visible`).
- On new worktrees it shows "Branch will be named automatically, or click here
  to manually name" (`12-new-worktree-modal.png`).

## 7. Search / command palette (⌘K or title-bar search) — `11-search-palette.png`

- Input "Search for anything…", scope chip **Global** (⌃G toggles
  workspace-scoped vs global).
- Sections: worktrees (name + "Default · open-design-project" breadcrumb),
  tabs/chats per worktree (provider icon + title), Terminal entries,
  then commands: "Activate Navigation Slot 1/2" (⌘1/⌘2 … slots 1–9 pin
  favorite destinations).
- Footer action bar: context `main` · **⌘⏎ Switch** · **⏎ Open** · **→ Filter**
  · **Esc Close**.
- Palette content is config'd by `palette.show_{files,worktrees,tabs,commands,
  conversations,breadcrumbs}`.

## 8. Settings surface (⌘, or gear) — `14-settings.png`, `15-…`, `19-…`

Full-window page (Esc = "Back to app"), left submenu with search:
- **General** (Language / Updates / Behavior / Open In Apps / Storage /
  Diagnostics): display language; update channel "Nightly" + release notes;
  behavior toggles — Enhance prompt, Show Cmd+number hints on Cmd hold,
  Use Cmd+number to switch tabs, Confirm before quitting, Confirm before
  closing a running tab, Open links in the in-app browser.
- **Appearance** (Theme / Layout / Typography): System/Light/Dark; workspace
  theme customizer (chrome/accent/highlight HSL + transparency + tint);
  default theme; app icon style; loading indicator style; workspace-tab
  placement; pinned-tab style; left sidebar layout Compact↔Detailed with live
  preview cards; sidebar label = description or branch name; per-surface text
  scales (chat / editor+diff / terminal tab / right-sidebar terminal).
- **Terminal**: shell, font (Lilex Nerd Font), color scheme ("vscode"), scrollback.
- **Notifications**: sounds per event (task_complete, approval_needed),
  delivery mode (only_when_not_focused), break-through-focus, timeline limit.
- **Keyboard Shortcuts** (`16-settings-keyboard.png`): searchable, rebindable
  registry (see doc 09), "Find by shortcut", Reset all.
- **Command Palette**: toggles for files/worktrees/tabs/commands/conversations/breadcrumbs.
- **AI**: *Agents* (scope Global/Workspace/Project; Model routing…; **New tabs
  open in: Terminal | Chat UI**; per-provider cards: enable, model visibility
  All/Select, Compliant Claude view (beta), Advanced setup = executable +
  flags; Refresh model caches), *Defaults/Providers/Execution & approvals/
  Session context*; *Profiles* (CLAUDE_CONFIG_DIR / CODEX_HOME account
  profiles); *Prompts*; *Routing* (task-tier model matrix, doc 07).
- **Git & Worktrees**: auto branch naming, auto fast-forward, cleanup options
  (delete local branch default-on; remote-tracking default-off).
- **Workspaces**: one entry per workspace (theme override) — "Default", "Noches".
- **Projects**: per-project settings (scripts, default branch/target, worktree root).
- **Advanced**: Privacy, Experimental (review, chat_editor,
  shared_context_workspaces, remote_workspaces, non_git_projects,
  hapi_mobile_resume, agent_orchestration, automations, browser_automation,
  claude_auto_compact, interactive_claude_chat_view), Docs link.

## 9. PiP & other windows

- **PiP** (⌘⇧P): compact floating mini-window of the active tab — mirrors the
  tab's *entire split tree* scaled down (`17-pip-window.png`), with its own
  traffic lights (green = pin), pin/unpin, expand, close, and its own bottom
  git bar. Pinning persists across focus changes. Runs are selectable in the
  PiP ("pip-open-picker-fab"). GPUI entities: `pip_window::{PipWindow,
  PipSplitContent}`, elements `pip-pin-fab`, `pip-tab-bar`, `pip-top-glass-bar`,
  `pip-bottom-glass-bar`.
- Window menu: **Fill ⌥⌘F, Center ⌥⌘C**, Move & Resize ▸ (Halves
  L/R/T/B, Quarters TL/TR/BL/BR, Arrange: Left & Right, Left & Quarters,
  Right & Left, Right & Quarters, Top & Bottom, Top & Quarters, Bottom & Top,
  Bottom & Quarters, Quarters; Return to Previous Size), Full Screen Tile,
  Remove Window from Set, Minimize, Zoom (`01-menu-window.png`).
