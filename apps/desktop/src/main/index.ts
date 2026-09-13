//! Noches Electron main — thin desktop shell. It owns the window, the engine
//! socket, and nothing else: workspace truth lives in the Rust engine.

import { app, BrowserWindow, session, shell } from "electron";
import { join } from "node:path";

import { EngineSocket } from "./engine";
import { registerIpc } from "./ipc";

// A dev-surface identity distinct from both shipped variants: its own userData
// dir, its own dock/taskbar name, and never a route to ~/.zeron.
app.setName("noches-electron-dev");

// Wayland: let Electron pick the platform instead of defaulting to X11.
if (process.env.XDG_SESSION_TYPE === "wayland") {
  app.commandLine.appendSwitch("ozone-platform-hint", "auto");
  app.commandLine.appendSwitch("enable-features", "WaylandWindowDecorations");
}

// A GPU-process crash is fatal to the whole app on some Linux/Wayland hosts,
// and the Chromium zygote sandbox fails outright on kernels/configs like this
// one (every neighboring Electron app on this machine launches with
// --no-sandbox for the same reason). Renderer `sandbox: true` — the boundary
// the ticket cares about — is a process-type constraint and stays on either
// way. Opt back out with NOCHES_OS_SANDBOX=1 / NOCHES_GPU=1.
if (process.platform === "linux") {
  if (process.env.NOCHES_OS_SANDBOX !== "1") {
    app.commandLine.appendSwitch("no-sandbox");
    app.commandLine.appendSwitch("no-zygote-sandbox");
  }
  if (process.env.NOCHES_GPU !== "1") {
    app.disableHardwareAcceleration();
  }
}

const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
}

const MAIN_DIR = __dirname;

const engine = new EngineSocket();
const ipc = registerIpc(engine);

function createWindow(): BrowserWindow {
  const win = new BrowserWindow({
    width: 1280,
    height: 840,
    minWidth: 720,
    minHeight: 480,
    show: false,
    autoHideMenuBar: true,
    backgroundColor: "#060606",
    title: "Noches (dev)",
    webPreferences: {
      preload: join(MAIN_DIR, "../preload/index.cjs"),
      // The renderer is untrusted web content by policy: no node, isolated
      // worlds, sandboxed, no insecure content, no popups.
      nodeIntegration: false,
      contextIsolation: true,
      sandbox: true,
      webSecurity: true,
      allowRunningInsecureContent: false,
      spellcheck: false,
    },
  });

  ipc.watchWebContents(win.webContents);

  win.once("ready-to-show", () => {
    console.log("[main] ready-to-show");
    win.show();
  });
  win.webContents.on("did-finish-load", () =>
    console.log("[main] did-finish-load"),
  );
  win.webContents.on("did-fail-load", (_e, code, desc) =>
    console.log(`[main] did-fail-load ${code} ${desc}`),
  );
  win.webContents.on("render-process-gone", (_e, details) =>
    console.log(`[main] render-process-gone ${JSON.stringify(details)}`),
  );
  win.webContents.on("unresponsive", () =>
    console.log("[main] renderer unresponsive"),
  );
  // If paint never completes (GPU-less Wayland quirks), don't hold the window
  // hostage — show it anyway after a beat.
  setTimeout(() => {
    if (!win.isDestroyed() && !win.isVisible()) {
      console.log("[main] ready-to-show timeout; forcing show");
      win.show();
    }
  }, 4000);

  // No new windows ever — links that want a browser get the user's browser.
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (url.startsWith("https://")) void shell.openExternal(url);
    return { action: "deny" };
  });

  // Navigation is pinned to the dev server (dev mode) or the bundled file
  // (packaged). Anything else — remote or file — is refused.
  const devUrl = process.env.ELECTRON_RENDERER_URL;
  win.webContents.on("will-navigate", (event, url) => {
    const allowed =
      (devUrl && url.startsWith(devUrl)) ||
      url.startsWith("file://" + join(MAIN_DIR, "../renderer"));
    if (!allowed) event.preventDefault();
  });

  if (devUrl) {
    void win.loadURL(devUrl);
  } else {
    void win.loadFile(join(MAIN_DIR, "../renderer/index.html"));
  }
  return win;
}

app.whenReady().then(() => {
  // Restrictive default headers on bundled loads; the dev server speaks for
  // itself. Deny every permission request outright.
  session.defaultSession.setPermissionRequestHandler((_wc, _perm, grant) => {
    grant(false);
  });
  session.defaultSession.setPermissionCheckHandler(() => false);

  engine.start();
  createWindow();

  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

app.on("second-instance", () => {
  const win = BrowserWindow.getAllWindows()[0];
  if (win) {
    if (win.isMinimized()) win.restore();
    win.focus();
  }
});

app.on("will-quit", () => engine.stop());
