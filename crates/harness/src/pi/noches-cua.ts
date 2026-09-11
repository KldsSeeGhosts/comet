// The Noches engine owns permissions, sessions and the driver. This adapter
// only forwards requests and converts complete MCP results into Pi results.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { truncateHead, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { connect } from "node:net";
import { StringDecoder } from "node:string_decoder";
import { mkdtempSync, writeFileSync, chmodSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

interface BridgeResult {
  content?: Array<{ type: string; text?: string; data?: string; mimeType?: string }>;
  structuredContent?: unknown;
  isError?: boolean;
}

const DESKTOP_POINTER_ACTIONS = new Set([
  "click", "double_click", "right_click", "drag", "mouse_button_down",
  "mouse_drag", "mouse_button_up", "scroll", "move_cursor",
]);

export function bridgeArgs(action: string, args: Record<string, unknown>): Record<string, unknown> {
  const forwarded = { ...args };
  delete forwarded.allow_user_input_disruption;
  const target = args.target as { kind?: unknown } | undefined;
  const desktopPointer = DESKTOP_POINTER_ACTIONS.has(action)
    && (target?.kind === "desktop" || args.scope === "desktop");
  const foreground = args.delivery_mode === "foreground";
  if ((desktopPointer || foreground) && args.allow_user_input_disruption !== true) {
    throw new Error(
      "This route can move the user's real pointer or change keyboard focus. " +
      "Ordinary window-scoped pointer actions use the synthetic agent cursor and do not move the user's pointer; " +
      "only scope=desktop with explicit disruption approval may use the real seat. " +
      "Use browser or background window actions instead. Only after the user explicitly allows disruption, " +
      "retry with allow_user_input_disruption=true.",
    );
  }
  return forwarded;
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
  const rawTextParts: string[] = [];
  const images: Array<{type: "image"; data: string; mimeType: string}> = [];
  for (const part of result.content ?? []) {
    if (part.type === "text" && typeof part.text === "string") rawTextParts.push(part.text);
    if (part.type === "image" && part.data) images.push({ type: "image", data: part.data, mimeType: part.mimeType ?? "image/png" });
  }

  // Preserve full raw text for the secure truncation file
  const rawAllParts = [...rawTextParts];
  if (result.structuredContent != null) {
    rawAllParts.push(JSON.stringify(result.structuredContent));
  }
  const rawFullText = rawAllParts.join("\n\n") || "Computer-use call returned no text.";

  const text: string[] = [];
  const tools = action === "help" && result.structuredContent && typeof result.structuredContent === "object"
    ? (result.structuredContent as { tools?: Array<{ name?: unknown }> }).tools
    : undefined;

  if (tools) {
    const names = tools.map(tool => tool.name).filter((name): name is string => typeof name === "string");
    text.length = 0;
    text.push(
      `Available actions: ${names.join(", ")}\n` +
      `Ordinary window-scoped pointer actions use the synthetic agent cursor and do not move the user's pointer; ` +
      `only scope=desktop with explicit disruption approval may use the real seat.\n` +
      `Use describe with args.name or args.names for schemas. Do not parse help output with shell commands.`
    );
  } else if (action === "get_window_state") {
    const structuredObj = result.structuredContent && typeof result.structuredContent === "object" && !Array.isArray(result.structuredContent)
      ? (result.structuredContent as Record<string, unknown>)
      : undefined;
    const hasElements = Array.isArray(structuredObj?.elements);
    const isError = Boolean(result.isError);

    if (isError) {
      // Keep refusal/error text and structured content
      text.push(...rawTextParts);
      if (result.structuredContent != null) {
        text.push(JSON.stringify(result.structuredContent));
      }
    } else if (hasElements) {
      // Shape get_window_state results so the model does not receive the same accessibility tree twice.
      // Suppress the redundant driver markdown text and exclude tree_markdown from the serialized structured copy.
      const { tree_markdown: _discard, ...structuredWithoutTreeMd } = structuredObj!;
      text.push(JSON.stringify(structuredWithoutTreeMd));
    } else {
      text.push(...rawTextParts);
      if (result.structuredContent != null) {
        text.push(JSON.stringify(result.structuredContent));
      }
    }
  } else {
    // Preserve other actions' result fidelity completely
    text.push(...rawTextParts);
    if (result.structuredContent != null) {
      text.push(JSON.stringify(result.structuredContent));
    }
  }

  const fullText = text.join("\n\n") || "Computer-use call returned no text.";
  const truncated = truncateHead(fullText, { maxLines: DEFAULT_MAX_LINES, maxBytes: DEFAULT_MAX_BYTES });
  let visible = truncated.content;
  if (truncated.truncated) {
    const dir = mkdtempSync(join(tmpdir(), "noches-cua-result-"));
    chmodSync(dir, 0o700);
    const path = join(dir, "result.txt");
    writeFileSync(path, rawFullText, { mode: 0o600, flag: "wx" });
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
    description: "Use the engine host's desktop through Noches. Pass action and args. Ordinary window-scoped pointer actions use the synthetic agent cursor and do not move the user's pointer; only scope=desktop with explicit disruption approval may use the real seat. Browser actions work in the background. 'help' returns a compact action list; 'describe' with args.name or args.names returns schemas.",
    promptSnippet: "noches_cua: Inspect and control the engine host's desktop with native Noches approval and cancellation.",
    promptGuidelines: [
      "Use noches_cua for desktop automation inside Noches; the direct cua tool is disabled. Do NOT shell out to hyprctl, wmctrl, or xdotool for window/app control - use noches_cua (list_windows, get_window_state, bring_to_front, set_window_frame) instead. Reserve bash for non-desktop work.",
      "Do not call help as a first step when the needed action is named here. Use describe with args.name or args.names for only the schemas you need. Never read, crop, or parse screenshots with shell or Python commands.",
      "For browser work, use list_windows, get_browser_state, browser_prepare when requested by a structured refusal, then browser_navigate/browser_click/browser_type. Browser screenshots come from get_browser_state with include_screenshot=true and do not foreground the browser. Do not use desktop screenshots or pixel clicks for tabs, URLs, or web content.",
      "Prefer browser actions, then background window actions with an exact pid/window_id. Observe only when the action needs a fresh semantic ref or element token. Verify the final requested state once. Do not take a screenshot after every action unless the result is unknown.",
      "Ordinary window-scoped pointer actions use the synthetic agent cursor and do not move the user's pointer. Only scope=desktop (or target.kind='desktop') pointer actions and delivery_mode='foreground' can commandeer the user's real pointer or keyboard focus on Wayland. They require explicit disruption approval from the user and allow_user_input_disruption=true. Never infer disruption permission from ordinary computer-use approval.",
      "If the user allows disruptive desktop input, call get_screen_size first. Select a returned display_id, capture it with get_desktop_state, use output-local native pixels, and echo layout_token as expected_layout. Do not add monitor origins or apply scale again.",
      "For isolated background keyboard input into a child window: first click the child (via its element token or coordinates), then call type_text with only pid/window_id/text and press_key with only pid/window_id/key. Targeted text/key arguments are rejected; keyboard input goes through the focused child window.",
      "In a browser, focus the omnibox with hotkey (keys:[\"ctrl\",\"l\"] plus the browser pid/window_id); alternatively press_key with key:\"l\", modifiers:[\"ctrl\"], pid/window_id. This is the robust way to focus the omnibox before typing a URL; do not rely on bare typing reaching it.",
      "For routine Chromium automation prefer browser_prepare with an isolated_new profile. Use existing_profile only when signed-in or user-profile state is required, because it exposes the user's logged-in profile data.",
      "Keep keyboard actions window-scoped with an exact pid/window_id. Do not reuse desktop PNG coordinates for window-scoped actions. If exact background targeting is unavailable, report that limitation instead of silently falling back to global input.",
      "One noches_cua approval covers the host for the whole session, including foreground and desktop delivery across turns. A denial lasts until the turn ends; do not retry the same action after one.",
      "noches_cua sessions and cleanup belong to the engine. Do not set session authority fields or call session lifecycle tools. The desktop lease and driver release when a turn finishes and re-acquire silently on the next turn; only the approval persists.",
      "If noches_cua reports cancellation or unknown delivery, do not repeat the action automatically. Previously delivered input cannot be undone.",
      "On Hyprland 0.55+ `hyprctl dispatch <name> <args>` is removed; it is now a Lua shorthand for `hl.dispatch(...)`. If a task genuinely needs a compositor action noches_cua lacks (e.g. moving a window to a workspace), run `hyprctl eval 'hl.dispatch(hl.dsp.<fn>({ ... }))'` - never the positional form.",
    ],
    parameters: Type.Object({
      action: Type.String({ description: "Action name, help, or describe." }),
      args: Type.Optional(Type.Record(Type.String(), Type.Any())),
    }),
    async execute(_id, params, signal) {
      return toolResult(await callBridge(params.action, bridgeArgs(params.action, params.args ?? {}), signal), params.action);
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
