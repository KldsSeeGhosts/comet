# 05 — Terminal & CLI Views

This is the "CLI view" half of the product: every provider can run as its
native TUI, and plain shells are first-class too.

## Implementation

- **PTY**: `portable-pty 0.9` (wezterm) — one PTY per terminal pane/tab.
- **Emulation**: `vte 0.15` ANSI parser feeding a custom grid model
  (`sc_terminal_core::terminal::Terminal`); rendering in
  `sc_terminal_view::TerminalView` with GPUI (GPU); font **Lilex Nerd Font**
  (`terminal_font_family`), color scheme setting (`terminal_color_scheme:
  "vscode"` — selectable palette independent of app theme), per-surface font
  size (`Increase/Decrease/Reset Terminal Tab Text Size` menu items,
  `⌥⌘+/−/0`), 6 text-size families total (app zoom, chat, editor, terminal
  tab, right-sidebar terminal).
- **Shell**: login shell via `LoginShellEnv`; launched with CWD = the owning
  worktree directory (verified: prompt at
  `~/superconductor/projects/open-design-project`); shell integration scripts
  injected from `~/.superconductor/{bash,zsh}`; env `SUPERCONDUCTOR_*` for
  context. `direnv` integration (`sc_direnv_export`).
- **Scrollback/selection**: wheel history (precise line + programmatic pixel
  scroll — `PreciseScrollLineWheelHistoryProgrammaticPixel` symbol),
  selectable text, find bar (`FindBar`), context menu with **Preview File /
  Copy Path / Open With / open URL in browser** for recognized paths/URLs
  (hover underlines them).

## Terminal surfaces (5 places a terminal can live)

1. **Terminal tab** (⌘T / launcher ▸ Terminal) — full pane, own tab.
2. **Terminal pane** in any split (⌘D then pick Terminal; "Hold ⌘ for
   Terminal" in launcher = ⌘-click any provider row to open it as a terminal
   session instead of Chat UI).
3. **Right-panel terminal** (⌃` toggles; `DraggedBottomTerminalResize` for the
   docked bottom terminal; its own font-size menu family).
4. **Provider terminal tabs** — e.g. ⌘-click "Claude Code" runs `claude` TUI
   in the worktree.
5. **PiP** of any of the above.

## Chat ⇄ CLI in-place switching (key differentiator)

- Tab context/view menu offers **switch surface** for idle sessions when a
  resume target exists — supported for Claude Code, Codex, Cursor, Grok,
  OpenCode, Pi, Oh My Pi, Kimi Code (docs/terminal-and-chat). The same
  provider session reopens in the other surface; busy sessions refuse.
- Chat UI sessions wrap the CLI: structured transports (`claude -p` streaming,
  Codex proto/JSONL, Pi session JSONL tailing via `session_discovery`) so the
  transcript, tool calls, and diffs render natively.
- Compliant Claude (beta): local chats run through `claude` interactive mode
  instead of `-p`; SSH remotes always use structured `claude -p`.

## Terminal tab persistence

`session.json` terminal tab: `{kind:"terminal", preset_key:"terminal",
working_directory_path:"<worktree>", title:"Terminal"}` — terminal *tabs*
restore position; scrollback/processes are not resurrected (a dead pane closes
silently), consistent with "closing a tab never discards worktree changes".

## Shortcuts

| Keys | Action |
|---|---|
| ⌘T | New terminal tab (File menu: New Shell Tab ⌥⌘T also exists) |
| ⌘⇧T | Open tool picker |
| ⌃` | Toggle right-panel terminal |
| ⌃⇧T | New shell tab from a non-terminal surface |
| ⌥⌘+ / ⌥⌘− / ⌥⌘0 | Terminal tab text size +/−/reset (right-sidebar family: ⇧⌥⌘ variants) |
