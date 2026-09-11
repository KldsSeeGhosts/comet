// The Noches engine owns permissions, sessions and the driver. This adapter
// only forwards requests and converts complete MCP results into Pi results.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { truncateHead, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { connect } from "node:net";
import { StringDecoder } from "node:string_decoder";
import { mkdtempSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

interface BridgeResult {
  content?: Array<{ type: string; text?: string; data?: string; mimeType?: string }>;
  structuredContent?: unknown;
  isError?: boolean;
}

export function callBridge(action: string, args: Record<string, unknown>, signal?: AbortSignal): Promise<BridgeResult> {
  const path = process.env.NOCHES_CUA_SOCKET;
  if (!path) return Promise.reject(new Error("Managed computer use is unavailable on this host. The legacy cua tool remains disabled inside Noches."));
  if (signal?.aborted) return Promise.reject(new Error("Computer use cancelled"));
  const request = JSON.stringify({ action, args }) + "\n";
  if (Buffer.byteLength(request) > 1024 * 1024) return Promise.reject(new Error("Computer-use request exceeds size limit"));
  return new Promise((resolve, reject) => {
    const socket = connect(path);
    const decoder = new StringDecoder("utf8");
    let buffer = "";
    let bytes = 0;
    let settled = false;
    const finish = (error?: Error, result?: BridgeResult) => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener("abort", abort);
      socket.destroy();
      if (error) reject(error); else resolve(result!);
    };
    const abort = () => finish(new Error("Computer use cancelled. Already delivered input cannot be undone."));
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) { abort(); return; }
    socket.setTimeout(125_000, () => finish(new Error("Computer-use bridge timed out; no action was retried")));
    socket.on("error", (error) => finish(error));
    socket.on("end", () => finish(new Error("Computer-use bridge closed before returning a result")));
    socket.on("close", () => finish(new Error("Computer-use bridge disconnected")));
    socket.on("connect", () => socket.write(request));
    socket.on("data", (chunk: Buffer) => {
      bytes += chunk.length;
      if (bytes > 32 * 1024 * 1024) { finish(new Error("Computer-use response exceeds size limit")); return; }
      buffer += decoder.write(chunk);
      const newline = buffer.indexOf("\n");
      if (newline < 0) return;
      try {
        const result = JSON.parse(buffer.slice(0, newline));
        if (!result || typeof result !== "object" || Array.isArray(result)) throw new Error("expected a result object");
        finish(undefined, result);
      } catch (error) {
        finish(new Error(`Invalid computer-use response: ${error}`));
      }
    });
  });
}

export function toolResult(result: BridgeResult, action: string) {
  const text: string[] = [];
  const images: Array<{type: "image"; data: string; mimeType: string}> = [];
  for (const part of result.content ?? []) {
    if (part.type === "text" && typeof part.text === "string") text.push(part.text);
    if (part.type === "image" && part.data) images.push({ type: "image", data: part.data, mimeType: part.mimeType ?? "image/png" });
  }
  if (result.structuredContent != null) text.push(JSON.stringify(result.structuredContent));
  const fullText = text.join("\n\n") || "Computer-use call returned no text.";
  const truncated = truncateHead(fullText, { maxLines: DEFAULT_MAX_LINES, maxBytes: DEFAULT_MAX_BYTES });
  let visible = truncated.content;
  if (truncated.truncated) {
    const dir = mkdtempSync(join(tmpdir(), "noches-cua-result-"));
    const path = join(dir, "result.txt");
    writeFileSync(path, fullText, { mode: 0o600, flag: "wx" });
    visible += `\n\n[Output truncated. Full text and structured result: ${path}]`;
  }
  return {
    content: [{type: "text" as const, text: visible}, ...images],
    details: { action, structuredContent: result.structuredContent ?? null, driverIsError: result.isError === true },
  };
}

export default function (pi: ExtensionAPI) {
  const deactivateLegacy = () => {
    pi.setActiveTools(pi.getActiveTools().filter((name) => name !== "cua"));
  };
  pi.on("session_start", deactivateLegacy);
  pi.on("before_agent_start", deactivateLegacy);
  pi.on("tool_call", (event) => {
    if (event.toolName === "cua") return {
      block: true,
      reason: "The direct cua tool is disabled inside Noches. Use noches_cua for engine-managed computer use.",
    };
  });
  pi.registerTool({
    name: "noches_cua",
    label: "Computer use",
    description: "Use the engine host's desktop through Noches. Pass action and args. 'help' lists supported tools; 'describe' with args.name returns a schema. Text output is capped at 50KB or 2000 lines; full truncated results are saved to a private file.",
    promptSnippet: "noches_cua: Inspect and control the engine host's desktop with native Noches approval and cancellation.",
    promptGuidelines: [
      "Use noches_cua for desktop automation inside Noches; the direct cua tool is disabled. Do NOT shell out to hyprctl, wmctrl, or xdotool for window/app control - use noches_cua (list_windows, get_window_state, bring_to_front, set_window_frame) instead. Reserve bash for non-desktop work.",
      "Use noches_cua help and describe to inspect current tool schemas. Observe a specific window before acting, use fresh element tokens, and verify the postcondition after each action.",
      "For desktop scope, call get_screen_size first. When the driver returns displays, select a returned display_id and pass it to get_desktop_state. Use that PNG's output-local native pixels with target={kind:'desktop',display_id:<returned name>}; do not add monitor origins or apply scale again. Echo the observation's layout_token as expected_layout, then verify on the same display. The primary alias is not necessarily the focused monitor.",
      "Keep keyboard actions window-scoped with an exact pid/window_id. Do not reuse desktop PNG coordinates for window-scoped actions. If a named display or layout check is refused, re-observe; never substitute another display or silently fall back to global input.",
      "One noches_cua approval covers the host for the whole turn, including foreground and desktop delivery. A denial lasts until the turn ends; do not retry the same action after one.",
      "noches_cua sessions and cleanup belong to the engine. Do not set session authority fields or call session lifecycle tools. Approvals expire when the turn finishes.",
      "If noches_cua reports cancellation or unknown delivery, do not repeat the action automatically. Previously delivered input cannot be undone.",
      "On Hyprland 0.55+ `hyprctl dispatch <name> <args>` is removed; it is now a Lua shorthand for `hl.dispatch(...)`. If a task genuinely needs a compositor action noches_cua lacks (e.g. moving a window to a workspace), run `hyprctl eval 'hl.dispatch(hl.dsp.<fn>({ ... }))'` - never the positional form.",
    ],
    parameters: Type.Object({
      action: Type.String({ description: "Action name, help, or describe." }),
      args: Type.Optional(Type.Record(Type.String(), Type.Any())),
    }),
    async execute(_id, params, signal) {
      return toolResult(await callBridge(params.action, params.args ?? {}, signal), params.action);
    },
  });
  // Pi's documented result hook preserves error details and images, whereas
  // throwing from execute would discard the structured driver refusal.
  pi.on("tool_result", (event) => {
    if (event.toolName === "noches_cua" && (event.details as {driverIsError?: boolean})?.driverIsError) {
      return { isError: true };
    }
  });
}
