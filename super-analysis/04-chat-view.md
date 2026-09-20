# 04 — Chat View (agent chat UI)

GPUI entities: `sc_chat_view::ChatView`, `ChatTimelineList`,
`MarkdownMessageView`, `ChatTimeline` (sc_chat_timeline), richtext pipeline in
`sc_chat_richtext`, persistence via `conversation_*` SQLite tables and
provider transcript stores.

## Empty state (before first message)

- Provider glyph centered (brand-colored, soft 3D mark), then a **randomized
  serif one-liner tagline** — observed: "It's tomorrow already. Efficient.",
  "Houston, we have a reproducer.", "Fewer leaps. Better landings.",
  "Hooray, Ermin!" (uses machine/username). Font is a serif display face —
  deliberate contrast with the UI sans/mono.
- Divider, then "Your custom actions will appear here." +
  underlined link **"Configure actions in Settings"** (custom actions = named
  prompts shown as cards; `empty_state_actions::ActionCardView` /
  `ActionOverflowView`).
- Ghost composer pinned at bottom-center ("Type a message…" / "⌘L to focus
  chat" when unfocused).

## Composer anatomy (bottom of focused chat pane)

Row 1: multiline input, placeholder "Type a message…". `@` mentions worktree
files (`chat_editor_mentions`), pastes of large text become staged context;
file drag-drop shows `DraggedRepoFilesPreview`.

Row 2 (left → right; see `10-provider-picker.png`):
- **Provider pill** — provider icon + model/profile label, e.g.
  `devin/swe-2` (Codex) or `cpa/devin/swe-2` (Pi profile). Click/hover opens
  the provider+model picker (same list as launcher, plus models from
  `detected-models.json`; per-provider reasoning efforts low/medium/high/xhigh/max).
- **Model/effort pill** — bar-chart icon + "Max"/"Medium"…
- **Lightning icon** — "Enhance prompt" quick action (prompt_improvement).
- **Paperclip/attach** — attachments.
- Right side: `+` (insert/attach menu) and a circular **send ▲** button with a
  gradient ember ring (`composer_ember`, `composer_send_button`).
- Far right of unfocused panes: a circular **context-usage badge** ("9%") —
  session context window consumption.
- Blue underline glow under the focused composer (focus indicator).

The **provider header** of a pane shows resolved provider, model, profile and
permission controls; permission modes are per provider (`permission_mode:
"bypass"` in session.json; Settings → Agents → Execution & approvals).

## Transcript rendering

- Markdown messages (`MarkdownMessageView`) via `sc_markdown`; **collapsible
  code blocks** (`code_block_disclosure::DisclosureView`), syntax highlighting
  (tree-sitter grammar set from nvim-treesitter vendored), inline diffs.
- **Streaming edge**: newly arriving text gets a highlight attachment that
  fades (`streaming_edge::IncomingTextHighlight`).
- Thinking blocks collapsible (`expand_thinking_by_default` off); tool calls
  collapsible (`expand_tool_calls_by_default` off), completed tool summaries
  collapsed by default.
- **File edits inline**: chat shows changed files with per-file +/− counts
  (`render::files_changed`, `show_file_edits_inline: true`,
  `show_files_changed_summary: true`) and "live edits" preview toggle.
- Turn footer: timestamp + duration ("2:35 AM · 59.6s") above a hairline;
  `RunTimerView` shows "Worked for 58s · done" style status.
- Subagent activity nests under the parent task (parent owns approvals/diff).
- Approval / question / error states render as interactive cards
  (`ActionPromptTooltip`, right-click menu on prompts); approvals queue into
  the notification inbox when the provider hook reports them.
- "Steering": typing while the agent runs queues follow-ups
  (`steer_shortcut_hint`); send behaves as steer for supporting providers.
- Message history browser: clock icon (top-right) opens **previous sessions**
  list; actions include Clear conversation, Compact session, "Open a new
  Superconductor tab forked from this session" (fork/branching exists in
  `conversation_runtime::fork`).
- `ActivePlanHud` — floating HUD when a plan card is active (`plan_card_expanded`).

## Session model

- Every chat tab = a conversation row (`conv:<provider>:<session_id>`); local
  history in SQLite + provider-native transcript (e.g. Pi JSONL at
  `~/.pi/agent/sessions/...`; Codex thread archive; Claude projects).
- Restore validates the provider resume target; on failure opens a fresh
  session rather than faking continuity (docs/terminal-and-chat).
- Tab titles: first prompt → AI naming provider (`tab_title_generation`,
  cooldown per attempt); rename via `sc tab title "..."`.

## Empty-view launcher & actions

- Custom actions (user-defined prompts) surface as cards in the empty state
  and in the composer `+` menu; action prompts dispatch per
  `action_prompt_dispatch` settings: `inject_current_or_new_tab` vs
  `always_new_tab` (per action: create_pr, commit_push, commit,
  resolve_conflicts, fix_ci, fix_merge_blocked, fix_comments, fix_changes).
- Action flights animate the dispatch (`ActionFlightView`).
