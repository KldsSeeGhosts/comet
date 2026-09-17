# 02 — Architecture

## Internal crate map (recovered from binary symbols)

The workspace has ~70 crates. Names below are verbatim from binary strings
(`sc_*` module paths and `crates/…` paths). This is the blueprint for how the
app separates concerns — Xeron's port can mirror it.

**App shell / workspace**
- `superconductor` (bin): `SuperconductorApp`, `render::AddProjectMenu`,
  `app_menus::AboutWindow`, `settings_view::SettingsView` (+ repository
  `ScriptSectionView`, `SettingsSidebarSubmenu`), `notification_inbox`
  (`NotificationInboxBar`, onboarding modal), `mobile_hub_controller`,
  `automations::{view::AutomationsView, ui::rail::{AutomationActionsMenu,
  ExternalActionsMenu}}`, `remote_preconnect::view::RemotePreconnectSurface`,
  `DraggedRightSidebar`, `PresentationSurface`
- `crates/workspace` → `WorkspaceState`, `tab_manager`, `tab_launcher`,
  `tab_toggle`, `tab_title_generation` (AI naming w/ naming_provider_attempt +
  cooldown), `pip_window::{PipWindow, PipSplitContent}`, `run_fab`,
  `workspace::{SplitViewRight, SplitViewDown, SplitPaneRight, SplitPaneDown,
  CloseSplitView}` actions, `right_panel`, `terminal_metadata`,
  `session_restore`, `conversation_refresh`, `remote_history::hydration`,
  `local_api`, `pr_ops`, `prompt_improvement`, `team_run_delivery/persistence`,
  `fx_session_watcher`, `api_chat_events`, `open_in_app`
- `crates/sidebar` → `SidebarPresentation`, `animation_layer`,
  `SidebarPeekBackdrop`, `CmdTooltip`
- `crates/right_panel` → `RightPanel`, `file_tree`, `change_list`,
  `checks_panel`, `tab_indicator`, `CopyPathMenu`, `DraggedStackedShellResize`,
  `DraggedBottomTerminalResize`, `action_transition`
- `crates/command_palette` → fuzzy command/file/worktree/conversation palette

**Terminal**
- `crates/sc_terminal_core` → `terminal::Terminal` (portable-pty + vte)
- `crates/sc_terminal_view` → `TerminalView`, `LoginShellEnv`

**Chat / agents**
- `crates/chat` → providers: `claude_interactive` (engine, prompts, mapping,
  transcript, attachments, auth gate, signals phase machine
  running/pending/in_progress), `codex` (+ `thread_archive`,
  `scratch_archive`), `opencode::runtime`, `pinot`, `replay`
  (`SC_CHAT_PROVIDER_EVENT_REPLAY`), `one_shot`, `one_shot_history`
- `crates/chat_runtime` → session lifecycle (`lifecycle::idle_fence`,
  `durable_admission`), `history`, `ingress`, `memory`, `telemetry`,
  `registry`, `session::commands`, `durable_turn_id/runtime_turn_id`
- `crates/conversation_runtime` → canonical mutations, `provider_reconcile`
  (effects, history loader), `repair_dedupe`, `save_coordinator`, `fork`,
  `preserve_unsaved_history`
- `crates/chat_view` → `ChatView`, `ChatTimelineList`, `MarkdownMessageView`,
  composer pieces (`composer_send_button`, `composer_ember`, `composer_completion_wash`,
  `steer_shortcut_hint`, `ActionPromptTooltip`, `ActivePlanHud`, `RunTimerView`,
  `ActionFlightView`, `empty_state_actions`, `worktree_preparation`,
  `subagent_codex`, `restore_merge`, `link_presentation`,
  `PromptImprovementTooltip`, `files_changed` renderer)
- `crates/chat_richtext` → markdown, `code_block_disclosure` (collapsible code
  blocks w/ `DisclosureView`), `streaming_edge` (`IncomingTextHighlight`,
  `HighlightAttachment`), `history_preview`
- `crates/chat_timeline`, `chat_transcript_store`, `sc_chat_transcript`,
  `chat_tooling`, `chat_transcript_model`, `chat_types`, `chat_input`,
  `chat_controls::editor_shell`, `provider_contract`, `prompt_processing`,
  `chat_event_replay`
- Agent keys observed in `chat_view::render`: providers `Codex, Claude, Cursor,
  Grok, OpenCode, Pi, Oh My Pi, Prime Agent, Kimi, Superpowers Plugin,
  Unknown`, plus `ChatSkillRefrange`, `ToolInputAnalysis`

**Git / review**
- `crates/git`, `git_service` (`repository::Repository`, `store::RepositoryStore`,
  `pr_service::PrServiceState`), `diff`, `diff_view` (`DiffView`, `DiffToolbar`,
  `remote_threads::card::RemoteThreadCard`, `DraggedDiffSplitResize`,
  `CommentPreviewTooltip`, selectable text, view modes), `review_comments`,
  `worktree` (`worktree::Worktree`, `store::WorktreeStore`, `watcher`, `lane`)

**Files / editor / browser**
- `crates/file_view` → `FileView`, `ImageView`, `pdf::PdfView`,
  `edit::FileEditor` + `ComponentThemeSyncState`, `selectable_text`
- `crates/sc_bezel` → `editor::MarkdownEditors` (bezel_editor), `image_store`
- `crates/sc_browser` → `BrowserView`, `render::favicon`, `native`,
  `preview_protocol`; `sc_browser_automation`

