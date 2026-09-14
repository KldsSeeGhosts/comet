//! App store — presentation state only. The engine owns truth; everything
//! here is a projection of workspace-doc watches and doc streams, rebuilt on
//! every reconnect.
//!
//! The send path mirrors `crates/ui/src/composer.rs`: staged attachments
//! upload first (queued `pending://` flow when both engines are ≥0.2.12,
//! else the legacy host upload), an optimistic user echo lands immediately,
//! and a failure hands the draft + stash back.

import { create } from "zustand";

import {
  applyTranscriptUpdate,
  TranscriptDesync,
  type TranscriptState,
} from "../../shared/transcript";
import {
  ATTACHMENT_ONLY_TEXT,
  MAX_ATTACHMENT_BYTES,
  ensureExtension,
  isPendingRef,
  mimeByExtension,
  pendingRef,
  retryDelayMs,
  withAttachments,
} from "../../shared/attachments";
import {
  QUEUED_ATTACHMENTS_MIN,
  clampReasoning,
  deviceVersionAtLeast,
  effectiveHarness,
  effectiveModelId,
  effectiveReasoning,
  explicitOptions,
  resolveRunConfig,
  selectedModel,
  traitLadder,
  type DraftConfig,
  type ResolveInput,
} from "../../shared/pickers";
import { capabilities, methods } from "../../shared/protocol";
import type {
  Chat,
  ChatConfig,
  Connectivity,
  Device,
  EngineInfo,
  HarnessDescriptor,
  HarnessId,
  Model,
  QueuedMessage,
  ReasoningLevel,
  RepoRef,
  Session,
  SessionMessageEntry,
  SessionView,
  Space,
  TransferProgress,
  TranscriptUpdate,
} from "../../shared/types";
import { effectiveIndicator } from "../../shared/view";
import * as rpc from "./lib/rpc";
import { readAttachmentImage, uploadAttachment } from "./lib/upload";
import type { EngineStatus } from "../../preload/index";

interface Subscription {
  cancel: () => void;
}

// ---------------------------------------------------------------------------
// Staged attachments + transcript image cache (attachments.rs)
// ---------------------------------------------------------------------------

/** An image staged in the composer, before upload. */
export interface StagedAttachment {
  id: string;
  /** File name with a type-matching extension (ensureExtension). */
  name: string;
  mime: string;
  /** Whole-file base64. */
  base64: string;
  /** Binary byte length. */
  size: number;
  /** data: URL for the thumb/lightbox (built once at stage time). */
  dataUrl: string;
  /** 0..1 while a send uploads it (staged strip shows progress). */
  progress?: number;
  error?: string;
}

export type AttachmentSnapshot =
  | { kind: "loading" }
  | { kind: "loaded"; name: string; dataUrl: string }
  | { kind: "error"; retryInMs: number };

type CacheEntry =
  | { kind: "loading"; attempts: number }
  | { kind: "loaded"; name: string; dataUrl: string; bytes: number; lastUsed: number }
  | { kind: "error"; attempts: number; at: number };

/// Byte budget for retained encoded images (IMAGE_CACHE_BUDGET_BYTES).
const IMAGE_CACHE_BUDGET_BYTES = 64 * 1024 * 1024;

const attachmentCache = new Map<string, CacheEntry>();
let cacheTick = 0;
let cacheLoadedBytes = 0;

const akey = (deviceId: string, path: string) => `${deviceId}${path}`;

function storeLoaded(deviceId: string, path: string, name: string, dataUrl: string, bytes: number) {
  cacheTick += 1;
  const prev = attachmentCache.get(akey(deviceId, path));
  if (prev?.kind === "loaded") cacheLoadedBytes -= prev.bytes;
  attachmentCache.set(akey(deviceId, path), {
    kind: "loaded",
    name,
    dataUrl,
    bytes,
    lastUsed: cacheTick,
  });
  cacheLoadedBytes += bytes;
  while (cacheLoadedBytes > IMAGE_CACHE_BUDGET_BYTES) {
    let oldestKey: string | null = null;
    let oldestTick = Infinity;
    for (const [k, e] of attachmentCache) {
      if (k === akey(deviceId, path) || e.kind !== "loaded") continue;
      if (e.lastUsed < oldestTick) {
        oldestTick = e.lastUsed;
        oldestKey = k;
      }
    }
    if (oldestKey === null) break;
    const evicted = attachmentCache.get(oldestKey);
    if (evicted?.kind === "loaded") cacheLoadedBytes -= evicted.bytes;
    attachmentCache.delete(oldestKey);
  }
}

function storeError(deviceId: string, path: string) {
  const prev = attachmentCache.get(akey(deviceId, path));
  const attempts =
    prev?.kind === "loading"
      ? prev.attempts + 1
      : prev?.kind === "error"
        ? prev.attempts
        : 1;
  attachmentCache.set(akey(deviceId, path), {
    kind: "error",
    attempts,
    at: Date.now(),
  });
}

/** Claim the load for a source: true ⇒ caller fetches now (begin_load). */
function beginLoad(deviceId: string, path: string): boolean {
  const k = akey(deviceId, path);
  const entry = attachmentCache.get(k);
  if (!entry) {
    attachmentCache.set(k, { kind: "loading", attempts: 0 });
    return true;
  }
  if (
    entry.kind === "error" &&
    Date.now() - entry.at >= retryDelayMs(entry.attempts - 1)
  ) {
    attachmentCache.set(k, { kind: "loading", attempts: entry.attempts });
    return true;
  }
  return false;
}

/** The uploadId fragment a committed upload's basename starts with
 *  (`{id8}-{name}` per the engine's Uploads::pending_target). */
function uploadAliasId8(path: string): string | null {
  const base = path.split("/").pop() ?? "";
  if (base.length < 9 || base[8] !== "-") return null;
  const id8 = base.slice(0, 8);
  return /^[a-zA-Z0-9]+$/.test(id8) ? id8 : null;
}

const aliasKey = (deviceId: string, id8: string) =>
  akey(deviceId, `upload-alias://${id8}`);

