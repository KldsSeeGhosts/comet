Review instructions

- Use documented `sc worktree review-*` commands by default for app-managed
  review threads. Use another mechanism only when the human user explicitly
  requests that specific mechanism.
- Read in-app comments with `sc worktree review-list --json` and
  `sc worktree review-get <comment_id> --json`.
- Inspect review scope with `sc worktree review-checklist --json` and
  `sc worktree diff-summary --json` when needed.
- Add with `sc worktree review-add`, respond with `sc worktree review-reply`,
  and update state with `sc worktree review-set-status`. Mutate a thread only
  when the human user's requested outcome requires it; use `sc help worktree`
  for exact syntax.
- `review-add` attributes an omitted `--provider`/`--author` to the calling
  super.engineering session's provider. Pass one explicitly only to override it.
- Review threads are super.engineering-managed state; do not replace them with
  guessed state from source files or provider-specific APIs.
