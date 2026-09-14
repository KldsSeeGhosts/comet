//! Wire types — the TypeScript mirror of `crates/proto` + `crates/doc` shapes
//! that cross the RPC boundary. Field names match the wire (camelCase);
//! variants match serde tags. When a proto type changes, change it here.

// ---------------------------------------------------------------------------
// Harnesses / models (proto::agent)
// ---------------------------------------------------------------------------

export type HarnessId =
  | "claude-code"
  | "codex"
  | "cursor"
  | "devin"
  | "grok"
  | "hermes"
  | "pi"
  | "opencode"
  | "mock";

export type ReasoningLevel =
  | "off"
  | "minimal"
  | "low"
  | "medium"
  | "high"
  | "xhigh"
  | "max"
  | "ultra"
  | "ultracode"
  | "ultrathink";

export type SandboxLevel = "read-only" | "workspace-write" | "danger-full-access";

export type SteeringMode = "step-boundary" | "turn-boundary";

export interface Model {
  id: string;
  label: string;
  description?: string;
  reasoningLevels: ReasoningLevel[];
  options: ModelOption[];
}

export interface ModelOption {
  id: string;
  label: string;
  choices: { id: string; label: string }[];
  defaultChoice: string;
}

/// `ListHarnesses` reply row (engine `HarnessDescriptor`).
export interface HarnessDescriptor {
  id: HarnessId;
  name: string;
  supportsSteering: boolean;
  steeringMode: SteeringMode;
  reasoningLevels: ReasoningLevel[];
  installed: boolean;
  enabled?: boolean;
}

/** Descriptor gate for the non-interrupting mid-turn Steer action. */
export function steersMidTurn(d: HarnessDescriptor): boolean {
  return d.supportsSteering && d.steeringMode === "step-boundary";
}

export interface ChatConfig {
  harness: HarnessId;
  model: string | null;
  reasoning: ReasoningLevel | null;
  modelOptions: Record<string, unknown>;
  sandbox: SandboxLevel;
}

// ---------------------------------------------------------------------------
// Workspace entities (proto::entities)
// ---------------------------------------------------------------------------

export interface Space {
  id: string;
  deviceId: string;
  path: string;
  name?: string;
  gitDetected: boolean;
  gitCheckedAt?: string;
  checkoutId?: string;
  createdAt: string;
}

/** proto::entities::Device — `WatchDevices` row. */
export interface Device {
  id: string;
  name: string;
  platform: string;
  lastSeenAt?: string | null;
  createdAt?: string | null;
  version?: string | null;
  capabilities?: string[];
}

/** proto::entities::RepoRef — `ListRefs` row (branch + checkout state). */
export interface RepoRef {
  name: string;
  current: boolean;
  worktreePath?: string | null;
}

/** proto::entities::Worktree — `CreateWorktree` reply. */
export interface Worktree {
  repoPath: string;
  path: string;
  branch: string;
  name: string;
  checkoutId?: string | null;
}

/** proto::entities::TransferProgress — `WatchTransfers` row. */
export interface TransferProgress {
  uploadId: string;
  fileName: string;
  done: number;
  total: number;
}

export function spaceDisplayName(space: Space): string {
  const name = space.name?.trim();
  if (name) return name;
  const trimmed = space.path.replace(/[/\\]+$/, "");
  const base = trimmed.split(/[/\\]/).pop();
  return base && base.length > 0 ? base : space.path;
}

export interface ConversationSourceContext {
  checkoutId: string;
  repoRoot: string;
  cwd: string;
  branch: string;
  headSha?: string;
  observedAt: string;
}

export interface Chat {
  id: string;
  deviceId: string;
  title: string | null;
  archived: boolean;
  cwd: string | null;
  branch: string | null;
  checkoutId: string | null;
  sourceContext?: ConversationSourceContext;
  config: ChatConfig | null;
  lastMessagePreview: string | null;
  lastMessageAt: string | null;
  createdAt: string;
  harnessSessionId?: string;
  harnessSessionCwd?: string;
  spaceId?: string;
  lastSeenAt?: string;
  roomGen?: number;
}

export function chatUnseen(chat: Chat): boolean {
  if (!chat.lastMessageAt) return false;
  if (!chat.lastSeenAt) return true;
  return chat.lastMessageAt > chat.lastSeenAt;
}

export type SessionStatus = "idle" | "working" | "awaitingInput" | "errored";

export interface Session {
  chatId: string;
  deviceId: string;
  status: SessionStatus;
  startedAt: string | null;
  updatedAt: string;
}

export type ChatIndicator =
  | "working"
  | "awaitingInput"
  | "errored"
  | "completed"
  | "idle";

// ---------------------------------------------------------------------------
// Engine / device identity (proto::workspace)
// ---------------------------------------------------------------------------

export type WorkspaceScope = "local" | "synced" | "development";

export interface EngineInfo {
  deviceId: string;
  workspaceScope: WorkspaceScope;
  capabilities: string[];
}

// ---------------------------------------------------------------------------
// Transcript (doc::schema + doc::parts)
// ---------------------------------------------------------------------------

export type MessageRole = "user" | "assistant" | "system";