/** `attachment_snapshot` — what a render pass sees for one (deviceId, path),
 *  including the upload-alias fallback for queued-send refs rewritten to the
 *  host's absolute path. */
function attachmentSnapshot(deviceId: string, path: string): AttachmentSnapshot {
  const entry = attachmentCache.get(akey(deviceId, path));
  switch (entry?.kind) {
    case "loaded":
      entry.lastUsed = ++cacheTick;
      return { kind: "loaded", name: entry.name, dataUrl: entry.dataUrl };
    case "loading":
      return { kind: "loading" };
    case "error":
      return {
        kind: "error",
        retryInMs: Math.max(
          0,
          retryDelayMs(entry.attempts - 1) - (Date.now() - entry.at),
        ),
      };
    default: {
      const id8 = uploadAliasId8(path);
      if (id8) {
        const alias = attachmentCache.get(aliasKey(deviceId, id8));
        if (alias?.kind === "loaded") {
          storeLoaded(deviceId, path, alias.name, alias.dataUrl, alias.bytes);
          return { kind: "loaded", name: alias.name, dataUrl: alias.dataUrl };
        }
      }
      return { kind: "loading" };
    }
  }
}

/** Seed the cache after a successful upload so the just-sent bubble's
 *  thumbnails render from local bytes instead of a round-trip. */
function seedAttachment(deviceId: string, path: string, att: StagedAttachment) {
  storeLoaded(deviceId, path, att.name, att.dataUrl, att.size);
}

/** Seed under the upload identity: the host rewrites `pending://{id}/{name}`
 *  to `{its uploads}/{id8}-{name}` — the alias keeps the thumbnail on the
 *  already-local bytes through that rewrite. */
function seedAttachmentAlias(deviceId: string, uploadId: string, att: StagedAttachment) {
  const id8 = uploadId.slice(0, 8);
  const [dev, path] = aliasKey(deviceId, id8).split("");
  storeLoaded(dev, path, att.name, att.dataUrl, att.size);
}

// ---------------------------------------------------------------------------
// Composer defaults (zeron `zeron.composer.defaults:v1` → localStorage)
// ---------------------------------------------------------------------------

interface ComposerDefaults {
  harness?: HarnessId;
  reasoning?: ReasoningLevel;
  /** harness → last-used model. */
  models?: Partial<Record<HarnessId, { id: string; label: string }>>;
}

const DEFAULTS_KEY = "noches.composer.defaults:v1";

function loadDefaults(): ComposerDefaults {
  try {
    const raw = localStorage.getItem(DEFAULTS_KEY);
    return raw ? (JSON.parse(raw) as ComposerDefaults) : {};
  } catch {
    return {};
  }
}

