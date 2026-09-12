import { execFile } from "node:child_process";
import { readFileSync } from "node:fs";
import { randomUUID } from "node:crypto";

export const NochesHooks = async ({ directory }) => {
  const contextPath = process.env.NOCHES_HOOK_CONTEXT;
  const notify = process.env.NOCHES_NOTIFY;
  if (!contextPath || !notify) return {};
  const binding = JSON.parse(readFileSync(contextPath, "utf8")).binding;
  // OpenCode servers multiplex sessions. Do not guess which session owns this plugin.
  if (!binding.provider_session_id || directory !== binding.worktree_path) return {};
  const allowed = new Set(["session.idle", "session.error", "permission.asked", "permission.replied"]);
  return {
    event: async ({ event }) => {
      if (!allowed.has(event.type) || event.properties?.sessionID !== binding.provider_session_id) return;
      const payload = JSON.stringify({
        ...event.properties, cwd: directory,
        noches_event_id: event.id || randomUUID(),
      });
      await new Promise((resolve) => {
        execFile("/bin/sh", [notify, event.type, payload], { timeout: 30000, maxBuffer: 4096 }, resolve);
      });
    },
  };
};