export type MessageStatus = "streaming" | "complete" | "aborted";

export type SubagentStatus = "running" | "done" | "failed";

export interface ToolDiff {
  path: string;
  oldText?: string;
  newText: string;
}

export interface ToolDiffStat {
  path: string;
  additions: number;
  deletions: number;
}

export interface TodoItem {
  text: string;
  done: boolean;
}

export type ToolCall =
  | { kind: "exec"; command: string }
  | { kind: "readFile"; path: string }
  | { kind: "writeFile"; path: string; content?: string }
  | { kind: "editFile"; path: string; oldString?: string; newString?: string }
  | { kind: "applyPatch"; path?: string }
  | { kind: "search"; pattern: string; path?: string }
  | { kind: "glob"; pattern: string }
  | { kind: "webFetch"; url: string; prompt?: string }
  | { kind: "webSearch"; query: string }
  | { kind: "todo"; items: TodoItem[] }
  | { kind: "mcp"; server: string; tool: string; input?: unknown }
  | { kind: "unknown"; name: string; input?: unknown };

export interface UserInputQuestion {
  id: string;
  header: string;
  question: string;
  options: string[];
  multiSelect: boolean;
}

export type MessagePart =
  | { kind: "text"; id: string; text: string }
  | { kind: "reasoning"; id: string; text: string }
  | {
      kind: "tool";
      id: string;
      call: ToolCall;
      isError: boolean;
      resolved: boolean;
      output?: string;
      diff?: ToolDiff;
      outputRef?: string;
      outputBytes?: number;
      diffRef?: string;
      diffStats?: ToolDiffStat[];
      subagentRef?: string;
      subagentStatus?: SubagentStatus;
      subagentTail?: string;
    }
  | {
      kind: "input";
      id: string;
      requestId: string;
      questions: UserInputQuestion[];
      resolved: boolean;
    }
  | { kind: "error"; id: string; message: string };

export interface SessionMessageEntry {
  id: string;
  role: MessageRole;
  parts: MessagePart[];
  /** Epoch millis. */
  createdAt: number;
  deviceId: string;
  status?: MessageStatus;
  continuationOf?: string;
}

// ---------------------------------------------------------------------------
// WatchDocMessages stream items (doc::transcript_delta)
// ---------------------------------------------------------------------------

export interface TranscriptUpsert {
  after: string | null;
  entry: SessionMessageEntry;
}

export interface TextAppend {
  entry: string;
  part: string;
  text: string;
  len: number;
}

export interface ContextComponent {
  kind:
    | "system_prompt"
    | "tools"
    | "skills"
    | "context_files"
    | "messages";
  tokens: number;
}

export interface ContextUsage {
  tokens: number | null;
  window: number | null;
  components: ContextComponent[];
}

/** One `WatchDocMessages` item: `{reset}` XOR `{upsert,append,remove,count}`,
 *  both optionally carrying `contextUsage`. */
export type TranscriptUpdate = {
  contextUsage?: ContextUsage;
} & (
  | { reset: SessionMessageEntry[] }
  | {
      upsert: TranscriptUpsert[];
      append: TextAppend[];
      remove: string[];
      count: number;
    }
);

// ---------------------------------------------------------------------------
// Commands (doc::commands) — the durable command ledger
// ---------------------------------------------------------------------------

export interface WorktreeSpec {
  repoPath: string;
  base: string;
}

export interface RunRequest {
  prompt: string;
  harness?: HarnessId;
  model: string | null;
  reasoning: ReasoningLevel | null;
  modelOptions: Record<string, unknown>;
  cwd: string;
  sandbox: SandboxLevel;
  autoApprove: boolean;
  resume: string | null;
  attachments?: string[];
  worktree?: WorktreeSpec | null;
}

export interface UserInputAnswer {
  questionId: string;
  labels: string[];
}

export type SessionCommandPayload =
  | { kind: "run"; request: RunRequest; messageId: string }
  | { kind: "steer"; prompt: string; messageId?: string | null }
  | { kind: "interrupt" }
  | { kind: "respondInput"; requestId: string; answers: UserInputAnswer[] };

// ---------------------------------------------------------------------------
// Queue (doc::queue)
// ---------------------------------------------------------------------------

export interface QueuedMessage {
  id: string;
  text: string;
  attachments?: unknown[];
  holdForTurnEnd?: boolean;
  createdAt?: number;
  deviceId?: string;
}

// ---------------------------------------------------------------------------
// SessionView (engine::session_view) — active harness/model metadata
// ---------------------------------------------------------------------------

export type SessionOwner = "chat" | "opening" | "cli" | "hydrating" | "recoveryRequired";

export interface SessionView {
  chatId: string;
  owner: SessionOwner;
  provider?: HarnessId;
  nativeSessionId?: string;
  worktreePath?: string;
  model?: string;
  reasoning?: ReasoningLevel;
  terminal?: unknown;
  error?: string | null;
}

// ---------------------------------------------------------------------------
// Connectivity (proto::entities)
// ---------------------------------------------------------------------------

export interface Connectivity {
  state: "online" | "degraded" | "offline" | "disabled" | string;
  retryAtMs: number;
  chats: unknown[];
}
