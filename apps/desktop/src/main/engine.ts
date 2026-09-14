//! EngineSocket — the renderer's only path to the Rust engine.
//!
//! The socket lives in the MAIN process for two reasons:
//!   1. The engine's WS acceptor rejects any handshake carrying an `Origin`
//!      header (crates/rpc/src/server.rs). Browser WebSocket clients — the
//!      renderer included — always send `Origin`, so a renderer-side dial can
//!      never connect. Node's `ws` sends none.
//!   2. Keeping it here means renderer reloads/HMR never drop the transport,
//!      and the sandboxed UI holds no socket of its own.
//!
//! Framing is ndjson: `{id, method, params}` out; `{id, ok|err|item|done}` in.

import { EventEmitter } from "node:events";
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import WebSocket from "ws";

import type { ClientFrame, ServerFrame } from "../shared/protocol";
import type { EngineInfo } from "../shared/types";

export type EngineStatus =
  | { kind: "connecting" }
  | { kind: "ready"; info: EngineInfo }
  | { kind: "reconnecting"; attempt: number; nextDelayMs: number; reason: string }
  | { kind: "failed"; error: string; detail?: string };

const DEV_IPC_PORT = 27655;
const PROD_IPC_PORT = 27654;
const DEFAULT_ENDPOINT = `ws://127.0.0.1:${DEV_IPC_PORT}`;

const MAX_BACKOFF_MS = 10_000;

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
  onItem?: (item: unknown) => void;
  onDone?: () => void;
}

function defaultEngineBin(): string {
  const fromEnv = process.env.NOCHES_DEV_ENGINE_BIN;
  if (fromEnv) return fromEnv;
  const installed = join(homedir(), ".local", "bin", "zeron-dev");
  if (existsSync(installed)) return installed;
  return "zeron-dev"; // PATH fallback
}

/**
 * The dev endpoint is a hard boundary: this client must never silently reach
 * the production engine. An override pointing at the prod IPC port is a
 * configuration error, not a connection to try.
 */
export function resolveEndpoint(): { url: string; error?: string } {
  const raw = process.env.NOCHES_ENGINE_WS ?? DEFAULT_ENDPOINT;
  try {
    const url = new URL(raw);
    if (url.port === String(PROD_IPC_PORT)) {
      return {
        url: raw,
        error:
          `refusing to connect to the production IPC port ${PROD_IPC_PORT} ` +
          `(NOCHES_ENGINE_WS=${raw}). The Electron client only talks to the dev engine.`,
      };
    }
  } catch {
    return { url: raw, error: `invalid NOCHES_ENGINE_WS: ${raw}` };
  }
  return { url: raw };
}

export class EngineSocket extends EventEmitter {
  private ws: WebSocket | null = null;
  private nextId = 1;
  private pending = new Map<number, PendingRequest>();
  private status: EngineStatus = { kind: "connecting" };
  private reconnectTimer: NodeJS.Timeout | null = null;
  private attempt = 0;
  private stopped = false;
  private spawned: ChildProcess | null = null;
  private spawnAttempted = false;
  readonly endpoint: string;
  readonly endpointError?: string;

  constructor() {
    super();
    const { url, error } = resolveEndpoint();
    this.endpoint = url;
    this.endpointError = error;
  }

  getStatus(): EngineStatus {
    return this.status;
  }

  private setStatus(status: EngineStatus) {
    this.status = status;
    console.log("[engine] status:", JSON.stringify(status));
    this.emit("status", status);
  }

  /** Begin (or restart) the connect loop. Idempotent. */
  start() {
    this.stopped = false;
    if (this.endpointError) {
      this.setStatus({ kind: "failed", error: this.endpointError });
      return;
    }
    this.connect();
  }

  stop() {
    this.stopped = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.ws?.close();
    this.failAll(new Error("engine socket stopped"));
  }

  /** Manual "try again": re-dials now and, if the port is still dead, allows
   *  one more engine spawn attempt. */
  retry() {
    this.attempt = 0;
    this.spawnAttempted = false;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.ws?.removeAllListeners();
    this.ws?.close();
    this.ws = null;
    this.start();
  }

