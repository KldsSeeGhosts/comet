# 11 — Persistence & Local API

Everything the app keeps lives in `~/.superconductor/` (name unchanged by the
super.engineering rebrand; env prefix `SUPERCONDUCTOR_*`, config file
`superconductor/config.json` in repos).

## Directory map (verified)

```
~/.superconductor/
├── settings.json              # global app settings + workspaces + projects (doc 08/10)
├── session.json               # per-workspace selection: tabs + split trees + snapshots
├── session_state.json, active-route.json   # last selection (workspace/project/worktree)
├── chat-defaults.json         # per-provider default model/effort
├── detected-models.json       # live model catalog per provider (reasoning efforts, modalities)
├── db/superconductor.db{,-shm,-wal}   # SQLite, WAL
├── bin/                       # PATH shims: sc + 17 provider CLIs
├── hooks/                     # per-provider hook configs + notify.sh (+ statusline, plugins)
├── bash/ zsh/                 # shell integration injected into terminals
├── worktrees/<project>/<name> # task worktree directories
├── local-api.json + local-api.sock (+ .lock, .selection.lock)
├── chat-snapshots/ one-shot/ snapshots/ migration/
├── logs/ cache/ update-cache/ instances/ codex-session-logs/
├── hook-port, hook-queue/     # hook server (sc_agent::hook_server)
├── lifecycle_notifications.json, notifications.json, tool-status.json,
├── pr_cache.json, install_id, pending-renames/
```

## settings.json — field inventory (see `reference/settings.json` verbatim)

- Tools registry `tools{}` (18 entries, command+args+env), `default_tool`,
  `default_add_action`.
- `ai_routing` v2 (task tiers, fast/thorough providers, per-provider models),
  `ai_execution` (permission bypass modes), `action_prompt_dispatch`
  (per-action inject/new-tab policy).
- `experimental{}` flags (review, chat_editor, shared_context_workspaces,
  remote_workspaces, non_git_projects, hapi_mobile_resume,
  agent_orchestration, automations, browser_automation, claude_auto_compact,
  interactive_claude_chat_view, enabled_providers, experimental_providers).
- Typography/theme/layout: `ui_language`, `font_family`, `mono_font_family:
  "Lilex"`, `terminal_font_family: "Lilex Nerd Font"`,
  `terminal_color_scheme`, `font_size: 12`, `line_height: 16`, `ui_scale`,
  `chat_text_multiplier`, `theme_mode`, `workspace_tab_placement`,
  `pinned_tab_style`, `workspace_vertical_tab_rail_width: 196`,
  `loading_indicator_style`, `app_icon`, `left_sidebar_layout: "detailed"`,
  `left_sidebar_width_design: 260`, `right_panel_width_design: 260`,
  `auto_hide_sidebars`, `worktree_sort_mode: "activity"`.
- Worktrees/git: `automatic_branch_naming`, `auto_fast_forward(+_default)`,
  `delete_local_branch_on_cleanup`, `delete_remote_tracking_branches_on_cleanup`.
- Chat display: `diff_view_mode`, `diff_word_wrap`,
  `markdown_file_open_mode: "rich"`, `show_file_edits_inline`,
  `show_files_changed_summary`, `show_live_edits_in_chat`, `chat_full_width`,
  expand_* defaults, `show_conversation_cost`, `conversation_cost_scope`.
- Notifications, palette section toggles, `open_in_apps`,
  `right_panel_tab: "changes"`, `default_right_panel_layout_mode: "stacked"`,
  `review_submit`, `preferred_commit_action: "commit_and_push"`,
  telemetry, update channel.
- `workspaces[]`: id, name, icon, project_ids, `worktree_sections[]`
  (`{id:"pinned", kind:{type:"manual"}}` — filter sections have rule kinds),
  `workspace_type: Individual|SharedContext|Remote`, panel layout mode +
  weights, `ai_tier`.
- `projects[]`: id, name, `main_repo_path`, ui_state (color hue, worktree
  usage scores), settings (scripts/branches/worktree root).

## session.json — tab/split persistence

Full format in doc 06. Notes: `provider_profile_selection.mode` =
`cli_default | inherit`; `messages_snapshot` = inline recent transcript;
`title_sc_owned` marks AI-generated titles; conversation ids
`conv:<provider>:<session>`; Pi sessions reattach by JSONL path.

## SQLite schema (`reference/db-schema.sql`, 30+ tables)

Groups:
- Conversations: `conversations` (metadata JSON incl. `chat_view_state_json`),
  `conversation_messages`, `conversation_turns` (source_kind/source_key),
  `conversation_transcript_items` + `_state`, `conversation_aliases`,
  `conversation_assets`, `conversation_bindings` (worktree↔conversation),
  `conversation_runtime_commands`, `conversation_runtime_turn_ids`,
  `conversation_state_cache`.
- Search: FTS5 `fts_conversation_titles`, `fts_conversation_transcript`.
- Chats: `chat_sessions`, `chat_messages`.
- Ops: `command_executions`, `automation_runs`, `import_checkpoints`,
  `session_scopes`, `schema_version`.
- Usage: `usage_facts`, `usage_sessions`, `usage_fact_claims`,
  `usage_replace_stage` (cost metering feeding the $ pill + usage meter;
  pricing catalog cached in `cache/models-dev-pricing.json`).
- `embedding_refs` (semantic search prep).

## Local API + sc CLI

- Socket `~/.superconductor/local-api.sock`; `local-api.json` pins
  `{version: 33, pid, socket_path}` (instance ownership).
- `sc status --json` → `{api_version, app_version:"0.1.0", instance{pid,
  process_start_token, build_channel "prod", build_commit, executable_path}}`.
- Command groups: `status, instance, history, chat, project scripts, commands,
  layout, agents, agent, team, coordination-state, section, worktree, tab,
  browser, workspace, mobile` (full text in `reference/sc-cli-help.txt`;
  built-in agent docs in `reference/sc-instructions-*.txt`).
- Auth model: local Unix socket, no network listener; browser-automation
  page-mutating verbs need a per-workspace grant; experimental gates in
  Settings.

## Notification/hook pipeline

`hooks/notify.sh` (provider-agnostic JSON via argv/stdin, bounded read) →
app `hook_server` (port file `hook-port`, queue dir `hook-queue/`) →
status dots / approval prompts / notification inbox / sounds. Claude-specific
`claude-settings.json` + `claude-settings-statusline.json` (statusline),
`statusline.sh`.
