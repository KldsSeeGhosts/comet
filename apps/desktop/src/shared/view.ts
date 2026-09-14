//! Frontend view logic — a TypeScript port of `crates/proto/src/view.rs` and
//! the bits of `crates/ui/src/composer.rs` every viewport needs. These
//! derivations must not diverge between the GPUI and Electron surfaces; each
//! function names its Rust source.

import type {
  Chat,
  ChatIndicator,
  MessagePart,
  Session,
  SessionMessageEntry,
  Space,
  ToolCall,
  UserInputQuestion,
} from "./types";
import { chatUnseen } from "./types";

// ---------------------------------------------------------------------------
// Connection + status
// ---------------------------------------------------------------------------

export type ConnectionStatus =
  | { kind: "connecting" }
  | { kind: "ready" }
  | { kind: "failed"; error: string };

export type Indicator = "none" | "working" | "awaitingInput" | "errored";

/// `Working`/`AwaitingInput` sessions older than this read as dead — the
/// crashed-backend guard. Mirrors view.rs `SESSION_STALE_MS`.
export const SESSION_STALE_MS = 45_000;

/** Staleness-checked indicator for a session row. Port of `effective_indicator`. */
export function effectiveIndicator(
  session: Session | undefined,
  now: Date,
): Indicator {
  if (!session) return "none";
  switch (session.status) {
    case "idle":
      return "none";
    case "errored":
      return "errored";
    case "working":
    case "awaitingInput": {
      const ageMs = now.getTime() - new Date(session.updatedAt).getTime();
      if (ageMs > SESSION_STALE_MS) return "none";
      return session.status === "working" ? "working" : "awaitingInput";
    }
  }
}

/** Full display status for a chat row. Port of `display_status`. */
export function displayStatus(
  chat: Chat,
  session: Session | undefined,
  now: Date,
): ChatIndicator {
  const live =
    session && effectiveIndicator(session, now) !== "none"
      ? session
      : undefined;
  switch (live?.status) {
    case "working":
      return "working";
    case "awaitingInput":
      return "awaitingInput";
    case "errored":
      // Errored rows only hold their error state while unseen; once read the
      // row goes quiet rather than pretending the run finished cleanly.
      return chatUnseen(chat) ? "errored" : "idle";
    default:
      return chatUnseen(chat) ? "completed" : "idle";
  }
}

// ---------------------------------------------------------------------------
// Sort orders (total, stable across devices)
// ---------------------------------------------------------------------------

const ts = (c: Chat) => c.lastMessageAt ?? c.createdAt;

/** Sidebar order: activity desc, createdAt desc, id. Port of `sort_chats`. */
export function sortChats(chats: Chat[]): Chat[] {
  return [...chats].sort((a, b) => {
    const byActivity = ts(b).localeCompare(ts(a));
    if (byActivity !== 0) return byActivity;
    const byCreated = b.createdAt.localeCompare(a.createdAt);
    if (byCreated !== 0) return byCreated;
    return a.id.localeCompare(b.id);
  });
}

/** Spaces list order: creation order, id tiebreak. Port of `sort_spaces`. */
export function sortSpaces(spaces: Space[]): Space[] {
  return [...spaces].sort(
    (a, b) =>
      a.createdAt.localeCompare(b.createdAt) || a.id.localeCompare(b.id),
  );
}

// ---------------------------------------------------------------------------
// Sidebar grouping
// ---------------------------------------------------------------------------

/** Project label for a chat: basename of cwd, or "No project". */
export function projectLabel(cwd: string | null | undefined): string {
  const trimmed = cwd?.trim();
  if (!trimmed || trimmed === "~" || trimmed === "~/") return "No project";
  const base = trimmed
    .replace(/[/\\]+$/, "")
    .split(/[/\\]/)
    .pop();
  return base && base.length > 0 ? base : trimmed;
}

export interface ChatGroup {
  label: string;
  chats: Chat[];
}

/** Group chats by project label, preserving incoming recency order. */
export function groupChats(chats: Chat[]): ChatGroup[] {
  const groups: ChatGroup[] = [];
  for (const chat of chats) {
    const label = projectLabel(chat.cwd);
    const existing = groups.find((g) => g.label === label);
    if (existing) existing.chats.push(chat);
    else groups.push({ label, chats: [chat] });
  }
  return groups;
}

/** "project · branch" sub-line. Port of `chat_location`. */
export function chatLocation(chat: Chat): string | null {
  const project = chat.cwd?.trim() ? projectLabel(chat.cwd) : null;
  const branch = chat.branch?.trim() || null;
  if (project && branch) return `${project} · ${branch}`;
  return project ?? branch;
}

/** Compact relative time — "now", "5m", "3h", "2d", "1w". */
export function formatTimeAgo(then: Date, now: Date): string {
  const s = Math.max(0, Math.floor((now.getTime() - then.getTime()) / 1000));
  if (s < 60) return "now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 7) return `${d}d`;
  const w = Math.floor(d / 7);
  if (w < 5) return `${w}w`;
  const mo = Math.floor(d / 30);
  if (mo < 12) return `${mo}mo`;
  return `${Math.floor(d / 365)}y`;
}

// ---------------------------------------------------------------------------
// Tool summaries — ports of `single_line` / `tool_chip_content` /
// `tool_group_summary` so both viewports name a tool identically.
// ---------------------------------------------------------------------------

