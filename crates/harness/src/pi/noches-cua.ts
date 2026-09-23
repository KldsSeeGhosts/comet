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

const NON_DISRUPTIVE_POLICY = "Only non-disruptive computer use is allowed. Physical focus, mouse and keyboard must remain untouched. Desktop capture is read-only. Supported window input uses background delivery with an exact pid/window_id; unsupported background routes must refuse, never fall back to foreground.";
const BLOCKED_ACTIONS = new Set([
  "bring_to_front", "launch_app", "kill_app", "set_window_frame",
  "mouse_button_down", "mouse_drag", "mouse_button_up", "invoke_menu", "clipboard_write",
]);
const WINDOW_INPUT_ACTIONS = new Set([
  "click", "double_click", "right_click", "drag", "scroll", "type_text", "press_key", "hotkey",
]);

export type CuaOutcome =
  | "success" | "refused" | "error" | "partial" | "unknown" | "unverifiable" | "cancelled";

const REFUSAL_STATUSES = new Set(["refused", "denied", "rejected", "blocked", "forbidden"]);
const ERROR_STATUSES = new Set(["error", "failed", "failure"]);
const PARTIAL_STATUSES = new Set(["partial", "partially_delivered", "incomplete"]);
const UNKNOWN_STATUSES = new Set(["unknown", "unobserved", "indeterminate"]);
const UNVERIFIABLE_STATUSES = new Set(["unverifiable", "unverified"]);
const CANCELLED_STATUSES = new Set(["cancelled", "canceled"]);