  private connect() {
    if (this.stopped) return;
    if (this.status.kind !== "reconnecting") {
      this.setStatus({ kind: "connecting" });
    }
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.endpoint, {
        // No Origin header — the engine refuses cross-origin browser dials;
        // a main-process client is a native viewport.
        headers: { "sec-websocket-protocol": "noches-rpc" },
        handshakeTimeout: 5000,
      });
    } catch (err) {
      this.scheduleReconnect(`dial failed: ${String(err)}`);
      return;
    }
    this.ws = ws;

    ws.on("open", () => {
      console.log("[engine] ws open, waiting for EngineReady");
      // EngineReady is the readiness barrier — the socket is only "ready"
      // once stores and journals are assembled.
      this.call("EngineReady", null)
        .then(() => this.call("EngineInfo", null))
        .then((info) => {
          this.attempt = 0;
          this.setStatus({ kind: "ready", info: info as EngineInfo });
        })
        .catch((err) => {
          this.setStatus({
            kind: "failed",
            error: `engine not ready: ${err instanceof Error ? err.message : String(err)}`,
          });
          ws.close();
        });
    });

    // The server writes one frame per WS text message (no \n terminator) —
    // the message boundary is the delimiter, not a byte scan.
    ws.on("message", (data) => {
      for (const line of data.toString().split("\n")) {
        const trimmed = line.trim();
        if (trimmed) this.dispatch(trimmed);
      }
    });

    ws.on("close", () => {
      this.failAll(new Error("engine connection closed"));
      if (!this.stopped) this.scheduleReconnect("connection closed");
    });
    ws.on("error", (err) => {
      console.log("[engine] ws error:", err.message);
      // 'error' is always followed by 'close'; the close handler schedules
      // the retry. First failure on a dead port may mean no engine — offer a
      // spawn attempt.
      if (this.attempt === 0) this.maybeSpawnEngine(err);
    });
  }

  private scheduleReconnect(reason: string) {
    if (this.stopped) return;
    this.attempt += 1;
    const delay = Math.min(1000 * 2 ** Math.min(this.attempt - 1, 4), MAX_BACKOFF_MS);
    this.setStatus({
      kind: "reconnecting",
      attempt: this.attempt,
      nextDelayMs: delay,
      reason,
    });
    this.reconnectTimer = setTimeout(() => this.connect(), delay);
  }

  /** Dead port on the very first dial → start `zeron-dev headless` once. */
  private maybeSpawnEngine(_err: Error) {
    if (this.spawnAttempted) return;
    if (process.env.NOCHES_ENGINE_SPAWN === "0") return;
    this.spawnAttempted = true;
    const bin = defaultEngineBin();
    try {
      this.spawned = spawn(bin, ["headless"], {
        detached: true,
        stdio: "ignore",
        env: { ...process.env },
      });
      this.spawned.on("error", () => {
        // Binary missing or not executable — the reconnect loop keeps the
        // honest "reconnecting" state on screen either way.
      });
      this.spawned.unref();
    } catch {
      // No spawn possible; retry loop continues.
    }
  }

  private dispatch(line: string) {
    let frame: ServerFrame;
    try {
      frame = JSON.parse(line) as ServerFrame;
    } catch {
      return;
    }
    const pending = this.pending.get(frame.id);
    if (!pending) return;
    if (frame.ok !== undefined) {
      this.pending.delete(frame.id);
      pending.resolve(frame.ok);
    } else if (frame.err !== undefined) {
      this.pending.delete(frame.id);
      pending.reject(new Error(frame.err));
    } else if (frame.item !== undefined) {
      pending.onItem?.(frame.item);
    } else if (frame.done) {
      this.pending.delete(frame.id);
      pending.onDone?.();
      pending.resolve(undefined);
    }
  }

  private send(frame: ClientFrame) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      throw new Error("engine not connected");
    }
    this.ws.send(JSON.stringify(frame));
  }

  call(method: string, params: unknown): Promise<unknown> {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      try {
        this.send({ id, method, params: params ?? null });
      } catch (err) {
        this.pending.delete(id);
        reject(err);
      }
    });
  }

  /** A streaming call: resolves with `id` for later cancel; items arrive via
   *  the callbacks. Resolving does not end the stream — `done`/`err` do. */
  subscribe(
    method: string,
    params: unknown,
    onItem: (item: unknown) => void,
    onDone: () => void,
    onError: (err: Error) => void,
  ): number {
    const id = this.nextId++;
    this.pending.set(id, {
      resolve: () => {},
      reject: (err) => onError(err),
      onItem,
      onDone,
    });
    try {
      this.send({ id, method, params: params ?? null });
    } catch (err) {
      this.pending.delete(id);
      onError(err as Error);
      return -1;
    }
    return id;
  }

  cancel(engineFrameId: number) {
    if (engineFrameId < 0) return;
    this.pending.delete(engineFrameId);
    try {
      this.send({ id: engineFrameId, cancel: true });
    } catch {
      // Socket already gone; the close path fails the request anyway.
    }
  }

  private failAll(err: Error) {
    for (const [, p] of this.pending) p.reject(err);
    this.pending.clear();
  }
}
