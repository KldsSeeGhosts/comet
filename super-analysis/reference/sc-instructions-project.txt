Project instructions

Script configuration:
- Read effective setup, named run/stop, teardown, and user-owned cleanup hooks with
  `sc project scripts --json`; the response reports each value's config or app
  settings source. Use `sc project scripts check --json` for an offline view
  and validation of the current Git worktree's `.superconductor/config.json`.
- Start the default Run entry with `sc project scripts run` (falling back to the
  first entry when none is marked default), or name one
  explicitly with `sc project scripts run NAME`. Use `status` to read the same
  runtime state shown in the app and `stop [NAME]` to stop one or all running
  scripts. These lifecycle commands require the running app.
- Every `set` or `unset` requires `--scope user|repo`. User scope updates
  personal App Project Settings through the running app and never changes repo
  files. Repo-scope commands operate offline, preserve unknown JSON keys, and
  are the only commands that create or atomically replace the config file.
- Each named Run entry may own Stop commands. Configure them with
  `sc project scripts set stop NAME COMMAND... --scope user|repo`; they run on
  explicit stops before the Run process is terminated. The Run entry must already
  exist in the chosen scope; `unset stop NAME --scope user|repo` clears them.
- A linked worktree with no config consults the main-repo config. Creating any
  worktree config shadows that entire main-repo file; `check` reports either case.
- An absent setup, run, or teardown key falls back to App Project Settings.
  Removing the last named run entry leaves `run: []`, which explicitly disables
  run-script fallback; `unset run` removes the key and restores fallback.
- Pre/post-cleanup hooks run automatically during deletion and remain user-owned
  App Project Settings, so they accept only `--scope user`.
- Editing a legacy string-array run config migrates only that field to named
  run entries.
