//! IPC surface between the sandboxed renderer and the main-process engine
//! socket. The bridge is deliberately narrow: generic RPC + subscription
//! forwarding, engine status, and a retry/ensure trigger. No filesystem,
//! no process, no shell.

import { BrowserWindow, dialog, ipcMain, type WebContents } from "electron";
import { readFile, stat } from "node:fs/promises";
import { basename } from "node:path";
import type { EngineSocket, EngineStatus } from "./engine";
import { MAX_ATTACHMENT_BYTES } from "../shared/attachments";

const CH = {
  call: "rpc:call",
  subscribe: "rpc:subscribe",
  cancel: "rpc:cancel",
  event: "rpc:event",
  status: "engine:status",
  getStatus: "engine:get-status",
  retry: "engine:retry",
  pickImages: "fs:pick-images",
} as const;

export function registerIpc(engine: EngineSocket) {
  // clientSubId → engine frame id, scoped per webContents so a reload or a
  // destroyed view can never cancel another view's streams.
  const subs = new Map<WebContents, Map<string, number>>();

  const dropSubscriptions = (wc: WebContents) => {
    const owned = subs.get(wc);
    if (!owned) return;
    for (const frameId of owned.values()) engine.cancel(frameId);
    subs.delete(wc);
  };

  ipcMain.handle(CH.call, (_e, method: string, params: unknown) =>
    engine.call(method, params),
  );

  ipcMain.handle(
    CH.subscribe,
    (e, clientId: string, method: string, params: unknown) => {
      const wc = e.sender;
      let owned = subs.get(wc);
      if (!owned) {
        owned = new Map();
        subs.set(wc, owned);
      }
      const post = (msg: Record<string, unknown>) => {
        if (!wc.isDestroyed()) wc.send(CH.event, { id: clientId, ...msg });
      };
      const frameId = engine.subscribe(
        method,
        params,
        (item) => post({ item }),
        () => {
          post({ done: true });
          owned.delete(clientId);
        },
        (err) => {
          post({ err: err.message });
          owned.delete(clientId);
        },
      );
      owned.set(clientId, frameId);
      return frameId;
    },
  );

  ipcMain.on(CH.cancel, (e, clientId: string) => {
    const frameId = subs.get(e.sender)?.get(clientId);
    if (frameId !== undefined) {
      engine.cancel(frameId);
      subs.get(e.sender)?.delete(clientId);
    }
  });

  ipcMain.handle(CH.getStatus, () => engine.getStatus());
  ipcMain.handle(CH.retry, () => engine.retry());

  // The composer's image picker is the one sanctioned filesystem touch: a
  // native open dialog + read into base64, extension-filtered to the engine's
  // image jail. The renderer stays sandboxed — it never sees a path it can
  // open itself.
  ipcMain.handle(CH.pickImages, async (e) => {
    const win = BrowserWindow.fromWebContents(e.sender);
    const opts = {
      properties: ["openFile", "multiSelections"] as (
        | "openFile"
        | "multiSelections"
      )[],
      filters: [
        {
          name: "Images",
          extensions: [
            "png",
            "jpg",
            "jpeg",
            "gif",
            "webp",
            "svg",
            "bmp",
            "tif",
            "tiff",
            "avif",
            "heic",
          ],
        },
      ],
    };
    const result = win
      ? await dialog.showOpenDialog(win, opts)
      : await dialog.showOpenDialog(opts);
    if (result.canceled) return [];
    const files: { name: string; base64: string; size: number }[] = [];
    for (const path of result.filePaths) {
      const meta = await stat(path).catch(() => null);
      if (!meta?.isFile() || meta.size > MAX_ATTACHMENT_BYTES) continue;
      const buf = await readFile(path).catch(() => null);
      if (!buf) continue;
      files.push({
        name: basename(path),
        base64: buf.toString("base64"),
        size: buf.length,
      });
    }
    return files;
  });

  engine.on("status", (status: EngineStatus) => {
    for (const wc of webContentsAll()) {
      if (!wc.isDestroyed()) wc.send(CH.status, status);
    }
  });

  return {
    // A renderer reload abandons every subscription it held — runs continue
    // in the engine; the fresh page re-subscribes from a reset frame.
    watchWebContents(wc: WebContents) {
      liveContents.add(wc);
      wc.on("did-start-navigation", () => dropSubscriptions(wc));
      wc.on("destroyed", () => {
        dropSubscriptions(wc);
        liveContents.delete(wc);
      });
    },
  };
}

// Live renderers that receive engine status broadcasts.
const liveContents = new Set<WebContents>();
function webContentsAll(): Set<WebContents> {
  return liveContents;
}
