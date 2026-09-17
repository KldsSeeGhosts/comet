# 07 — Providers & Agents

## Registry (18 configured tools in `settings.json`; 9 detected live)

`tools.<key>` entries: `{name, command, args, env}` — all overridable in
Settings → Agents → Advanced setup.

| key | Name | Command | Notes (docs / observed) |
|---|---|---|---|
| claude | Claude Code | `claude` | Chat+Terminal. Chat via `claude -p` structured transport; "Compliant Claude view" beta runs interactive mode. Profiles via `CLAUDE_CONFIG_DIR`. 15 models detected |
| codex | Codex | `codex` | Chat+Terminal. Default tool (`default_tool`). Thread archive + scratch; proto/JSON events. 16 models |
| cursor | Cursor | `cursor-agent` | Chat+Terminal |
| opencode | OpenCode | `opencode` | Chat+Terminal; plugin in hooks |
| pi | Pi | `pi` | Chat+Terminal. Sessions as JSONL under `~/.pi/agent/sessions/…`; 23 models |
| omp | Oh My Pi | `omp` | Chat+Terminal; 14 models |
| grok | Grok | `grok` | Chat+Terminal |
| kimi | Kimi Code | `kimi` | Chat+Terminal (experimental) |
| copilot | Copilot | `copilot` | Terminal-only (experimental); plugin+instructions hooks |
| factory | Factory Droid | `droid` | Terminal-only; settings hook |
| kiro | Kiro | `kiro-cli chat` | Terminal-only |
| qwen | Qwen Code | `qwen` | Terminal-only |
| hermes | Hermes | `hermes --tui` | Terminal-only; hermes wrapper scripts in binary |
| antigravity | Antigravity | `agy` | Terminal-only (shown in launcher ⌘9) |
| gemini | Gemini Legacy | `gemini` | Terminal-only, legacy |
| fx | fx (Vercel) | `fx` | configured, not in launcher |
| prime-agent | Prime Agent | `prime-agent` | disabled here |
| terminal | Terminal | — | pseudo-tool for shell tabs |

Launcher order/shortcuts (⌘1–9): Super(1)=default tool, Codex, Claude Code,
OpenCode, Pi, Oh My Pi, Grok, Cursor, Antigravity; then Browser, Terminal
("Hold ⌘ for Terminal" = open that provider in a raw PTY instead of Chat UI).

`sc chat providers --json` returns the live registry incl. per-provider model
lists with `supported_reasoning_efforts` (low/medium/high/xhigh/max) —
mirrored in `reference/detected-models.json`; "Refresh model caches" button in
Settings → Agents; hourly provider-release checks
(`provider_update_checks`, "Automatic network update checks").

## Default surface

Settings → Agents → **"New tabs open in: Terminal | Chat UI"** (global →
workspace → project cascade). Per-tab override via tab menu. Some providers
Chat-UI-capable, others terminal-only (table above).

## Model routing (Settings → AI → Routing; `ai_routing` v2)

Task-tier matrix — each tier maps to a provider+model (with
`\u0000cli-default` = "whatever the CLI defaults to"):

`session_default`, `inline_commit`, `prompt_improve`, `create_pr`,
`commit_push`, `commit`, `resolve_conflicts`, `review`, `fix_ci`,
`fix_merge_blocked`, `fix_comments`, `fix_changes`; plus
`session_default_provider`, `fast_provider`, `thorough_provider`,
`per_provider_models` overrides.

Optional **naming model** for branch/tab naming (`tab_title_generation` with
cooldown skip states ok/signed_out/error/app_unavailable).

## Execution & approvals

- Per-provider permission bypass toggles (`ai_execution`):
  `bypass_claude_permissions`, `bypass_codex_permissions` (mode
  `dangerous_bypass`), opencode/antigravity/grok/kimi equivalents;
  `claude_permission_mode: "bypass"`.
- Approvals/questions arrive via **provider hooks** (doc 02) → notification
  inbox + tab status; "Background means do not focus an agent tab", never
  "skip permissions".
- Notification config: `notification_sounds` per event (task_complete,
  approval_needed), `delivery_mode: only_when_not_focused`,
  `break_through_focus`, `timeline_limit: 50`.

## Session context & instructions

- Every session inherits project/worktree context: directory, provider
  defaults, profile, model routing, notification state, saved layout
  ("Shared context").
- The app injects system instructions (branch naming, worktree rules, action
  prompts) — inspectable via "Chat UI can disclose app/system instructions"
  (docs). Repo instructions (AGENTS.md etc.) are the repo's own.
- `sc instructions <topic>` prints the built-in agent docs (workspace /
  project / commands / worktree / orchestration / layout / review / browser /
  instance) — dumps in `reference/sc-instructions-*.txt`.
- ChatSkillRefrange / skills: providers' skill systems are surfaced
  (`Superpowers Plugin` observed in chat_view provider keys).

## Teams & orchestration (experimental gate)

- `sc team run` — 1–8 labelled roles in parallel tabs; each must finish with
  `sc team report` (16 KiB summary cap); `team status/list` read-only;
  interrupted on app restart, no auto-resume.
- `sc agent send|read|wait|subscribe|stop|interrupt|should-stop`,
  labels & groups (`sc agents group …`), `coordination-state` KV with
  optimistic `--if-version`.
- Requires Settings → Experimental → Agent orchestration.
