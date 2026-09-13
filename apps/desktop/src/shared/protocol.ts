//! RPC method names — the subset of `zeron_rpc::methods` the desktop client
//! speaks, plus the engine capability flags it gates on. Names are the wire
//! contract; keep them in sync with crates/rpc/src/lib.rs.

export const methods = {
  EngineInfo: "EngineInfo",
  EngineReady: "EngineReady",
  LocalDevice: "LocalDevice",
  ListHarnesses: "ListHarnesses",
  ListModels: "ListModels",
  ListCommands: "ListCommands",
  WatchSpaces: "WatchSpaces",
  WatchChats: "WatchChats",
  WatchDevices: "WatchDevices",
  WatchSessions: "WatchSessions",
  WatchDocMessages: "WatchDocMessages",
  WatchQueue: "WatchQueue",
  WatchConnectivity: "WatchConnectivity",
  WatchTransfers: "WatchTransfers",
  GetSessionView: "GetSessionView",
  QueueCommand: "QueueCommand",
  QueueMessage: "QueueMessage",
  UpdateQueuedMessage: "UpdateQueuedMessage",
  RemoveQueuedMessage: "RemoveQueuedMessage",
  SendQueuedMessageNow: "SendQueuedMessageNow",
  SteerQueuedMessageNow: "SteerQueuedMessageNow",
  ListRefs: "ListRefs",
  SwitchRef: "SwitchRef",
  CreateWorktree: "CreateWorktree",
  UploadChunk: "UploadChunk",
  UploadCommit: "UploadCommit",
  ReadAttachmentChunk: "ReadAttachmentChunk",
  Mutate: "Mutate",
} as const;

export type MethodName = (typeof methods)[keyof typeof methods];

/// Engine capabilities the client checks before exposing queue/send features
/// (`EngineInfo.capabilities`, crates/engine `capabilities` module).
export const capabilities = {
  MessageQueueV1: "message-queue-v1",
  MessageQueueAttachmentsV1: "message-queue-attachments-v1",
  MessageQueueCleanAttachmentTextV1: "message-queue-clean-attachment-text-v1",
} as const;

// ---------------------------------------------------------------------------
// ndjson frames (zeron_rpc::{ClientFrame, ServerFrame})
// ---------------------------------------------------------------------------

export interface ClientFrame {
  id: number;
  method?: string;
  params?: unknown;
  cancel?: boolean;
}

export interface ServerFrame {
  id: number;
  ok?: unknown;
  err?: string;
  item?: unknown;
  done?: boolean;
}
