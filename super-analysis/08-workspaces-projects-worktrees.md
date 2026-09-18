# 08 — Workspaces, Projects, Worktrees

## Object hierarchy (docs/how-it-works)

| Object | Meaning | Owns |
|---|---|---|
| **Workspace** | a durable context (personal / company / client) | projects, theme (chrome/accent/highlight per light+dark), agent defaults, sections, layout policy |
| **Project** | one repo or folder | branch rules, scripts, provider routing overrides, worktree root |
| **Worktree** | one branch-backed task | files, git state, tabs, processes, diff, review context |
| **Tab** | one focused activity | chat / terminal / file / diff / browser |
| **Shared Context** | one feature spanning projects (experimental) | shared instructions + child worktrees (`group::repo` identity) |

## Primary worktree

The original checkout, starred (★) in the sidebar; for quick questions/small
edits. Real tasks go in task worktrees so agents work in parallel without
sharing a checkout.

## Creation flow (⌘N)

- ⌘N creates a worktree **instantly** — no dialog. Sidebar row appears with
  italic "*my new worktree*", a Codex chat tab opens (default tool), bottom
  bar shows "Branch will be named automatically, or click here to manually
  name". Verified live (`12-new-worktree-modal.png`).
- ⌘⇧N ("New Worktree From…") opens the picker flow: choose base branch or PR.
- Names are auto-generated `sc-<adjective>-<noun>-<hex4>` (observed:
  `sc-coupled-fermion-707b`, `sc-entangled-phonon-391b`); usage scoring
  (`worktree_usage.usage_score`, `last_interaction_at`) drives "activity" sort.
- Branch names derive from the first meaningful prompt:
  `feat/docs-system`, `fix/api-download-redirect`, `refactor/sidebar-state`
  (project `branch_prefix` optional).
- **Task directories live under `~/.superconductor/worktrees/<project>/<name>`**
  (verified via `sc worktree delete` output).

## Target branch & status

"Target branch" = what status/review/PR compare against. Project-level
defaults (default branch, default target); auto-detect: origin/HEAD →
origin/main → origin/master → local main/master. `auto_fast_forward` +
`auto_fast_forward_default_branch` keep worktrees current. Sidebar rows can
show local commit/push position before PR state
(`sidebar_show_commit_status`).

## Sections (sidebar)

- Manual sections (e.g. built-in **Pinned**) + **filter sections**:
  `--rule review=review_required`, checks status, agent state, run state,
  `--branch-glob 'release/*'`; all rules must match; priority order; manual
  assignment beats filters. CLI: `sc section create/edit/assign/…`.
- Row context menu (`13-worktree-context-menu.png`): Run ⌘R · Run setup ·
  Rename feature label… · Rename current branch · Hide worktree · New section ▸ ·
  Copy path ⌥⌘C · Open in Finder · Open in Terminal · **Delete worktree** ·
  Pin worktree · Move to section ▸.

## Lifecycle & cleanup

1. Create (⌘N / `sc worktree create` — blank vs task-bearing forms with
   `--provider/--prompt/--background/--skip-setup-scripts`).
2. Optional **setup scripts** run (auto, can run in background, output
   auto-clearable); run scripts exposed in right panel (Setup | Run) + Run ▷
   titlebar button.
3. Agent works; diff tracked against target (Changes/Review/Checks panels).
4. Ship: commit/push/PR (GitHub `gh` / GitLab `glab`; `pr_ops`,
   "Open Pull Request or Merge Request" prompt templates embedded in binary).
5. Cleanup removes worktree + runtime state; local branch deletion
   default-on, remote-tracking default-off; pre/post-cleanup hooks (failing
   pre-cleanup keeps the worktree; repo can define teardown but not deletion
   guards). `sc worktree delete` refuses primary/dirty/unpushed without
   `--force`. Mistaken deletions have a troubleshooting recovery path.

## Project settings (`projects[]` in settings.json)

`main_repo_path`, `ui_state` (collapsed, color hue, show_external_worktrees,
show_main_worktree, worktree_usage scores), `settings`: default_branch,
default_target_branch, branch_prefix, default_tool, worktree_root,
setup_scripts, run_scripts(+entries), pre_cleanup/teardown/post_cleanup,
auto_run_setup_scripts, auto_init_submodules, output auto-clear toggles,
`ai_tier` overrides. Repo-level `superconductor/config.json` complements
project settings (referenced by the run-script empty state).
