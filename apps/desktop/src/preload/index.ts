//! Preload — the entire renderer ⇄ main bridge, exposed as `window.noches`.
//!
//! The renderer is sandboxed: this file is the only code that can touch
//! `ipcRenderer`, and it exposes a narrow RPC client — no fs, no process, no
//! generic `invoke`. Subscription callbacks stay renderer-side; preload maps
//! them onto a client-minted id.

import { contextBridge, ipcRenderer, type IpcRendererEvent } from "electron";

export type EngineStatus =
  | { kind: "connecting" }
  | { kind: "ready"; info: unknown }
  | { kind: "reconnecting"; attempt: number; nextDelayMs: number; reason: string }
  | { kind: "failed"; error: string; detail?: string };

export interface SubscriptionHandlers {
  onItem: (item: unknown) => void;
  onDone?: () => void;
  onError?: (err: Error) => void;
}

export interface Subscription {
  id: string;
  cancel: () => void;
}

let nextClientSub = 1;

const api = {
  /** Unary RPC: resolves with the `ok` payload, rejects on `err`. */
  call(method: string, params?: unknown): Promise<unknown> {
    return ipcRenderer.invoke("rpc:call", method, params ?? null);
  },

  /** Streaming RPC: items/done/err arrive through `handlers`; `cancel()`
   *  sends the protocol-level `{id, cancel: true}` frame. */
  subscribe(
    method: string,
    params: unknown,
    handlers: SubscriptionHandlers,
  ): Subscription {
    const id = `sub-${nextClientSub++}`;
    const listener = (_e: IpcRendererEvent, msg: Record<string, unknown>) => {
      if (msg.id !== id) return;
      if ("item" in msg) handlers.onItem(msg.item);
      else if (msg.done) {
        handlers.onDone?.();
        off();
      } else if (typeof msg.err === "string") {
        handlers.onError?.(new Error(msg.err));
        off();
      }
    };
    const off = () => ipcRenderer.removeListener("rpc:event", listener);
    ipcRenderer.on("rpc:event", listener);
    void ipcRenderer.invoke("rpc:subscribe", id, method, params ?? null);
    return {
      id,
      cancel: () => {
        off();
        ipcRenderer.send("rpc:cancel", id);
      },
    };
  },

  /** Latest engine connection status (pull) — pair with `onStatus` (push). */
  getStatus(): Promise<EngineStatus> {
    return ipcRenderer.invoke("engine:get-status");
  },

  /** Push feed of engine status transitions. Returns an unsubscribe fn. */
  onStatus(cb: (status: EngineStatus) => void): () => void {
    const listener = (_e: IpcRendererEvent, status: EngineStatus) => cb(status);
    ipcRenderer.on("engine:status", listener);
    return () => ipcRenderer.removeListener("engine:status", listener);
  },

  /** Manual retry: re-dial now and allow another engine-spawn attempt. */
  retry(): Promise<void> {
    return ipcRenderer.invoke("engine:retry");
  },

  /** Native image picker for the composer: returns the picked files' bytes
   *  as base64 (the renderer is sandboxed and cannot read paths itself). */
  pickImages(): Promise<{ name: string; base64: string; size: number }[]> {
    return ipcRenderer.invoke("fs:pick-images");
  },
};

export type NochesBridge = typeof api;

contextBridge.exposeInMainWorld("noches", api);