function saveDefaults(d: ComposerDefaults) {
  try {
    localStorage.setItem(DEFAULTS_KEY, JSON.stringify(d));
  } catch {
    /* private mode / quota — defaults are best-effort */
  }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/** Composer key for the new-chat canvas (GPUI uses "" — no chat row). */
export const CANVAS_KEY = "";

export interface CanvasDraft {
  spaceId: string | null;
  config: DraftConfig;
  /** The picked ref row — needed at send for its worktreePath. */
  ref: RepoRef | null;
}

interface PendingSend {
  messageId: string;
  started: number;
}

/// `UNDELIVERED_GRACE_MS` (state.rs): past it the send surfaces as failed.
const UNDELIVERED_GRACE_MS = 120_000;

export interface AppState {
  conn: EngineStatus;
  engineInfo: EngineInfo | null;
  endpoint: string;

  spaces: Space[];
  chats: Chat[];
  sessions: Record<string, Session>;
  devices: Record<string, Device>;
  harnesses: HarnessDescriptor[];
  connectivity: Connectivity | null;
  transfers: TransferProgress[];

  /** harness → its model catalog (fetched lazily per effective harness). */
  models: Partial<Record<HarnessId, Model[]>>;
  modelErrors: Partial<Record<HarnessId, string>>;

  selectedSpaceId: string | null;
  selectedChatId: string | null;
  /** Non-null while the new-chat canvas is up (selectedChatId === null). */
  canvas: CanvasDraft | null;

  /** chatId → transcript projection. `version` bumps per applied frame. */
  transcripts: Record<string, TranscriptState & { version: number }>;
  transcriptErrors: Record<string, string | null>;
  queues: Record<string, QueuedMessage[]>;
  sessionViews: Record<string, SessionView>;

  /** Optimistic user echoes per chat (dropped when the doc carries the id). */
  echoes: Record<string, SessionMessageEntry[]>;
  /** Sends in flight, keyed by chat — the "Sending…/Queued" overlay feed. */
  pendingSends: Record<string, PendingSend>;

  /** Composer-local state. */
  drafts: Record<string, string>;
  staged: Record<string, StagedAttachment[]>;
  sending: boolean;
  sendError: string | null;
  /** composerDefaults — sticky last-used picks. */
  defaults: ComposerDefaults;

  /** Bumps when the module-level attachment cache changes. */
  attachmentRev: number;
  lightbox: { name: string; src: string } | null;

  /** Ticked so staleness-gated indicators decay without traffic. */
  now: number;

  boot: () => void;
  retry: () => void;
  selectChat: (chatId: string) => void;
  selectSpace: (spaceId: string | null) => void;
  openCanvas: (spaceId: string | null) => void;
  setCanvasSpace: (spaceId: string | null) => void;
  setCanvasRef: (ref: RepoRef | null, checkout: DraftConfig["checkout"]) => void;
  setDraft: (key: string, text: string) => void;
  stageFiles: (
    key: string,
    files: { name: string; base64: string; size: number; mime?: string }[],
  ) => void;
  pickImages: (key: string) => Promise<void>;
  unstage: (key: string, id: string) => void;
  send: () => Promise<void>;
  interrupt: () => Promise<void>;
  steerQueued: (id: string) => Promise<void>;
  sendQueuedNow: (id: string) => Promise<void>;
  removeQueued: (id: string) => Promise<void>;
  respondInput: (
    requestId: string,
    answers: { questionId: string; labels: string[] }[],
  ) => Promise<void>;

  /** Pickers — the effective/resolved view of the run config. */
  resolveInput: (chatId: string | null) => ResolveInput;
  pickHarness: (harness: HarnessId) => void;
  pickModel: (modelId: string) => void;
  pickReasoning: (level: ReasoningLevel) => void;
  pickOption: (optionId: string, choiceId: string, isDefault: boolean) => void;
  pickBranch: (ref: RepoRef) => Promise<void>;
  ensureModels: (harness: HarnessId | undefined, deviceId?: string) => void;

  /** Transcript attachment thumbs. */
  attachmentFor: (chatId: string, path: string) => AttachmentSnapshot;
  openLightbox: (name: string, src: string) => void;
  closeLightbox: () => void;
}

// Subscriptions live outside the store: they are transport handles, not state.
let workspaceSubs: Subscription[] = [];
let docSubs = new Map<string, Subscription[]>();
let booted = false;
const modelFetches = new Set<string>();
const attachmentLoads = new Set<string>();
const attachmentRetries = new Map<string, ReturnType<typeof setTimeout>>();

const DEV_ENDPOINT_NOTE = "ws://127.0.0.1:27655";

export const useApp = create<AppState>((set, get) => ({
  conn: { kind: "connecting" },
  engineInfo: null,
  endpoint: DEV_ENDPOINT_NOTE,

  spaces: [],
  chats: [],
  sessions: {},
  devices: {},
  harnesses: [],
  connectivity: null,
  transfers: [],
  models: {},
  modelErrors: {},

  selectedSpaceId: null,
  selectedChatId: null,
  canvas: null,

  transcripts: {},
  transcriptErrors: {},
  queues: {},
  sessionViews: {},
  echoes: {},
  pendingSends: {},

  drafts: {},
  staged: {},
  sending: false,
  sendError: null,
  defaults: loadDefaults(),

  attachmentRev: 0,
  lightbox: null,

  now: Date.now(),

  boot: () => {
    if (booted) return;
    booted = true;

    void window.noches.getStatus().then((conn) => {
      set({ conn });
      if (conn.kind === "ready") openWorkspaceWatches(set, get);
    });
    window.noches.onStatus((conn) => {
      const was = get().conn;
      set({ conn });
      if (conn.kind === "ready" && was.kind !== "ready") {
        openWorkspaceWatches(set, get);
        const selected = get().selectedChatId;
        if (selected) openDocWatches(selected, set, get);
      }
      if (conn.kind !== "ready") {
        closeWorkspaceWatches();
      }
    });

    // Staleness tick: Working/AwaitingInput rows go quiet after 45s without a
    // session heartbeat even when no new frames arrive.
    setInterval(() => set({ now: Date.now() }), 15_000);
  },

  retry: () => {
    set({ conn: { kind: "connecting" } });
    void window.noches.retry();
  },

  selectChat: (chatId) => {
    if (get().selectedChatId === chatId) return;
    set({ selectedChatId: chatId, canvas: null, sendError: null });
    openDocWatches(chatId, set, get);
    void rpc.markChatSeen(chatId).catch(() => {});
    void rpc
      .getSessionView(chatId)
      .then((view) =>
        set((s) => ({ sessionViews: { ...s.sessionViews, [chatId]: view } })),
      )
      .catch(() => {});
    const chat = get().chats.find((c) => c.id === chatId);
    get().ensureModels(chat?.config?.harness, chat?.deviceId);
  },

  selectSpace: (spaceId) => set({ selectedSpaceId: spaceId }),

  openCanvas: (spaceId) => {
    const fallback =
      spaceId ??
      get().selectedSpaceId ??
      get().spaces.find((s) => s.deviceId === get().engineInfo?.deviceId)?.id ??
      get().spaces[0]?.id ??
      null;
    set({
      selectedChatId: null,
      canvas: {
        spaceId: fallback,
        config: { checkout: { kind: "local" } },
        ref: null,
      },
      sendError: null,
    });
    get().ensureModels(undefined);
  },

  setCanvasSpace: (spaceId) => {
    const canvas = get().canvas;
    if (!canvas) return;
    set({
      canvas: { spaceId, config: { checkout: { kind: "local" } }, ref: null },
    });
    get().ensureModels(undefined);
  },

  setCanvasRef: (ref, checkout) => {
    const canvas = get().canvas;
    if (!canvas) return;
    set({
      canvas: {
        ...canvas,
        ref,
        config: { ...canvas.config, branch: ref?.name, checkout },
      },
    });
  },

  setDraft: (key, text) =>
    set((s) => ({ drafts: { ...s.drafts, [key]: text } })),

  stageFiles: (key, files) => {
    const staged = [...(get().staged[key] ?? [])];
    const errors: string[] = [];
    for (const file of files) {
      const mime = file.mime ?? mimeByExtension(file.name) ?? "";
      if (!mime.startsWith("image/")) {
        errors.push(`${file.name} is not a supported image.`);
        continue;
      }
      if (file.size > MAX_ATTACHMENT_BYTES) {
        errors.push(`${file.name} is too large (24 MB max).`);
        continue;
      }
      const name = ensureExtension(file.name, mime);
      staged.push({
        id: crypto.randomUUID(),
        name,
        mime,
        base64: file.base64,
        size: file.size,
        dataUrl: `data:${mime};base64,${file.base64}`,
      });
    }
    set((s) => ({
      staged: { ...s.staged, [key]: staged },
      sendError: errors.length > 0 ? errors.join(" ") : s.sendError,
    }));
  },

  pickImages: async (key) => {
    const files = await window.noches.pickImages();
    if (files.length > 0) get().stageFiles(key, files);
  },

  unstage: (key, id) =>
    set((s) => ({
      staged: {
        ...s.staged,
        [key]: (s.staged[key] ?? []).filter((a) => a.id !== id),
      },
    })),

  // ---- send ---------------------------------------------------------------
  //
  // Port of composer.rs `send`: snapshot-and-clear attachments, optimistic
  // echo (non-queue sends), queued pending:// flow when both engines
  // understand it, else the legacy host upload; QueueCommand carries
  // `transfers` for the queued flow.

  send: async () => {
    const s = get();
    const canvas = s.canvas;
    const isCanvas = !s.selectedChatId;
    const chatId = s.selectedChatId ?? crypto.randomUUID();
    const key = s.selectedChatId ?? CANVAS_KEY;
    const text = (s.drafts[key] ?? "").trim();
    const staged = [...(s.staged[key] ?? [])];
    if (!text && staged.length === 0) return;

    const chat = s.chats.find((c) => c.id === chatId);
    const space = canvas
      ? s.spaces.find((sp) => sp.id === canvas.spaceId)
      : s.spaces.find((sp) => sp.id === chat?.spaceId);

    const live = !isCanvas && runLive(s.sessions, s.now, chatId);
    const queue = live;

    // Capability gate (composer.rs): queue rows on old engines can't carry
    // attachments; queue itself needs message-queue-v1.
    if (queue) {
      const cap = staged.length === 0
        ? capabilities.MessageQueueV1
        : capabilities.MessageQueueAttachmentsV1;
      const engineOk = s.engineInfo?.capabilities.includes(cap) ?? false;
      if (!engineOk || !deviceSupports(s, chat?.deviceId, cap)) {
        set({
          sendError:
            "Update the chat's engine to queue messages during a response.",
        });
        return;
      }
    }

    const resolved = resolveRunConfig(s.resolveInput(s.selectedChatId));

    // The PROJECT fixes the new chat's device + base folder; with no project
    // the local device hosts and the session runs from `~`.
    const localId = s.engineInfo?.deviceId;
    const deviceId = isCanvas
      ? (space?.deviceId ?? localId ?? "local")
      : (chat?.deviceId ?? localId ?? "local");
    const hostDeviceId = deviceId !== localId ? deviceId : undefined;
    const hostIsRemote = hostDeviceId !== undefined;

    // Snapshot-and-clear NOW (takeAttachments): the strip empties the instant
    // you hit send; a failure hands the files back into the stash.
    set((st) => ({
      staged: { ...st.staged, [key]: [] },
      drafts: { ...st.drafts, [key]: "" },
      sending: true,
      sendError: null,
    }));

    const cleanQueueAttachmentText =
      staged.length === 0 ||
      ((s.engineInfo?.capabilities.includes(
        capabilities.MessageQueueCleanAttachmentTextV1,
      ) ??
        false) &&
        deviceSupports(
          s,
          chat?.deviceId,
          capabilities.MessageQueueCleanAttachmentTextV1,
        ));

    // Queued flow (durable-by-design): stage bytes on the LOCAL engine, queue
    // immediately with pending:// refs; the engine pushes bytes to a remote
    // host afterwards. Queue rows keep the proven host-upload path.
    const queuedFlow =
      !queue &&
      staged.length > 0 &&
      localId !== undefined &&
      deviceVersionAtLeast(s.devices[localId]?.version, QUEUED_ATTACHMENTS_MIN) &&
      (!hostIsRemote ||
        deviceVersionAtLeast(
          s.devices[deviceId]?.version,
          QUEUED_ATTACHMENTS_MIN,
        ));

    const uploadIds = staged.map(() => crypto.randomUUID());
    const echoPaths = staged.map((att, i) =>
      queuedFlow ? pendingRef(uploadIds[i], att.name) : `pending/${att.id}/${att.name}`,
    );
    const echoText = withAttachments(text, echoPaths);

    // Seed the transcript cache under every device key the transcript
    // consults, so the sent bubble's thumbnails never round-trip.
    if (queuedFlow) {
      for (const [i, att] of staged.entries()) {
        seedAttachmentAlias(deviceId, uploadIds[i], att);
        if (localId && localId !== deviceId) {
          seedAttachmentAlias(localId, uploadIds[i], att);
        }
      }
    }
    for (const [i, att] of staged.entries()) {
      seedAttachment(deviceId, echoPaths[i], att);
      if (localId && localId !== deviceId) {
        seedAttachment(localId, echoPaths[i], att);
      }
    }

    // Optimistic echo (client-minted id doubles as the persisted message id,
    // so the doc frame dedups it away). A queued message's echo is the queue
    // panel — never a transcript bubble.
    const messageId = crypto.randomUUID();
    const echo: SessionMessageEntry = {
      id: messageId,
      role: "user",
      parts: [{ kind: "text", id: "t0", text: echoText }],
      createdAt: Date.now(),
      deviceId: "local",
    };
    if (!queue) {
      pushEcho(set, chatId, echo);
      set((st) => ({
        pendingSends: {
          ...st.pendingSends,
          [chatId]: { messageId, started: Date.now() },
        },
      }));
      if (isCanvas) {
        set({ selectedChatId: chatId, canvas: null });
        openDocWatches(chatId, set, get);
      }
    }
    bumpAttachmentRev(set);

    const failRestore = (message: string) => {
      // Failure: red banner, echo removed, prompt back in the draft, staged
      // files back in the stash (merged by id so files staged mid-send
      // survive).
      if (isCanvas) {
        void rpc
          .mutate({ op: "deleteChat", chatId })
          .catch(() => {});
      }
      set((st) => {
        const restoreKey = isCanvas ? CANVAS_KEY : chatId;
        const merged = [...(st.staged[restoreKey] ?? [])];
        for (const att of staged) {
          if (!merged.some((a) => a.id === att.id)) merged.push(att);
        }
        return {
          sending: false,
          sendError: message,
          drafts: { ...st.drafts, [restoreKey]: text },
          staged: { ...st.staged, [restoreKey]: merged },
          pendingSends: dropPendingSend(st.pendingSends, chatId),
          selectedChatId: isCanvas ? null : st.selectedChatId,
          canvas: isCanvas
            ? { spaceId: space?.id ?? null, config: canvas?.config ?? { checkout: { kind: "local" } }, ref: canvas?.ref ?? null }
            : st.canvas,
        };
      });
      removeEcho(set, chatId, messageId);
      bumpAttachmentRev(set);
    };

    try {
      // Attachments stage FIRST — before the chat row or anything else
      // exists (staging is chat-independent, keyed by uploadId).
      let content = text;
      let attachmentPaths: string[] = [];
      const transfers: { uploadId: string; fileName: string }[] = [];

      if (staged.length > 0 && queuedFlow) {
        for (const [i, att] of staged.entries()) {
          try {
            await uploadAttachment({
              uploadId: uploadIds[i],
              fileName: att.name,
              base64: att.base64,
              onProgress: (done) =>
                setStageProgress(set, key, att.id, done / att.size),
            });
          } catch (err) {
            console.warn("local attachment stage failed", att.name, err);
            throw new Error("Couldn't stage the attachment locally.");
          }
          transfers.push({ uploadId: uploadIds[i], fileName: att.name });
        }
        attachmentPaths = echoPaths;
        content = echoText;
      } else if (staged.length > 0) {
        for (const [i, att] of staged.entries()) {
          let path: string;
          try {
            path = await uploadAttachment({
              targetDeviceId: hostDeviceId,
              uploadId: uploadIds[i],
              fileName: att.name,
              base64: att.base64,
              onProgress: (done) =>
                setStageProgress(set, key, att.id, done / att.size),
            });
          } catch (err) {
            console.warn("attachment upload failed", att.name, err);
            throw new Error(
              "Couldn't upload the attachment — the device may be offline.",
            );
          }
          attachmentPaths.push(path);
        }
        const seedDevice = hostDeviceId ?? deviceId;
        for (const [i, att] of staged.entries()) {
          seedAttachment(seedDevice, attachmentPaths[i], att);
          if (seedDevice !== deviceId) {
            seedAttachment(deviceId, attachmentPaths[i], att);
          }
        }
        content = withAttachments(text, attachmentPaths);
        if (!queue) {
          // Refresh the echo in place with the uploaded refs so its
          // thumbnails never flicker.
          removeEcho(set, chatId, messageId);
          pushEcho(set, chatId, {
            ...echo,
            parts: [{ kind: "text", id: "t0", text: content }],
          });
        }
      }

      // Canvas checkout plan → cwd/branch/worktree (composer.rs).
      let cwd = isCanvas
        ? (space?.path ?? "~")
        : (chat?.cwd ?? space?.path ?? "~");
      let chatBranch: string | undefined;
      let runWorktree: { repoPath: string; base: string } | undefined;
      if (isCanvas && space) {
        const ref = canvas?.ref;
        const checkout = canvas?.config.checkout ?? { kind: "local" };
        if (checkout.kind === "newWorktree") {
          chatBranch = ref?.name;
          runWorktree = {
            repoPath: space.path,
            base: ref?.name ?? "HEAD",
          };
        } else if (ref?.worktreePath) {
          cwd = ref.worktreePath;
          chatBranch = ref.name;
        } else {
          chatBranch = ref?.name;
        }
      }

      if (isCanvas) {
        // Best-effort createChat with the picked config — the engine resolves
        // device + cwd from the project row when one is picked (idempotent;
        // the doc host would materialize the chat on first command anyway).
        const config = chatConfigFor(resolved, chat?.config);
        const params: Record<string, unknown> = {
          op: "createChat",
          chatId,
        };
        if (space) params.spaceId = space.id;
        else params.deviceId = deviceId;
        if (canvas?.ref?.worktreePath && canvas?.config.checkout.kind !== "newWorktree") {
          params.cwd = cwd;
        }
        if (chatBranch) params.branch = chatBranch;
        if (config) params.config = config;
        try {
          await rpc.mutate(params);
        } catch (err) {
          console.warn("createChat mutate unavailable; doc host will materialize", err);
        }
      }

      if (queue) {
        // Queue rows keep the prompt free of the internal attachment trailer
        // when both engines speak clean-attachment-text; the host rebuilds
        // the transport when it promotes the row.
        const queueText = !cleanQueueAttachmentText
          ? content
          : text.length === 0 && attachmentPaths.length > 0
            ? ATTACHMENT_ONLY_TEXT
            : text;
        await rpc.call(methods.QueueMessage, {
          chatId,
          text: queueText,
          attachments: attachmentPaths,
          holdForTurnEnd: true,
        });
        set({ sending: false });
        return;
      }

      const command = {
        kind: "run",
        request: {
          prompt: content,
          harness: resolved.harness,
          model: resolved.model ?? null,
          reasoning: resolved.reasoning ?? null,
          modelOptions: resolved.modelOptions,
          cwd,
          sandbox: chat?.config?.sandbox ?? "workspace-write",
          autoApprove: false,
          resume: null,
          attachments: attachmentPaths,
          worktree: runWorktree ?? null,
        },
        messageId,
      };
      const params: Record<string, unknown> = { chatId, command };
      if (transfers.length > 0) params.transfers = transfers;
      await rpc.queueCommandRaw(params);
      set({ sending: false });
    } catch (err) {
      failRestore(err instanceof Error ? err.message : String(err));
    }
  },

  interrupt: async () => {
    const chatId = get().selectedChatId;
    if (!chatId) return;
    await rpc.queueCommand(chatId, { kind: "interrupt" });
  },

  steerQueued: async (id) => {
    const chatId = get().selectedChatId;
    if (chatId) await rpc.steerQueuedNow(chatId, id);
  },

  sendQueuedNow: async (id) => {
    const chatId = get().selectedChatId;
    if (chatId) await rpc.sendQueuedNow(chatId, id);
  },

  removeQueued: async (id) => {
    const chatId = get().selectedChatId;
    if (chatId) await rpc.removeQueued(chatId, id);
  },

  respondInput: async (requestId, answers) => {
    const chatId = get().selectedChatId;
    if (!chatId) return;
    await rpc.queueCommand(chatId, {
      kind: "respondInput",
      requestId,
      answers,
    });
  },

  // ---- pickers -------------------------------------------------------------

  resolveInput: (chatId) => {
    const s = get();
    const chat = chatId ? s.chats.find((c) => c.id === chatId) : undefined;
    const canvasDraft = !chatId ? s.canvas?.config : undefined;
    const harness =
      canvasDraft?.harness ??
      chat?.config?.harness ??
      s.defaults.harness;
    return {
      draft: canvasDraft,
      chat,
      models: harness ? s.models[harness] : undefined,
      harnesses: s.harnesses,
      defaults: {
        harness: s.defaults.harness,
        model: harness ? s.defaults.models?.[harness]?.id : undefined,
        reasoning: s.defaults.reasoning,
      },
    };
  },

  pickHarness: (harness) => {
    const s = get();
    if (s.selectedChatId) {
      // Harness is locked once the chat exists (feature-inventory §1.7).
      return;
    }
    const canvas = s.canvas;
    if (!canvas) return;
    set({
      canvas: { ...canvas, config: { ...canvas.config, harness } },
    });
    s.ensureModels(harness, canvasSpaceDevice(s));
  },

  pickModel: (modelId) => {
    const s = get();
    const chatId = s.selectedChatId;
    if (chatId) {
      updateChatConfig(set, get, chatId, (config) => {
        config.model = modelId;
      });
    } else {
      const canvas = s.canvas;
      if (!canvas) return;
      set({
        canvas: { ...canvas, config: { ...canvas.config, model: modelId } },
      });
      const harness = effectiveHarness(s.resolveInput(null));
      if (harness) {
        const label =
          s.models[harness]?.find((m) => m.id === modelId)?.label ?? modelId;
        const defaults = {
          ...s.defaults,
          models: { ...s.defaults.models, [harness]: { id: modelId, label } },
        };
        set({ defaults });
        saveDefaults(defaults);
      }
    }
  },

  pickReasoning: (level) => {
    const s = get();
    const chatId = s.selectedChatId;
    if (chatId) {
      updateChatConfig(set, get, chatId, (config) => {
        config.reasoning = level;
      });
    } else {
      const canvas = s.canvas;
      if (!canvas) return;
      set({
        canvas: { ...canvas, config: { ...canvas.config, reasoning: level } },
      });
      const defaults = { ...s.defaults, reasoning: level };
      set({ defaults });
      saveDefaults(defaults);
    }
  },

  pickOption: (optionId, choiceId, isDefault) => {
    const s = get();
    const chatId = s.selectedChatId;
    const apply = (options: Record<string, unknown>) => {
      const next = { ...options };
      if (isDefault) delete next[optionId];
      else next[optionId] = choiceId;
      return next;
    };
    if (chatId) {
      updateChatConfig(set, get, chatId, (config) => {
        config.modelOptions = apply(config.modelOptions);
      });
    } else {
      const canvas = s.canvas;
      if (!canvas) return;
      set({
        canvas: {
          ...canvas,
          config: {
            ...canvas.config,
            modelOptions: apply(canvas.config.modelOptions ?? {}),
          },
        },
      });
    }
  },

  /** Branch pick: canvas → draft ref (worktree reuse / new-worktree base);
   *  existing chat → mid-session SwitchRef on the chat's cwd. */
  pickBranch: async (ref) => {
    const s = get();
    const chatId = s.selectedChatId;
    if (!chatId) {
      const canvas = s.canvas;
      if (!canvas) return;
      // A ref materialized as a linked worktree reuses that checkout; a plain
      // branch keeps the space folder (newWorktree rides the checkout toggle).
      const checkout =
        canvas.config.checkout.kind === "newWorktree"
          ? canvas.config.checkout
          : { kind: "local" as const };
      s.setCanvasRef(ref, checkout);
      return;
    }
    const chat = s.chats.find((c) => c.id === chatId);
    const repoPath = chat?.cwd ?? s.spaces.find((sp) => sp.id === chat?.spaceId)?.path;
    if (!repoPath) return;
    try {
      const reply = await rpc.call<{ branch: string }>(methods.SwitchRef, {
        repoPath,
        refName: ref.name,
      });
      // Optimistic branch stamp; the host's HEAD watch reconciles it.
      set((st) => ({
        chats: st.chats.map((c) =>
          c.id === chatId ? { ...c, branch: reply.branch } : c,
        ),
      }));
    } catch (err) {
      set({
        sendError: err instanceof Error ? err.message : String(err),
      });
    }
  },

  ensureModels: (harness, deviceId) => {
    const s = get();
    const eff = harness ?? effectiveHarness(s.resolveInput(s.selectedChatId));
    if (!eff) return;
    if (s.models[eff] || modelFetches.has(eff)) return;
    modelFetches.add(eff);
    const target =
      deviceId && deviceId !== s.engineInfo?.deviceId ? deviceId : undefined;
    rpc
      .listModels(eff, target)
      .then((models) =>
        set((st) => ({
          models: { ...st.models, [eff]: models },
          modelErrors: { ...st.modelErrors, [eff]: undefined },
        })),
      )
      .catch((err) =>
        set((st) => ({
          modelErrors: {
            ...st.modelErrors,
            [eff]: err instanceof Error ? err.message : String(err),
          },
        })),
      )
      .finally(() => modelFetches.delete(eff));
  },

  // ---- transcript attachment thumbs ----------------------------------------

  attachmentFor: (chatId, path) => {
    const s = get();
    const chat = s.chats.find((c) => c.id === chatId);
    const localId = s.engineInfo?.deviceId;
    // Devices that may own the files: the chat's host device plus this one.
    const deviceIds: string[] = [];
    if (chat) deviceIds.push(chat.deviceId);
    if (localId && !deviceIds.includes(localId)) deviceIds.push(localId);

    for (const dev of deviceIds) {
      const snap = attachmentSnapshot(dev, path);
      if (snap.kind === "loaded") return snap;
    }
    let anyLoading = false;
    let minRetry: number | null = null;
    for (const dev of deviceIds) {
      if (beginLoad(dev, path)) {
        attachmentLoads.add(akey(dev, path));
        const target = localId !== dev ? dev : undefined;
        void readAttachmentImage(path, target)
          .then((loaded) => {
            if (loaded) {
              storeLoaded(
                dev,
                path,
                loaded.name,
                `data:${loaded.mimeType};base64,${loaded.base64}`,
                Math.floor((loaded.base64.length * 3) / 4),
              );
            } else {
              storeError(dev, path);
            }
          })
          .catch(() => storeError(dev, path))
          .finally(() => {
            attachmentLoads.delete(akey(dev, path));
            bumpAttachmentRev(set);
          });
      }
      const snap = attachmentSnapshot(dev, path);
      if (snap.kind === "loaded") return snap;
      if (snap.kind === "loading") anyLoading = true;
      if (snap.kind === "error") {
        minRetry =
          minRetry === null ? snap.retryInMs : Math.min(minRetry, snap.retryInMs);
        if (!attachmentRetries.has(akey(dev, path)) && snap.retryInMs < 86_400_000) {
          attachmentRetries.set(
            akey(dev, path),
            setTimeout(() => {
              attachmentRetries.delete(akey(dev, path));
              bumpAttachmentRev(set);
            }, snap.retryInMs + 50),
          );
        }
      }
    }
    if (anyLoading) return { kind: "loading" };
    if (minRetry !== null) return { kind: "error", retryInMs: minRetry };
    return { kind: "error", retryInMs: Number.MAX_SAFE_INTEGER };
  },

  openLightbox: (name, src) => set({ lightbox: { name, src } }),
  closeLightbox: () => set({ lightbox: null }),
}));

// ---------------------------------------------------------------------------
// Derived helpers used by components
// ---------------------------------------------------------------------------

/** Is a run live on this chat right now (staleness-gated)? */
export function runLive(
  sessions: Record<string, Session>,
  now: number,
  chatId: string,
): boolean {
  const indicator = effectiveIndicator(sessions[chatId], new Date(now));
  return indicator === "working" || indicator === "awaitingInput";
}

/** state.rs `device_supports` — the live EngineInfo answers for the
 *  connected engine's device; synced device rows answer for peers. Missing
 *  declarations are conservatively unsupported. */
export function deviceSupports(
  s: Pick<AppState, "engineInfo" | "devices">,
  deviceId: string | undefined,
  capability: string,
): boolean {
  if (!deviceId) return false;
  if (s.engineInfo?.deviceId === deviceId) {
    return s.engineInfo.capabilities.includes(capability);
  }
  return s.devices[deviceId]?.capabilities?.includes(capability) ?? false;
}

/** state.rs `chat_host_supports`. */
export function chatHostSupports(
  s: Pick<AppState, "engineInfo" | "devices" | "chats">,
  chatId: string,
  capability: string,
): boolean {
  const chat = s.chats.find((c) => c.id === chatId);
  return chat ? deviceSupports(s, chat.deviceId, capability) : false;
}

/** state.rs `send_pending` — the send is in flight inside the grace window. */
export function sendPending(
  pendingSends: Record<string, PendingSend>,
  chatId: string,
  now: number,
): boolean {
  const p = pendingSends[chatId];
  return !!p && now - p.started <= UNDELIVERED_GRACE_MS;
}

/** state.rs `send_undelivered` — the send sat unadopted past the grace
 *  window: the explicit failed state. */
export function sendUndelivered(
  pendingSends: Record<string, PendingSend>,
  chatId: string,
  now: number,
): boolean {
  const p = pendingSends[chatId];
  return !!p && now - p.started > UNDELIVERED_GRACE_MS;
}

export { attachmentSnapshot };

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

type Set = (fn: (s: AppState) => Partial<AppState> | AppState) => void;
type Get = () => AppState;

function canvasSpaceDevice(s: AppState): string | undefined {
  const space = s.spaces.find((sp) => sp.id === s.canvas?.spaceId);
  return space?.deviceId;
}

function bumpAttachmentRev(set: Set) {
  set((s) => ({ attachmentRev: s.attachmentRev + 1 }));
}

function setStageProgress(set: Set, key: string, id: string, progress: number) {
  set((s) => ({
    staged: {
      ...s.staged,
      [key]: (s.staged[key] ?? []).map((a) =>
        a.id === id ? { ...a, progress } : a,
      ),
    },
  }));
}

function pushEcho(set: Set, chatId: string, entry: SessionMessageEntry) {
  set((s) => {
    const list = s.echoes[chatId] ?? [];
    if (list.some((e) => e.id === entry.id)) return {};
    return { echoes: { ...s.echoes, [chatId]: [...list, entry] } };
  });
}

function removeEcho(set: Set, chatId: string, messageId: string) {
  set((s) => {
    const list = s.echoes[chatId];
    if (!list?.some((e) => e.id === messageId)) return {};
    return {
      echoes: {
        ...s.echoes,
        [chatId]: list.filter((e) => e.id !== messageId),
      },
    };
  });
}

function dropPendingSend(
  pendingSends: Record<string, PendingSend>,
  chatId: string,
) {
  if (!pendingSends[chatId]) return pendingSends;
  const next = { ...pendingSends };
  delete next[chatId];
  return next;
}

/** After a transcript frame lands: drop echoes the doc now carries, and ack
 *  the pending send whose message id showed up (state.rs
 *  `ack_pending_send_from_transcript`). */
function reconcileEchoes(set: Set, chatId: string, entries: SessionMessageEntry[]) {
  set((s) => {
    const echoes = s.echoes[chatId];
    const pending = s.pendingSends[chatId];
    const nextEchoes = echoes?.filter((e) => !entries.some((x) => x.id === e.id));
    const ack = pending && entries.some((e) => e.id === pending.messageId);
    if ((!echoes || nextEchoes!.length === echoes.length) && !ack) return {};
    return {
      echoes:
        nextEchoes && nextEchoes.length !== echoes?.length
          ? { ...s.echoes, [chatId]: nextEchoes }
          : s.echoes,
      pendingSends: ack ? dropPendingSend(s.pendingSends, chatId) : s.pendingSends,
    };
  });
}

/** `update_chat_config` — apply `change` to the chat's effective config and
 *  persist it: optimistic row stamp + `Mutate setChatConfig`. The written row
 *  carries the CONCRETE resolved model/reasoning, re-clamped to the (possibly
 *  just-changed) model's ladder. */
function updateChatConfig(
  set: Set,
  get: Get,
  chatId: string,
  change: (config: ChatConfig) => void,
) {
  const s = get();
  const chat = s.chats.find((c) => c.id === chatId);
  const resolved = resolveRunConfig(s.resolveInput(chatId));
  const config = chatConfigFor(resolved, chat?.config);
  if (!config) return; // harness unknown — nothing safe to write
  change(config);
  // Reasoning must stay concrete for whatever model the row now names.
  const models = s.models[config.harness];
  if (models) {
    let ladder =
      models.find((m) => m.id === config.model)?.reasoningLevels ?? [];
    if (ladder.length === 0) {
      ladder =
        s.harnesses.find((d) => d.id === config.harness)?.reasoningLevels ?? [];
    }
    if (ladder.length > 0) {
      config.reasoning = clampReasoning(config.reasoning, ladder) ?? null;
    }
  }
  // Optimistic stamp; the workspace doc write reconciles it.
  set((st) => ({
    chats: st.chats.map((c) => (c.id === chatId ? { ...c, config } : c)),
  }));
  void rpc
    .mutate({ op: "setChatConfig", chatId, config })
    .catch((err) => console.warn("setChatConfig mutate failed", err));
}

function chatConfigFor(
  resolved: ReturnType<typeof resolveRunConfig>,
  existing: ChatConfig | null | undefined,
): ChatConfig | null {
  if (!resolved.harness) return null;
  return {
    harness: resolved.harness,
    model: resolved.model ?? null,
    reasoning: resolved.reasoning ?? null,
    modelOptions: resolved.modelOptions,
    // Preserve fields the pickers don't own.
    sandbox: existing?.sandbox ?? "workspace-write",
  };
}

// ---------------------------------------------------------------------------
// Subscription plumbing
// ---------------------------------------------------------------------------

function closeWorkspaceWatches() {
  for (const sub of workspaceSubs) sub.cancel();
  workspaceSubs = [];
  for (const subs of docSubs.values()) for (const sub of subs) sub.cancel();
  docSubs = new Map();
}

function openWorkspaceWatches(set: Set, get: Get) {
  closeWorkspaceWatches();
  const s = (h: Subscription) => workspaceSubs.push(h);

  s(
    rpc.watchSpaces({
      onItem: (spaces) => set(() => ({ spaces })),
      onError: () => {},
    }),
  );
  s(
    rpc.watchChats({
      onItem: (chats) => set(() => ({ chats })),
      onError: () => {},
    }),
  );
  s(
    rpc.watchSessions({
      onItem: (list) =>
        set(() => ({
          sessions: Object.fromEntries(list.map((x) => [x.chatId, x])),
        })),
      onError: () => {},
    }),
  );
  s(
    rpc.watchDevices({
      onItem: (list) =>
        set(() => ({
          devices: Object.fromEntries(list.map((x) => [x.id, x])),
        })),
      onError: () => {},
    }),
  );
  s(
    rpc.watchConnectivity({
      onItem: (connectivity) => set(() => ({ connectivity })),
      onError: () => {},
    }),
  );
  s(
    rpc.watchTransfers({
      onItem: (transfers) => set(() => ({ transfers })),
      onError: () => {},
    }),
  );

  void rpc
    .listHarnesses()
    .then((harnesses) => set(() => ({ harnesses })))
    .catch(() => {});
  void rpc
    .engineInfo()
    .then((engineInfo) => set(() => ({ engineInfo })))
    .catch(() => {});

  // Re-open doc streams for the selected chat (reconnect path).
  const selected = get().selectedChatId;
  if (selected) openDocWatches(selected, set, get);
}

function openDocWatches(chatId: string, set: Set, get: Get) {
  const existing = docSubs.get(chatId);
  if (existing) for (const sub of existing) sub.cancel();
  const subs: Subscription[] = [];
  docSubs.set(chatId, subs);

  subs.push(
    rpc.watchDocMessages(chatId, {
      onItem: (update) => applyTranscript(chatId, update, set, get),
      onError: (err) =>
        set((s) => ({
          transcriptErrors: {
            ...s.transcriptErrors,
            [chatId]: err.message,
          },
        })),
    }),
  );
  subs.push(
    rpc.watchQueue(chatId, {
      onItem: ({ items }) =>
        set((s) => ({ queues: { ...s.queues, [chatId]: items } })),
      onError: () => {},
    }),
  );
}

function applyTranscript(
  chatId: string,
  update: TranscriptUpdate,
  set: Set,
  get: Get,
) {
  const s = get();
  const prev = s.transcripts[chatId] ?? {
    entries: [],
    contextUsage: null,
    version: 0,
  };
  try {
    applyTranscriptUpdate(prev, update);
    set(() => ({
      transcripts: {
        ...s.transcripts,
        [chatId]: { ...prev, version: prev.version + 1 },
      },
      transcriptErrors: { ...s.transcriptErrors, [chatId]: null },
    }));
    reconcileEchoes(set, chatId, prev.entries);
  } catch (err) {
    if (err instanceof TranscriptDesync) {
      // Diverged copy — resubscribe for a fresh reset.
      openDocWatches(chatId, set, get);
      set(() => ({
        transcripts: {
          ...s.transcripts,
          [chatId]: { entries: [], contextUsage: null, version: 0 },
        },
        transcriptErrors: {
          ...s.transcriptErrors,
          [chatId]: `resyncing: ${err.message}`,
        },
      }));
    } else {
      set(() => ({
        transcriptErrors: { ...s.transcriptErrors, [chatId]: String(err) },
      }));
    }
  }
}

// Re-export the pickers helpers components consume.
export {
  effectiveHarness,
  effectiveModelId,
  effectiveReasoning,
  explicitOptions,
  isPendingRef,
  resolveRunConfig,
  selectedModel,
  traitLadder,
};
export type { DraftConfig, ResolveInput };
