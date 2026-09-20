Instance instructions

- Use `sc instance current --json` to identify the exact app process and socket
  inherited by the calling agent.
- Use `sc instance list --json` to enumerate every live app in the standard
  local runtime channels. Results include build branch, commit, source worktree,
  executable, PID, process-start token, and local-API socket.
- Use `--socket PATH` with `sc instance current` to verify an explicitly chosen
  app before sending other `sc --socket PATH ...` commands.
- Unmanaged `sc` commands use the current channel's discovery pointer. If that
  pointer is missing or stale, they use the sole live instance in that channel;
  multiple live instances require an explicit `--socket PATH`.
- Treat the manifest and socket as authoritative machine identity. Dock labels
  are a visual aid only; do not infer identity from process titles or icon order.
- Stale manifests are excluded by PID liveness and an exact match with the
  socket's health identity. Do not delete runtime files manually as part of
  discovery.
