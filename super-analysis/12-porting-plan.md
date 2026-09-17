# 12 — Porting Plan: Super → Xeron (zeron `upstream-v0.2.72`)

Target repo: `/Users/kidsseemac/AiStack/Noches/zeron` (GPUI desktop shell in
`crates/ui`, engine in `crates/engine`, provider adapters in `crates/harness`).
Super is also GPUI — so most porting is *structural adoption*, not
re-implementation of primitives.

## What Xeron already has (reuse, don't rebuild)

| Xeron | Super equivalent | Gap |
|---|---|---|
| `crates/ui/src/shell.rs`, `shell/tabs.rs` | `workspace::tab_manager`, tab bar | per-worktree tab ownership, provider avatar stack, AI titles (engine has `titles.rs`), activity states |
| `crates/ui/src/shell/spaces.rs` | sidebar (workspaces/projects/worktrees) | worktree rows + sections (manual/filter), hover quick-actions, context menu (doc 08), primary ★ |
| `crates/ui/src/shell/command_palette.rs` | search palette (⌘K) | worktree/tab results, Global scope, Navigation Slots, footer action bar |
| `crates/ui/src/composer.rs`, `composer_dock/` | chat composer | provider/model pills, ⌘L focus, context-% badge, ember send, steering |
| `crates/ui/src/transcript.rs`, `markdown/` | `chat_view`/`chat_richtext` | collapsible code blocks w/ disclosure, streaming-edge highlight, tool-call/disclosure states, inline files-changed |
| `crates/ui/src/terminal/` + `engine/src/terminals.rs` | `sc_terminal_core/view` | verify PTY backend (portable-pty + vte is the proven combo), worktree-rooted CWD, shell integration, find bar, link/file hover actions |
| `crates/harness/{claude,codex,acp}/` | provider adapters | add the Super surface model: Chat UI *or* native TUI per tab, in-place switching, structured `-p` transports, transcript reattachment |
| `crates/engine/src/sessions.rs`, `spaces.rs`, `source_control.rs`, `titles.rs` | conversation/worktree/git services | worktree-per-task model, auto branch naming, target-branch diffing, cleanup lifecycle |
| `crates/ui/src/settings/`, `settings.rs` | `settings_view` | scope cascades (Global/Workspace/Project), Agents page, Keyboard Shortcuts registry page |
| `theme.rs`, `crates/theme` | per-workspace themes | chrome/accent/highlight HSL roles + transparency/tint, light+dark per workspace |
| `edge_fade.rs`, `frost.rs`, `motion.rs`, `loaders.rs`, `popover.rs` | sc_ui kit | same primitives already exist |

## Missing in Xeron (build list, in dependency order)

### Phase 1 — Worktree-centric workspace model (foundation)
1. **Object model**: workspace → project → worktree → tab → pane, serialized
   exactly like Super (doc 06 JSON). Implement `split_layouts` as a recursive
   enum `{Split{axis,ratio,first,second}, Leaf{pane_id, content}}` where
   content is `PrimaryTab | Tab(TabState)`; persist per worktree in engine
   state; restore on selection (tabs, active tab, ratios, pane ids).
2. **Worktree engine**: create-from-HEAD instantly on ⌘N (auto name
   `sc-<word>-<word>-<hex4>`, auto branch from first prompt via
   `engine::titles`), storage convention `~/.xeron/worktrees/<project>/<name>`,
   primary worktree starred, target-branch tracking + diff summary, cleanup
   lifecycle with pre/post hooks and local-branch deletion default.
   Mirrors `crates/engine/src/source_control.rs` + `spaces.rs` extensions.
3. **Sidebar rework** (doc 03 §2): project groups, worktree rows (detailed/
   compact layouts, hover quick actions Run/Terminal/Finder/Copy, context
   menu from doc 08, sections incl. filter rules), usage-score "activity"
   sort, sidebar edge-peek auto-hide (reuse `edge_fade`/motion).

