# 09 — Keybindings & Menus

All shortcuts rebindable in Settings → Keyboard Shortcuts (searchable list,
"Find by shortcut", per-item "Terminal on"/"Enabled" badges, "Reset All to
Defaults"). The registry lists ≥100 actions (A→Z: Activate Navigation Slot 1–9,
Activate Workspace 1–10, Add Project, Autofill Selected, Cancel URL,
Choose Open App, Choose Run Script, …).

## Menu bar (System Events / visual walk)

**super.engineering** — About, Settings ⌘, (standard set; Settings opens the
full-window settings surface), Services, Hide, Quit.

**File**
| Item | Shortcut |
|---|---|
| New Worktree | ⌘N |
| New Worktree From… | ⇧⌘N |
| New Tab (default provider chat) | ⌘T |
| New Shell Tab | ⌥⌘T |
| Open… | ⌘O |
| Close Tab | ⌘W |
| Close Window | (⇧⌘W) |

**Edit** — standard: Undo ⌘Z, Redo ⇧⌘Z, Cut ⌘X, Copy ⌘C, Paste ⌘V,
Select All ⌘A, AutoFill ▸, Start Dictation, Emoji & Symbols.

**View**
| Item | Shortcut |
|---|---|
| Toggle Right Panel | ⌘E |
| Toggle Left Sidebar | ⇧⌘E |
| Toggle Both Sidebars | ⌃⌘E |
| Toggle Panel Terminal | ⌃` |
| Toggle Picture-in-Picture | ⇧⌘P |
| Increase / Decrease / Reset App Zoom | ⌘= / ⌘− / ⌘0 |
| Increase / Decrease / Reset Chat Text Size | ⌥⌘= / ⌥⌘− / ⌥⌘0 |
| Increase / Decrease / Reset Editor Text Size | (unassigned by default) |
| Increase / Decrease / Reset Terminal Tab Text Size | ⌥⌘+ / ⌥⌘− / ⌥⌘0 family |
| Increase / Decrease / Reset Right Sidebar Terminal Text Size | ⇧⌥⌘ family |
| Enter Full Screen | ⌃⌘F |

**Go** — Recent ▸ (recent worktrees, e.g. "Mac Storage Cleanup, main").

**Window**
| Item | Shortcut |
|---|---|
| Fill | ⌥⌘F |
| Center | ⌥⌘C |
| Move & Resize ▸ (Halves, Quarters, Arrange 3+ zone presets, Return to Previous Size) | |
| Full Screen Tile | |
| Remove Window from Set | |
| Minimize / Zoom | |

## In-app shortcuts (docs + observation)

| Keys | Action |
|---|---|
| ⌘1…⌘9 | Activate Navigation Slot 1–9 (default: provider tab shortcuts shown in launcher; setting "Use Cmd+number to switch tabs" repurposes them for tabs) |
| ⌘⌥1…⌘⌥0 | Activate Workspace 1–10 |
| ⌘K (title-bar search) | Search/command palette (docs call it CtrlK; palette footer: ⌘⏎ Switch, ⏎ Open, → Filter, Esc Close, ⌃G Global scope) |
| ⌘L | Focus chat composer (placeholder "⌘L to focus chat") |
| ⌘R | Run (worktree context menu) |
| ⌘⌥A | Add Project |
| ⌘⌥R | Choose Run Script |
| ⌥⌘C | Copy worktree path |
| ⌥⌘0 | Choose Open App |
| ⌘T / ⌘⇧T / ⌃` / ⌃⇧T | Terminal: new tab / tool picker / panel terminal / shell tab (see doc 05) |
| ⌘D, ⌘⇧D | Split pane right / down |
| ⌥⌘D, ⌥⌘⇧D | Split view right / down |
| ⌥⌘W | Close current split view |
| ⌘⇧P | Toggle PiP |
| ⌘N / ⌘⇧N | New worktree instant / from-branch picker |
| "Show Cmd+number hints on Cmd hold" (0.5s) | overlay numbering worktrees/tabs |

## Zoom system (four independent text scales)

App zoom (whole UI), Chat text scale (chats + agent terminal tabs), Editor &
diff font size, Terminal tab font size, Right-sidebar terminal font size —
each with Increase/Decrease/Reset (`ui_scale`, `chat_text_multiplier`,
`font_size: 12`, `line_height: 16` defaults).
