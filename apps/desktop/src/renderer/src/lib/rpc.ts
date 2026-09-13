//! Typed veneer over `window.noches` — the preload bridge. Everything the
//! renderer does against the engine funnels through here; there is no other
//! transport.

import { methods } from "../../../shared/protocol";
import type {
  Chat,
  Connectivity,
  Device,
  EngineInfo,
  HarnessDescriptor,
  HarnessId,
  Model,
  QueuedMessage,
  RepoRef,
  Session,
  SessionCommandPayload,
  SessionView,
  Space,
  TransferProgress,
  TranscriptUpdate,
  Worktree,
} from "../../../shared/types";

export function call<T = unknown>(method: string, params?: unknown): Promise<T> {
  return window.noches.call(method, params ?? null) as Promise<T>;
}

export interface StreamHandlers<T> {
  onItem: (item: T) => void;
  onDone?: () => void;
  onError?: (err: Error) => void;
}

export function subscribe<T = unknown>(
  method: string,
  params: unknown,
  handlers: StreamHandlers<T>,
): { id: string; cancel: () => void } {
  return window.noches.subscribe(method, params ?? null, {
    onItem: (item) => handlers.onItem(item as T),
    onDone: handlers.onDone,
    onError: handlers.onError,
  });
}

// -- typed calls -------------------------------------------------------------

export const engineInfo = () => call<EngineInfo>(methods.EngineInfo);
export const listHarnesses = () =>
  call<HarnessDescriptor[]>(methods.ListHarnesses);
export const listModels = (harness: HarnessId, targetDeviceId?: string) =>
  call<Model[]>(methods.ListModels, {
    harness,
    ...(targetDeviceId ? { targetDeviceId } : {}),
  });
export const getSessionView = (chatId: string) =>
  call<SessionView>(methods.GetSessionView, { chatId });
export const queueCommand = (chatId: string, command: SessionCommandPayload) =>
  call<{ commandId: string }>(methods.QueueCommand, { chatId, command });
/** QueueCommand with the full params object (transfers for queued
 *  attachments ride alongside the command). */
export const queueCommandRaw = (params: Record<string, unknown>) =>
  call<{ commandId: string }>(methods.QueueCommand, params);
export const queueMessage = (
  chatId: string,
  text: string,
  attachments: string[] = [],
) =>
  call<{ id: string }>(methods.QueueMessage, {
    chatId,
    text,
    attachments,
    holdForTurnEnd: true,
  });
export const listRefs = (repoPath: string, targetDeviceId?: string) =>
  call<RepoRef[]>(methods.ListRefs, {
    repoPath,
    ...(targetDeviceId ? { targetDeviceId } : {}),
  });
export const switchRef = (repoPath: string, refName: string) =>
  call<{ branch: string }>(methods.SwitchRef, { repoPath, refName });
export const createWorktree = (repoPath: string, branch: string) =>
  call<Worktree>(methods.CreateWorktree, { repoPath, branch });
export const steerQueuedNow = (chatId: string, id: string) =>
  call<{ sent: boolean }>(methods.SteerQueuedMessageNow, { chatId, id });
export const sendQueuedNow = (chatId: string, id: string) =>
  call<{ sent: boolean }>(methods.SendQueuedMessageNow, { chatId, id });
export const removeQueued = (chatId: string, id: string) =>
  call<{ removed: boolean }>(methods.RemoveQueuedMessage, { chatId, id });
export const mutate = (params: Record<string, unknown>) =>
  call<{ ok: boolean }>(methods.Mutate, params);
export const markChatSeen = (chatId: string) =>
  mutate({ op: "markChatSeen", chatId });

// -- typed streams -----------------------------------------------------------

export const watchSpaces = (h: StreamHandlers<Space[]>) =>
  subscribe<Space[]>(methods.WatchSpaces, null, h);
export const watchChats = (h: StreamHandlers<Chat[]>) =>
  subscribe<Chat[]>(methods.WatchChats, null, h);
export const watchDevices = (h: StreamHandlers<Device[]>) =>
  subscribe<Device[]>(methods.WatchDevices, null, h);
export const watchTransfers = (h: StreamHandlers<TransferProgress[]>) =>
  subscribe<TransferProgress[]>(methods.WatchTransfers, null, h);
export const watchSessions = (h: StreamHandlers<Session[]>) =>
  subscribe<Session[]>(methods.WatchSessions, null, h);
export const watchConnectivity = (h: StreamHandlers<Connectivity>) =>
  subscribe<Connectivity>(methods.WatchConnectivity, null, h);
export const watchDocMessages = (
  chatId: string,
  h: StreamHandlers<TranscriptUpdate>,
) => subscribe<TranscriptUpdate>(methods.WatchDocMessages, { chatId }, h);
export const watchQueue = (
  chatId: string,
  h: StreamHandlers<{ items: QueuedMessage[] }>,
) => subscribe<{ items: QueuedMessage[] }>(methods.WatchQueue, { chatId }, h);