export function singleLine(text: string): string {
  return text.split(/\s+/).filter(Boolean).join(" ");
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null;
}

/** Chip label + one-line detail for a tool call. Port of `tool_chip_content`. */
export function toolChipContent(call: ToolCall): [string, string] {
  let label: string;
  let detail: string;
  switch (call.kind) {
    case "exec":
      label = "Run";
      detail = call.command;
      break;
    case "readFile":
      label = "Read";
      detail = call.path;
      break;
    case "writeFile":
      label = "Write";
      detail = call.path;
      break;
    case "editFile":
      label = "Edit";
      detail = call.path;
      break;
    case "applyPatch":
      label = "Patch";
      detail = call.path ?? "workspace";
      break;
    case "search":
      label = "Search";
      detail = call.path ? `${call.pattern} in ${call.path}` : call.pattern;
      break;
    case "glob":
      label = "Glob";
      detail = call.pattern;
      break;
    case "webFetch":
      label = "Fetch";
      detail = call.url;
      break;
    case "webSearch":
      label = "Web";
      detail = call.query;
      break;
    case "todo": {
      label = "Todo";
      const done = call.items.filter((i) => i.done).length;
      detail = `${done}/${call.items.length} done`;
      break;
    }
    case "mcp":
      label = "MCP";
      detail = `${call.server} · ${call.tool}`;
      break;
    case "unknown":
      if (call.name === "noches_cua") {
        label = "Computer use";
        const action =
          isRecord(call.input) && typeof call.input.action === "string"
            ? call.input.action
            : "";
        detail = action.replace(/_/g, " ");
      } else if (call.name.startsWith("Agent: ")) {
        label = "Agent";
        detail = call.name.slice("Agent: ".length);
      } else if (call.name === "Agent") {
        label = "Agent";
        detail = "";
      } else {
        label = "Tool";
        detail = call.name;
      }
      break;
  }
  return [label, singleLine(detail)];
}

function plural(n: number, one: string, many: string): string {
  return n === 1 ? `${n} ${one}` : `${n} ${many}`;
}

/** "Ran 3 commands · edited 2 files". Port of `tool_group_summary`. */
export function toolGroupSummary(tools: [ToolCall, boolean][]): string {
  let commands = 0;
  const edited: string[] = [];
  let reads = 0;
  let searches = 0;
  let fetches = 0;
  let todos = 0;
  let other = 0;
  let failed = 0;
  for (const [call, isError] of tools) {
    if (isError) failed++;
    switch (call.kind) {
      case "exec":
        commands++;
        break;
      case "writeFile":
      case "editFile":
        if (!edited.includes(call.path)) edited.push(call.path);
        break;
      case "applyPatch": {
        const p = call.path ?? "patch";
        if (!edited.includes(p)) edited.push(p);
        break;
      }
      case "readFile":
        reads++;
        break;
      case "search":
      case "glob":
      case "webSearch":
        searches++;
        break;
      case "webFetch":
        fetches++;
        break;
      case "todo":
        todos++;
        break;
      case "mcp":
      case "unknown":
        other++;
        break;
    }
  }
  const segments: string[] = [];
  if (commands > 0) segments.push(`ran ${plural(commands, "command", "commands")}`);
  if (edited.length > 0) segments.push(`edited ${plural(edited.length, "file", "files")}`);
  if (reads > 0) segments.push(`read ${plural(reads, "file", "files")}`);
  if (searches > 0) segments.push(`searched ${plural(searches, "time", "times")}`);
  if (fetches > 0) segments.push(`fetched ${plural(fetches, "page", "pages")}`);
  if (todos > 0) segments.push("updated todos");
  if (other > 0) segments.push(`called ${plural(other, "tool", "tools")}`);
  if (segments.length === 0) segments.push(plural(tools.length, "tool", "tools"));
  if (failed > 0) segments.push(`${failed} failed`);
  const summary = segments.join(" · ");
  return summary.charAt(0).toUpperCase() + summary.slice(1);
}

// ---------------------------------------------------------------------------
// Composer (ports of composer.rs helpers)
// ---------------------------------------------------------------------------

export type SendButtonMode = "send" | "queue" | "stop";

/** Port of `send_button_mode`: idle→send, live+text→queue, live+empty→stop. */
export function sendButtonMode(runLive: boolean, hasText: boolean): SendButtonMode {
  if (!runLive) return "send";
  return hasText ? "queue" : "stop";
}

/** First unresolved input request on the trailing assistant entry. Port of
 *  `pending_input_request` — assistant-entry-scoped so a steer appended behind
 *  the streaming entry doesn't hide the question. */
export function pendingInputRequest(
  transcript: SessionMessageEntry[],
): { requestId: string; questions: UserInputQuestion[] } | null {
  for (let i = transcript.length - 1; i >= 0; i--) {
    const entry = transcript[i];
    if (entry.role !== "assistant") continue;
    for (let j = entry.parts.length - 1; j >= 0; j--) {
      const part: MessagePart = entry.parts[j];
      if (part.kind === "input" && !part.resolved) {
        return { requestId: part.requestId, questions: part.questions };
      }
    }
    // Only the LAST assistant entry carries the live question; older ones are
    // superseded regardless of resolution state.
    return null;
  }
  return null;
}
