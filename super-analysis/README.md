# Super (super.engineering) — Complete Analysis for the Xeron Port

This folder is the **handoff package** for cloning Superconductor (marketed as
**super.engineering**, app bundle `super.engineering.app`, binary
`superconductor`, CLI `sc`) into **Xeron** (`/Users/kidsseemac/AiStack/Noches/zeron`,
branch `upstream-v0.2.72`, GPUI desktop shell in `crates/ui`).

Everything here was captured from the **installed nightly build**
(`faa1fb9f8cfccd4d7d17f69e8862c5493785c6c1`, api_version 33) on this machine:
live UI observation with screenshots, binary analysis, on-disk state
(`~/.superconductor/`), the `sc` CLI, and the official docs at
<https://super.engineering/docs/>.

**Analysis date:** 2026-09-17

## Document map

| Doc | Contents |
|---|---|
| [01-overview-and-stack.md](01-overview-and-stack.md) | What the app is, tech-stack proof (Rust + GPUI + gpui-component), bundle layout |
| [02-architecture.md](02-architecture.md) | Internal crate map (recovered from binary), process model, hooks, agent transports |
| [03-ui-layout-and-navigation.md](03-ui-layout-and-navigation.md) | Window chrome, left sidebar, tab bar, right panel, bottom bar, search palette, settings surface |
| [04-chat-view.md](04-chat-view.md) | Chat UI: transcript rendering, composer anatomy, empty states, steering, approvals |
| [05-terminal-and-cli-views.md](05-terminal-and-cli-views.md) | Terminal implementation (portable-pty + vte), shell tabs, chat↔CLI in-place switching |
| [06-tabs-splits-and-pip.md](06-tabs-splits-and-pip.md) | Tab model, split panes vs split views (the exact split-tree JSON), drag/drop rules, PiP windows |
| [07-providers-and-agents.md](07-providers-and-agents.md) | 15-provider registry with CLI commands, chat vs terminal surfaces, model catalogs, routing tiers, notification hooks |
| [08-workspaces-projects-worktrees.md](08-workspaces-projects-worktrees.md) | The 5-level object hierarchy, worktree lifecycle, branch naming, sections, cleanup |
| [09-keybindings.md](09-keybindings.md) | Every observed menu shortcut + the rebindable registry, app zoom/text-size system |
| [10-design-system.md](10-design-system.md) | Colors, typography, metrics, theme system, iconography, motion |
| [11-persistence-and-api.md](11-persistence-and-api.md) | `~/.superconductor/` layout, settings.json spec, session.json split-tree format, SQLite schema, local API + `sc` CLI |
| [12-porting-plan.md](12-porting-plan.md) | Phased plan mapping Super concepts onto the Xeron/zeron GPUI codebase, with gaps and risks |

## Screenshot index (`screenshots/`)

| File | Shows |
|---|---|
| `00-initial-window.png` | First observed state: 2×2 split, sidebar, tab bar, composer |
| `01-menu-window.png`, `01b-menu-window-zoom.png` | Window menu incl. Move & Resize tiling presets |
| `02-tab-launcher-menu.png`, `03-tab-launcher-menu.png` | Tab bar before/after creating a Pi chat tab; per-tab split isolation |
| `04-tab-launcher-dropdown.png` | New-tab launcher: Super/Codex/Claude Code/OpenCode/Pi/Oh My Pi/Grok/Cursor/Antigravity/Browser/Terminal with ⌘1–9 |
| `05-terminal-tab.png` | Live shell tab rooted in the worktree dir |
| `06-terminal-split-right.png` | Two terminals side-by-side after ⌘D |
| `07-pane-context-menu.png` | Pane header menu: split pane/view right/down, Tab layout ▸ |
| `08-tab-layout-submenu.png` | Pane header hover state |
| `09-right-panel.png` | Right panel: Files/Changes/Review/Checks, Commits filter, Setup/Run/Terminal, run-script empty state |
| `10-provider-picker.png` | Composer with provider pill, model pill, lightning, attach, context-% badge |
| `11-search-palette.png` | ⌘K-style search: worktrees/tabs, Global scope (⌃G), Navigation Slots, action hints |
| `12-new-worktree-modal.png` | Instant ⌘N worktree creation + auto-branch hint + $0.00 cost pill |
| `13-worktree-context-menu.png` | Worktree row menu: Run ⌘R, Run setup, renames, sections, delete, pin |
| `14-settings.png` | Settings → General (language, updates, behavior toggles) |
| `15-settings-appearance.png` | Theme/customizer/app icon/sidebar layout previews |
| `16-settings-keyboard.png`, `16b-keyboard-2.png` | Rebindable shortcut registry (first screenfuls) |
| `17-pip-window.png` | Floating PiP window mirroring the active tab incl. split tree |
| `18-workspace-switcher.png` | Main window final state (right panel + 2×2 split) |
| `19-settings-agents.png` | Settings → Agents: scope tabs, Terminal vs Chat UI default, provider cards |

## Reference captures (`reference/`)

| File | Contents |
|---|---|
| `settings.json` | **Verbatim copy** of the user's app settings — de-facto feature flag spec |
| `session.json` | Verbatim tab + split-tree persistence (see §6 of doc 06) |
| `detected-models.json` | Per-provider live model catalog with reasoning efforts |
| `db-schema.sql` | Full SQLite schema (`~/.superconductor/db/superconductor.db`) |
| `sc-cli-help.txt` | Complete `sc` command surface |
| `sc-instructions-*.txt` | The app's built-in prompt-docs for agents (workspace/worktree/layout/orchestration/review/browser/commands/project/instance) |

## TL;DR for the porting agent

1. **It is a Rust GPUI app** — same framework family as Xeron (`gpui` + `longbridge/gpui-component`). Icons are plain SVGs, fonts are Lilex / Lilex Nerd Font, theme is HSL-continuous per workspace.
2. **The product = worktree-scoped agent sessions in a tab/split workspace.** Everything (tabs, splits, terminals, chats, diffs) belongs to a git worktree; the whole layout tree is one serializable JSON that restores per worktree.
3. **Chat UI vs CLI are interchangeable surfaces over the same provider CLI session** (Claude Code, Codex, Pi, …) — Chat parses provider transcripts/JSONL; Terminal is a raw PTY (`portable-pty` + `vte`).
4. **State is fully on disk and inspectable**: `~/.superconductor/{settings.json,session.json,db/superconductor.db}` + a Unix-socket local API (`local-api.sock`) driven by the bundled `sc` CLI. `reference/` has verbatim copies.
5. Start with [12-porting-plan.md](12-porting-plan.md).
