# 01 — Overview & Tech Stack

## What Super is

Super (super.engineering, internally **Superconductor**) is a native macOS
desktop app for running **parallel AI coding agents** against isolated git
worktrees. One window holds multiple *workspaces*; each workspace holds
*projects* (repos); each project holds *worktrees* (branch-backed tasks); each
worktree holds *tabs* (agent chats, terminals, files, diffs, browser), and tabs
can be **split into panes**. A picture-in-picture mini window can float any
compatible tab. The app restores everything per-worktree on return.

- Site: <https://super.engineering/> · Docs: <https://super.engineering/docs/>
- Authors: David (@haveanicedavid) and Zarif Pour (@zarifpr)
- Listed in zed-industries/awesome-gpui
- Distribution: direct download (nightly channel, Sparkle-style auto-updater
  built in: `sc_auto_update::AutoUpdater`), plus an iOS companion app
  ("Superconductor", App Store id6749349238) that connects through the same
  hub used for SSH remotes.
- Installed here: `/Applications/super.engineering.app`, version
  `faa1fb9f…` (nightly, released Sep 17 2026), `api_version 33`.

## Tech stack — verified from the binary

| Component | Evidence |
|---|---|
| **Rust** | Mach-O arm64 static binary (165 MB), cargo registry paths `/Users/zarifpour/.cargo/registry/src/index.crates.io-…/` embedded, Rust panic strings |
| **GPUI (Zed's GPU UI framework)** | vendored `crates/gpui` in binary crate list; `gpui::app`, `gpui::arena`, `gpui::view::Empty`, `elements::animation::AnimationState` symbols; links Metal/OpenGL/AppKit/CoreText exactly like Zed; zero Swift libraries |
| **gpui-component (longbridge)** | `gpui_component::input::state::InputState`, `gpui_component::theme::Theme`, JSON theme schema URL `github.com/longbridge/gpui-component/.theme-schema.json` embedded |
| **Terminal emulation** | `portable-pty-0.9.0` (wezterm's PTY crate) + `vte-0.15.0` (parser); custom renderer (`sc_terminal_core::terminal::Terminal`, `sc_terminal_view::TerminalView`) |
| **SQLite** | `rusqlite-0.32.1`, WAL mode (`-shm`, `-wal` files), FTS5 virtual tables |
| **Async runtime** | `tokio-1.49.0` |
| **WebKit** | `WKWebView` for the in-app Browser tab and markdown/file previews (`sc_browser`, `preview_protocol`) |
| **Fonts** | Lilex (UI mono + code), Lilex Nerd Font (terminal glyphs) — bundled; Nerd Fonts + Lilex repos referenced in binary |
| **Updater** | custom (`sc_auto_update`), nightly channel, staged downloads |
| **Marketing claim** | "Built entirely in Rust … GPU-accelerated UI" (super.engineering); Show HN thread confirms GPUI + one-dark-derived theme |

`Contents/` layout: `MacOS/superconductor` (main app, 165 MB),
`MacOS/sc` (CLI, arm64), `Resources/bin/sc-{linux,macos}-{aarch64,x86_64}`
(the `sc` CLI fat binaries that get installed to `~/.superconductor/bin/`),
`Resources/super.icns`. Min macOS 14.0.

## Product surfaces the user wants cloned

1. **Main workspace for all coding agents** — one window, all providers in one
   sidebar/tab bar (doc 07).
2. **Chat and CLI views** — rendered chat UI *and* the provider's native TUI in
   a terminal, switchable in place for the same session (docs 04, 05).
3. **Split view workspaces** — panes inside a tab (⌘D/⌘⇧D) and full
   split *views* (⌥⌘D/⌥⌘⇧D), restored per worktree (doc 06).
4. **Multiple terminals in one window** — terminal tabs, terminal panes in any
   split, right-panel terminal, PiP (doc 05/06).