function objectRecord(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

function structuredRecord(result: BridgeResult): Record<string, unknown> | undefined {
  return objectRecord(result.structuredContent);
}

function normalizedToken(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value.toLowerCase() : undefined;
}

function firstString(...values: unknown[]): string | undefined {
  return values.find((value): value is string => typeof value === "string" && value.length > 0);
}

/**
 * A structured refusal outranks the generic MCP execution-error flag: a
 * refusal reported as `isError` is still an exact refusal, not a generic
 * error. `effect: "refused"` is the outcome-only variant of the same signal.
 */
function refusedOutcome(structured: Record<string, unknown> | undefined): boolean {
  const status = normalizedToken(structured?.status);
  const effect = normalizedToken(structured?.effect);
  return (status !== undefined && REFUSAL_STATUSES.has(status))
    || structured?.refused === true
    || (effect !== undefined && REFUSAL_STATUSES.has(effect));
}

/**
 * Classify the driver's structured outcome. A refusal and an explicit error
 * are failed executions; partial, unknown and unverifiable deliveries are
 * uncertain outcomes that stay non-errors only while the driver did not set
 * the execution-error flag.
 */
export function classifyOutcome(result: BridgeResult): CuaOutcome {
  const structured = structuredRecord(result);
  const status = normalizedToken(structured?.status);
  const effect = normalizedToken(structured?.effect);
  const driverIsError = result.isError === true;
  if (refusedOutcome(structured)) return "refused";
  if ((status && ERROR_STATUSES.has(status)) || (effect && ERROR_STATUSES.has(effect))) return "error";
  if ((status && CANCELLED_STATUSES.has(status)) || (effect && CANCELLED_STATUSES.has(effect))) return "cancelled";
  const uncertain = status && PARTIAL_STATUSES.has(status) ? "partial" as const
    : status && UNKNOWN_STATUSES.has(status) ? "unknown" as const
    : status && UNVERIFIABLE_STATUSES.has(status) ? "unverifiable" as const
    : effect && UNVERIFIABLE_STATUSES.has(effect) ? "unverifiable" as const
    : effect === "partial" ? "partial" as const
    : effect === "unknown" ? "unknown" as const
    : undefined;
  if (uncertain) return driverIsError ? "error" : uncertain;
  return driverIsError ? "error" : "success";
}

interface RefusalFields {
  code?: string;
  message?: string;
  reason?: string;
  nextAction?: string;
  approvals: string[];
}

/**
 * Read the driver's refusal payload. The audited export nests it as
 * `refusal: {code, message, detail: {next_action, reason, supported_strategies}}`
 * under `status: "refused"`; older drivers may put the same fields at the top
 * level. Both layouts are read without rewriting the nested record.
 */
function refusalFields(structured: Record<string, unknown> | undefined): RefusalFields {
  const refusal = objectRecord(structured?.refusal);
  const detail = objectRecord(refusal?.detail);
  const approvals: string[] = [];
  if (detail?.approval_required === true || structured?.approval_required === true) {
    approvals.push("approval_required");
  }
  if (detail?.browser_consent_required === true || structured?.browser_consent_required === true) {
    approvals.push("browser_consent_required");
  }
  const required = firstString(detail?.required_approval, structured?.required_approval);
  if (required) approvals.push(`required_approval=${required}`);
  const strategies = detail?.supported_strategies ?? structured?.supported_strategies;
  if (Array.isArray(strategies)) {
    const names = strategies.filter((name): name is string => typeof name === "string" && name.length > 0);
    if (names.length) approvals.push(`supported_strategies=${names.join(",")}`);
  }
  return {
    code: firstString(refusal?.code, structured?.reason_code, structured?.code),
    message: firstString(refusal?.message, structured?.message),
    reason: firstString(detail?.reason, structured?.reason, structured?.refusal_reason),
    nextAction: firstString(detail?.next_action, structured?.next_action),
    approvals,
  };
}

function refusalSummary(structured: Record<string, unknown> | undefined): string {
  const { code, message, reason, nextAction, approvals } = refusalFields(structured);
  const head = code ? `Computer-use refused (${code})` : "Computer-use refused";
  const parts = [`${head}: ${message ?? "the action was not executed."}`];
  if (reason && reason !== code) parts.push(`Reason: ${reason}.`);
  if (nextAction === "browser_prepare") {
    parts.push("Set up the browser and DevTools manually, then use get_browser_state. Automatic browser preparation is disabled.");
  } else if (nextAction) {
    // Preserve the original recommendation in the structured evidence, not as
    // adapter instructions that might suggest activating the physical seat.
    parts.push("Driver suggestions do not override the non-disruptive policy.");
  }
  if (approvals.length) parts.push(`Approval: ${approvals.join(", ")}.`);
  return parts.join(" ");
}

export function bridgeArgs(action: string, args: Record<string, unknown>): Record<string, unknown> {
  const forwarded = { ...args };
  const deny = (reason: string): never => { throw new Error(`${reason}. ${NON_DISRUPTIVE_POLICY}`); };
  const token = (value: unknown) => typeof value === "string" ? value.trim().toLowerCase() : undefined;
  if ("allow_user_input_disruption" in args) deny("Disruption overrides are forbidden, regardless of session approval");
  if ("delivery_mode" in args) {
    if (token(args.delivery_mode) !== "background") deny("delivery_mode must be background; foreground and unknown modes are forbidden");
    forwarded.delivery_mode = "background";
  }
  if ("scope" in args) {
    const scope = token(args.scope);
    if (scope !== "window" && scope !== "desktop") deny("scope must be window or desktop");
    forwarded.scope = scope;
  }
  const desktopCapture = action === "get_desktop_state" || action === "get_screen_size";
  if ("target" in args) {
    const target = objectRecord(args.target);
    if (!target) deny("target must be an object");
    const kind = token(target!.kind);
    if (kind !== "window" && kind !== "desktop") deny("target.kind must be window or desktop");
    if (kind === "desktop" && !desktopCapture) deny("Desktop input is forbidden, including keyboard input");
    forwarded.target = { ...target, kind };
  }
  if (!desktopCapture && (forwarded.scope === "desktop" || "display_id" in args || "expected_layout" in args)) {
    deny("Desktop routes are restricted to read-only get_desktop_state/get_screen_size");
  }
  if (action === "browser_prepare") deny("browser_prepare is disabled: the driver has no guaranteed attach-only, non-disruptive route. Set up the browser and DevTools manually, then use get_browser_state. Automatic setup and launch are forbidden");
  if (BLOCKED_ACTIONS.has(action)) deny(`${action} is forbidden: no verified non-disruptive background route`);
  if (action === "browser_dialog" && args.action !== "inspect") deny("Only browser_dialog action=inspect is allowed; resolving native browser dialogs can change physical focus. Handle the dialog manually");
  if (WINDOW_INPUT_ACTIONS.has(action) || action === "set_value" || action === "move_cursor") {
    for (const key of ["pid", "window_id"]) {
      if (!Number.isSafeInteger(args[key]) || (args[key] as number) <= 0) deny(`${action} requires an exact positive integer ${key} for background input`);
    }
    if ("target" in args) deny("Window input requires top-level pid/window_id, not an alternate target");
    if (WINDOW_INPUT_ACTIONS.has(action)) forwarded.delivery_mode = "background";
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

  const structuredObj = structuredRecord(result);
  const outcome = classifyOutcome(result);
  // The adapter marks a structured refusal as a failed tool execution while
  // preserving the driver's payload; uncertain deliveries keep their outcome.
  const driverIsError = outcome === "refused" || outcome === "error" || result.isError === true;

  // Human-readable text for the truncation export. Structured data goes to a
  // separate valid-JSON file so a truncated result stays machine-readable.
  const rawText = rawTextParts.join("\n\n") || "Computer-use call returned no text.";

  const text: string[] = [];
  const tools = action === "help"
    ? (structuredObj as { tools?: Array<{ name?: unknown }> } | undefined)?.tools
    : undefined;

  if (tools) {
    const names = tools.map(tool => tool.name).filter((name): name is string => typeof name === "string");
    text.push(
      `Available actions: ${names.join(", ")}\n` +
      `${NON_DISRUPTIVE_POLICY}\n` +
      `Use describe with args.name or args.names for schemas. Do not parse help output with shell commands.`
    );
  } else if (action === "get_window_state") {
    const hasElements = Array.isArray(structuredObj?.elements);
    if (driverIsError || !hasElements) {
      // Keep refusal/error text and structured content.
      text.push(...rawTextParts);
      if (result.structuredContent != null) {
        text.push(JSON.stringify(result.structuredContent));
      }
    } else {
      // Shape get_window_state results so the model does not receive the same accessibility tree twice.
      // Suppress the redundant driver markdown text and exclude tree_markdown from the serialized structured copy.
      const { tree_markdown: _discard, ...structuredWithoutTreeMd } = structuredObj!;
      text.push(JSON.stringify(structuredWithoutTreeMd));
    }
  } else {
    // Preserve other actions' result fidelity completely.
    text.push(...rawTextParts);
    if (result.structuredContent != null) {
      text.push(JSON.stringify(result.structuredContent));
    }
  }

  if (outcome === "refused") {
    text.unshift(refusalSummary(structuredObj));
  } else if (outcome === "partial" || outcome === "unknown" || outcome === "unverifiable") {
    // Uncertain delivery is not a failure; say so instead of letting the
    // model read a successful call. Never replay uncertain input.
    text.unshift(
      `Computer-use outcome: ${outcome}. Delivery was not verified; do not replay it. Inspect the current state before proceeding.`
    );
  }

  const fullText = text.join("\n\n") || "Computer-use call returned no text.";
  const truncated = truncateHead(fullText, { maxLines: DEFAULT_MAX_LINES, maxBytes: DEFAULT_MAX_BYTES });
  let visible = truncated.content;
  if (truncated.truncated) {
    const dir = mkdtempSync(join(tmpdir(), "noches-cua-result-"));
    chmodSync(dir, 0o700);
    const textPath = join(dir, "result.txt");
    const jsonPath = join(dir, "result.json");
    writeFileSync(textPath, rawText, { mode: 0o600, flag: "wx" });
    writeFileSync(jsonPath, JSON.stringify({ action, structuredContent: result.structuredContent ?? null }, null, 2), { mode: 0o600, flag: "wx" });
    visible += `\n\n[Output truncated. Full text: ${textPath}; structured result: ${jsonPath}]`;
  }
  return {
    content: [{type: "text" as const, text: visible}, ...images],
    details: { action, structuredContent: result.structuredContent ?? null, driverIsError, driverOutcome: outcome },
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
    description: `Use the engine host's desktop through Noches. Pass action and args. ${NON_DISRUPTIVE_POLICY} 'help' returns a compact action list; 'describe' with args.name or args.names returns schemas.`,
    promptSnippet: "noches_cua: Inspect and control the engine host's desktop with native Noches approval and cancellation.",
    promptGuidelines: [
      "Use noches_cua for desktop automation inside Noches; the direct cua tool is disabled. Never bypass its policy through shell commands, compositor IPC or another automation tool. Reserve bash for non-desktop work.",
      "Use describe with args.name or args.names for only the schemas you need. Never read, crop, or parse screenshots with shell or Python commands.",
      NON_DISRUPTIVE_POLICY,
      "Browser preparation is disabled because the driver has no guaranteed attach-only route. Ask the user to set up the browser and DevTools manually, then use get_browser_state and typed browser actions. Do not follow driver suggestions to run browser_prepare, launch a browser or switch to foreground delivery.",
      "Browser screenshots come from get_browser_state with include_screenshot=true. Use browser_navigate/browser_click/browser_type for web content, not desktop input or omnibox shortcuts.",
      "Prefer browser actions, then supported background window actions with an exact pid/window_id. Use fresh semantic refs or element tokens when required. Verify the final requested state. Background delivery is not proof of success; preserve driver refusals and report unsupported native targets.",
      "For isolated background keyboard input into a child window, first click the child via its element token or coordinates with an exact pid/window_id. Then call type_text with pid/window_id/text or press_key with pid/window_id/key. The engine forces background delivery. Never fall back to the physical keyboard.",
      "Stateful mouse holds, activation, launch, app termination, window rearrangement, menu invocation and clipboard writes are disabled. A single background drag remains available when the driver supports it. Desktop screenshots and clipboard reads do not authorize desktop input.",
      "One approval covers only non-disruptive inspection and supported background input across turns. Disruption overrides are forbidden. A denial lasts until the turn ends; do not retry the same action after one.",
      "noches_cua sessions and cleanup belong to the engine. Do not set session authority fields or call session lifecycle tools. The lease and driver release when a turn finishes; only approval persists.",
      "If noches_cua reports cancellation or unknown delivery, do not repeat the action automatically. Previously delivered input cannot be undone.",
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