**UI kit** (`crates/sc_ui`): `context_menu::ContextMenu`, `tooltip::{TextTooltip,
CopyableTextTooltip, ShortcutTooltip}`, `find::FindBar`, `shimmer::ShimmerView`,
`loading_indicator::LoadingIndicatorView`, `numeric_stepper`, `searchable_combobox`,
`icon_picker::IconPickerPopover`, `avatar`, `chrome_menu::ChromeMenu`,
`right_click_menu`, `dropdown_anchor::AnchoredOverlayState`,
`horizontal_reveal::RevealState`, `profile_transition`, `setup_loading`,
`scale::UiScaleState`, `dragged_files::DraggedRepoFilesPreview`, `glitch_text`,
`number_flow` (animated numbers)

**Infra**
- `crates/sc_db` (rusqlite; `conversation_runtime`, `conversation_search`),
  `sc_fuzzy`, `sc_markdown`, `sc_import`, `sc_telemetry`, `sc_assets`
- `crates/local_api{,_hub,_server}` + `sc_hub`, `sc_hub_proto` (Unix-socket
  JSON API + mobile hub protocol), `remote` (SSH remotes, authorized_keys
  management with flock/lockf helpers), `scheduler`, `session`, `settings`,
  `shell_hooks`, `history`, `usage` (`cost_meter`, `UsageMeter`, `CostStore`,
  pricing catalog refresh, per-provider sources incl. kimi/copilot/gemini),
  `auto_update`, `http_client`, `attachments`, `file_read`, `file_drop_import`,
  `syntax`/`sum_tree`/`picker` (Zed-derived), `vendor`
- Vendored `crates/gpui` (Zed's UI framework, patched fork)

## Process & integration model

1. **Single main process** (`superconductor`, this install: pid 4276) owns all
   windows (main window, PiP windows, popovers render in-window).
2. **Agent CLIs run as child processes** of the app — either wrapped by the
   Chat-UI adapter (structured transport, e.g. `claude -p` / Codex proto /
   transcript JSONL tailing) or attached raw in a PTY (terminal surface). PATH
   is prepended with `~/.superconductor/bin/`, which contains **shims for every
   provider CLI** (`claude`, `codex`, `pi`, `cursor-agent`, `droid`, `grok`,
   `agy`, `omp`, `kimi`, `kiro-cli`, `qwen`, `copilot`, `gemini`, `hermes`,
   `opencode`, `fx`, `prime-agent`) plus `sc`.
3. **Shell integration**: `~/.superconductor/{bash,zsh}/` scripts are injected
   into terminals (prompt/OSC context so the app can map terminals to
   worktrees; env vars `SUPERCONDUCTOR_*`).
4. **Event hooks back into the app**: `~/.superconductor/hooks/` contains
   per-provider hook configs the app writes into the providers' own config
   systems. Example — `claude-settings.json` installs Claude Code hooks
   (`SessionStart`, `UserPromptSubmit`, `Stop`, `SubagentStart/Stop`,
   `PostToolUse`, `PermissionRequest`, `PreToolUse` on `AskUserQuestion` /
   `request_user_input`) that call `hooks/notify.sh`; the script posts a JSON
   payload back to the app (stdin or $1, bounded read with
   `SUPERCONDUCTOR_HOOK_MAX_STDIN_BYTES` / `..._TIMEOUT_SECS`). Same pattern
   per provider: `copilot-notify.sh`, `cursor-notify.sh`, `droid-notify.sh`,
   `antigravity-notify.sh`, `pi-superengineering.ts`, `opencode/` plugin,
   `gemini/` extension, `copilot-plugin/`, `copilot-instructions/`,
   `statusline.sh`. **This is how the app gets status dots, "waiting for
   approval" prompts, and the notification queue without an agent-API.**
5. **Local API server**: the app listens on a Unix socket
   `~/.superconductor/local-api.sock` (see `local-api.json`:
   `{version:33, pid, socket_path, created_at_ms}` + `local-api.json.lock`,
   `.selection.lock`). The `sc` CLI talks to it (`--socket PATH`). Powers
   status, chat control, layout orchestration, worktree ops, browser
   automation, teams, coordination-state KV, mobile pairing.
6. **Mobile/remote hub**: `sc_hub` + `sc_hub_proto` implement the hub protocol
   used by the iOS app and by SSH remote workspaces (authorized_keys managed
   safely with lock files; `sc-macos/linux` binaries shipped in Resources/bin).

## Concurrency/animation notes

- `sc_ui::animation_heartbeat` + env `SC_RR_ANIM_HEARTBEAT_LOG` — a global
  animation heartbeat drives shimmer/pulse/loaders.
- Debug/log env vars in binary: `SC_RR_CHAT_VIEW_LOG`, `SC_RR_CPU_LOG`,
  `SC_DB_QUEUE_LOG`, `SC_CHAT_PROVIDER_EVENT_REPLAY`,
  `SUPERCONDUCTOR_HERMES_*` (hermes wrapper), `SC_API_CHAT_PROVIDER_RECONCILE_MAX_CONCURRENT`.
- FS watchers: `sc_worktree::watcher`, `fx_session_watcher`,
  `session_discovery` (`sc_agent::session_discovery`) find provider session
  files (e.g. Pi sessions under `~/.pi/agent/sessions/…`) to reattach.