### Phase 2 — Split views & panes (the headline feature)
4. **Pane system inside a tab** with the exact action set: SplitPaneRight ⌘D,
   SplitPaneDown ⌘⇧D, SplitViewRight ⌥⌘D, SplitViewDown ⌥⌘⇧D, CloseSplitView
   ⌥⌘W (Super's action names — reuse them as GPUI action names for parity).
   Focused-pane blue ring; ghost composers in unfocused chat panes; divider
   drag + double-click equalize; drag-to-edge split previews with drop ring;
   pane header (provider icon + title + pop-out/close on hover) with the
   context menu from `07-pane-context-menu.png`.
5. **Full split views**: content area becomes a second-level split of
   "views", each with its own tab strip; empty view shows the launcher cards
   + Recent. Tab drag between views/panes.
6. **PiP** (⌘⇧P): floating panel window rendering the active tab's split tree
   scaled, with pin/close/expand (`pip-*-fab`, glass bars — reuse `frost.rs`
   vibrancy). Super proves GPUI multi-window works here.

### Phase 3 — Agent chat + CLI duality
7. **Provider registry** like `tools{}`: name/command/args/env + enabled
   flag + model catalog refresh (`detected-models.json` equivalent, reasoning
   efforts). Launcher dropdown with ⌘1–9, "Hold ⌘ for Terminal",
   Browser + Terminal rows (doc 07 table is the spec).
8. **Chat UI over provider CLIs**: structured transports (Claude `-p`
   stream-json, Codex proto/JSONL, transcript tailing for Pi-like CLIs,
   ACP for the rest via `harness/acp`) with conversation persistence and
   resume validation; hooks pipeline for state/approvals (inject provider
   hook configs + notify script → app server; doc 02 §4).
9. **In-place Chat ⇄ Terminal switching** for idle sessions with a resume
   target; provider header with resolved model/profile/permission controls;
   permission bypass modes per provider.
10. **Composer**: provider/model pills (menu from registry), effort pill,
    prompt-enhance lightning, attachments, `@` file mentions (repo has
    `attachments.rs`, `context_usage.rs` for the % badge), steering queue.

### Phase 4 — Chrome & supporting surfaces
11. **Right panel** (⌘E): Files / Changes / Review / Checks tabs + bottom
    Setup/Run/Terminal cluster + git action bar; docked bottom terminal with
    draggable divider; `superconductor/config.json`-style project scripts.
12. **Search palette upgrade**: worktrees/tabs/commands/conversations,
    Global toggle, Navigation Slots, footer action bar.
13. **Settings surface**: full-window page with sidebar (General/Appearance/
    Terminal/Notifications/Keyboard Shortcuts/Command Palette/Agents/
    Routing/Worktrees/Workspaces/Projects/Privacy/Experimental), including
    the **rebindable keybinding registry** page.
14. **Window chrome**: status pills (queue, cost `$0.00` via usage metering,
    Run ▷), notification inbox, Move & Resize menu (halves/quarters/arrange),
    Pin Window, empty-state serif taglines.

### Phase 5 — Optional/experimental parity
15. `sc`-style local API (Unix socket + JSON, api-versioned) and CLI shim on
    PATH; layout orchestration verbs; browser automation (WKWebView tab +
    snapshot/click/fill); mobile hub; SSH remote workspaces (authorized_keys
    flow); teams (`team run/report`); coordination-state KV.

## Concrete first PR slice

`feat: worktree-scoped tab workspace with split panes`
- new `crates/ui/src/split_tree.rs` (model + GPUI render + hit-testing)
- extend `shell/tabs.rs` for per-worktree tab sets + launcher dropdown
- actions `workspace::{SplitPaneRight, SplitPaneDown, SplitViewRight,
  SplitViewDown, CloseSplitView}` wired to ⌘D/⌘⇧D/⌥⌘D/⌥⌘⇧D/⌥⌘W
- persistence in `engine` (session_state equivalent of `session.json`)
- terminals + chats as pane contents (existing `terminal/` + `composer.rs`)

This alone reproduces the user's headline asks: main agent workspace, split
views, multiple terminals per window — with chat/CLI duality following in the
next slice.

## Risks / notes

- GPUI popover/menu focus quirks: Super renders menus in-window; synthetic
  click edge cases observed during analysis (jot overlay) are environmental,
  not app issues.
- Multi-window PiP + zoomed main window interplay needs GPUI window
  lifecycle care (Super keeps PiP as separate NSWindow with its own
  traffic lights).
- Terminal reattachment after app restart is deliberately *not* done
  (processes die); only tabs/dirs restore — match that to avoid zombie PTYs.
- Do NOT clone the `superconductor` brand assets; Xeron keeps its own brand
  (Noches design language per AGENTS.md), but adopt layout/interaction/UX
  verbatim where the user asked for a functional clone.
