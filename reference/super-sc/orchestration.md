Orchestration instructions

- super.engineering orchestration is opt-in per human request. Enabling the feature
  only makes it available; it does not authorize its use.
- Requests for subagents, agents, delegation, parallel work, teams, workers, or
  similar standard agent language use the current provider's native subagent
  tools. Those words never authorize `sc agent`, `sc agents`, `sc team`,
  `sc layout`, or any other super.engineering orchestration command.
- Use `sc` orchestration only when the human user says orchestrate or orchestration,
  names another provider/model or multiple providers/models, or requests
  app-managed UI/session behavior such as a new tab, pane, view, split,
  side-by-side agents, or a visible session.
- An explicit request for native subagents always stays native. If native
  subagent tools are unavailable, report that instead of substituting `sc`
  orchestration.
- Delegate only when the human user explicitly requests delegation, agents, or
  parallel work. Do not infer delegation from task size or possible speedups.
- After a super.engineering trigger, use same-worktree orchestration only when
  `sc layout capabilities --output json` reports it.
- In that response, `team_runs` controls mutating team workflows while
  `team_runs_read` reports read-only `sc team status` and `sc team list` access.

Command map:
- Discover capabilities and targets with `sc layout capabilities`,
  `sc layout views`, `sc agents list`, and `sc agents get`.
- Compose a complete workspace topology and its heterogeneous cell contents in
  one safe flow with `sc layout compose --from-file PLAN --refresh-guard
  --output json`. Use this when the requested final state mixes existing
  sessions, new per-provider sessions, and empty cells;
  `layout_orchestration.compose` must be enabled.
  Cell order maps to topology leaves, and no position is reserved for the
  invoking session. Existing cells may retain a stable whole view, including an
  empty view, by `view_id`. New cells require an explicit UI. Omit `dry_run`,
  `worktree_path`, and `invoking` from plan JSON; CLI flags and the calling
  session own that metadata. `--refresh-guard`
  previews a guardless Close plan and applies it with exact observed view and
  surface IDs; JSON output contains only the final response.
- Launch initial sessions with `sc layout run views|tabs|panes`. Send follow-ups
  to existing targets with `sc agent send`; use `sc layout send` only for its
  layout-oriented broadcast, prefill, or dry-run behavior.
- Observe work with `sc layout state`, `sc layout read`, `sc agent read`,
  `sc agent wait`, and `sc agent subscribe`.
- Control work with `sc layout move`, `sc layout stop`, `sc agent interrupt`,
  `sc agent stop`, and `sc agent should-stop`.
- Manage stable names and collections with `sc agents label set`,
  `sc agents label clear`, `sc agents group create`, `sc agents group add`,
  `sc agents group remove`, `sc agents group delete`, and
  `sc agents group list`.
- For durable multi-agent fan-out/fan-in, prefer `sc team run` over assembling
  layout, wait, and coordination-state commands by hand. `sc team run`
  launches all roles in parallel, one tab per role, each with its own
  `--label`, `--provider`, and `--prompt` (1-8 roles). Each role must finish
  with `sc team report`; summaries are limited to 16 KiB, so put detailed
  output in `--result-file`. `sc team status` and `sc team list` are read-only.
  `sc team cancel` succeeds only after every still-working role target confirms
  it stopped. Runs that were nonterminal when the app restarted become
  Interrupted and are never resumed automatically. `sc team run` has no
  sequencing; for 'A finishes, then B starts' use the sequential handoff flow
  below.
- Share machine-readable decisions, locks, votes, and summaries with
  `sc coordination-state get`, `sc coordination-state set`,
  `sc coordination-state delete`, and `sc coordination-state watch`. Use
  `--if-version` for competing writers.

Targeting and output:
- `sc agents list` lists one worktree, not all worktrees. Without --worktree,
  managed sessions use their launch worktree; unmanaged callers use the selected
  worktree. Supply --worktree PATH to discover agents elsewhere.
- `sc chat list --worktree PATH --json` reports an explicit parked field without
  restoring chats. Without --worktree, managed sessions list their own worktree;
  unmanaged callers list chats across all worktrees.
- Parked chats retain labels and stable target IDs. Read/send restore them in
  the background without selecting their worktree; --open-if-needed is not
  required for parked targets. Retry session_retiring sends after retirement;
  session_parked means restoration failed, not that the target was deleted.
- After a restored chat becomes idle, repeat reads while read_consistency is
  snapshot; consume the settled transcript when it reports complete.
- Prefer `label:<name>` or `id:<stable_target_id>` over volatile layout indexes.
  Use `group:<name>` for intentional broadcasts. Do not launch a replacement for
  a follow-up to an existing target.
- Managed agents default to their launch worktree when `--worktree` is omitted.
  An explicit `--worktree <path>` may target another worktree. Unmanaged human
  CLI callers fall back to the active UI workspace when `--worktree` is omitted.
  Prefer `--output json` for discovery, orchestration, and coordination commands.

Stopping and steering:
- Redirect a running agent with `sc agent interrupt --to label:NAME`, then send
  the correction with `sc agent send`; interrupt breaks the current turn but
  keeps the session.
- Cancel the active turn with `sc agent stop --to label:NAME`
  (`sc layout stop` does the same by layout target); add `--kill` only to
  force-kill a terminal process. Neither closes the pane; use
  `sc layout close` to remove it.

Common follow-up flow:
- Launch with `sc layout run tabs --provider KEY --label NAME --prompt TEXT
  --output json`.
- Continue with `sc agent send --to label:NAME --prompt TEXT --queue
  --output json`.
- Finish by waiting with `sc agent wait --to label:NAME --idle --output json`
  and reading with `sc agent read --to label:NAME --last N --output json`.
- A successful send confirms dispatch or queue admission, not turn completion.
  `--wait-until-idle` waits for target availability before dispatch. After a
  send, use `sc agent wait` and `sc agent read`; provider failures are returned
  as target errors and must not be treated as successful idle completion.

Sequential handoff ('A does the task, then B reviews'):
- Launch A: `sc layout run tabs --provider KEY_A --label a --prompt TASK
  --output json`.
- Wait for A to finish: `sc agent wait --to label:a --idle --output json`,
  optionally reading the result with `sc agent read --to label:a`.
- Start B on the outcome: `sc layout run tabs --provider KEY_B --label b
  --prompt 'Review ...' --output json`, or `sc tab split --direction right
  --provider KEY_B` for a side-by-side reviewer, or `sc agent send` when B
  already exists.

- Never use worktree creation as delegation or as a substitute for unavailable
  agent capability. Report the missing capability to the human user.
- Use `sc help team`, `sc help layout run`, `sc help agent`, and
  `sc help agents` for exact syntax; use `sc help coordination-state` for shared
  state syntax.
