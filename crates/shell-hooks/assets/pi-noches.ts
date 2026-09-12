import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { request } from "node:http";
import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";

export default function (pi: ExtensionAPI) {
  const contextPath = process.env.NOCHES_HOOK_CONTEXT;
  if (!contextPath) return;
  const context = JSON.parse(readFileSync(contextPath, "utf8"));
  const { binding, token, socket } = context;

  // Turn admission needs a host acknowledgment, not a successful queue write.
  // No prompts, transcript contents or model credentials leave this process.
  async function emit(event_name: string, ctx: ExtensionContext): Promise<boolean> {
    const id = ctx.sessionManager.getSessionId();
    if (binding.provider_session_id !== id || ctx.cwd !== binding.worktree_path) return false;
    const body = JSON.stringify({
      binding, event_id: randomUUID(), event_name,
      payload: { session_id: id, cwd: ctx.cwd, is_idle: ctx.isIdle() && !ctx.hasPendingMessages() },
    });
    return new Promise<boolean>((resolve) => {
      const req = request({ socketPath: socket, path: "/hooks", method: "POST", headers: {
        Authorization: `Bearer ${token}`, "Content-Type": "application/json",
        "Content-Length": Buffer.byteLength(body),
      } }, (response) => {
        response.resume();
        response.on("end", () => resolve(response.statusCode === 200));
        response.on("error", () => resolve(false));
      });
      req.setTimeout(2000, () => req.destroy());
      req.on("error", () => resolve(false));
      req.end(body);
    });
  }

  pi.on("session_start", async (_event, ctx) => { await emit("session_start", ctx); });
  pi.on("input", async (_event, ctx) => {
    if (await emit("prompt_submitted", ctx)) return { action: "continue" };
    ctx.ui.notify("Native session handoff is pending or disconnected. Return to Zeron before sending.", "warning");
    return { action: "handled" };
  });
  pi.on("before_agent_start", async (_event, ctx) => {
    if (!await emit("before_agent_start", ctx)) await ctx.abort();
  });
  pi.on("agent_start", async (_event, ctx) => {
    if (!await emit("agent_start", ctx)) await ctx.abort();
  });
  pi.on("agent_settled", async (_event, ctx) => { await emit("agent_settled", ctx); });
  pi.on("tool_execution_start", async (_event, ctx) => { await emit("tool_execution_start", ctx); });
  pi.on("ui_prompt_start", async (_event, ctx) => { await emit("ui_prompt_start", ctx); });
  pi.on("ui_prompt_end", async (_event, ctx) => { await emit("ui_prompt_end", ctx); });
  pi.on("session_shutdown", async (_event, ctx) => { await emit("session_shutdown", ctx); });
  for (const name of ["session_before_switch", "session_before_fork"] as const) {
    pi.on(name, async (_event, ctx) => {
      ctx.ui.notify("Open another Zeron pane to start or resume a different session.", "warning");
      return { cancel: true };
    });
  }
}
