import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig, externalizeDepsPlugin } from "electron-vite";
import { resolve } from "node:path";

// Shells spawned from editor extension hosts can inherit
// ELECTRON_RUN_AS_NODE=1, which makes the Electron binary boot as plain Node
// (no `electron` builtin, no window). Strip it before electron-vite spawns
// the app.
delete process.env.ELECTRON_RUN_AS_NODE;

// This dev shell's kernel/sandbox setup breaks Chromium's zygote (its spawn
// fails, taking the GPU process and the whole app down). The runtime flag in
// main can't reach early-init, so the documented env var does it here —
// renderer `sandbox: true` is unaffected (it's a process-type boundary, not
// the OS sandbox). Set NOCHES_OS_SANDBOX=1 to keep Chromium's OS sandbox.
if (process.env.NOCHES_OS_SANDBOX !== "1") {
  process.env.ELECTRON_DISABLE_SANDBOX = "1";
}

export default defineConfig({
  main: {
    plugins: [externalizeDepsPlugin()],
    build: {
      outDir: "out/main",
      rollupOptions: {
        input: { index: resolve(__dirname, "src/main/index.ts") },
        output: {
          // Electron's `electron` builtin has no usable ESM named exports on
          // this stack; CJS main is the reliable shape.
          format: "cjs",
          entryFileNames: "[name].cjs",
        },
      },
    },
  },
  preload: {
    plugins: [externalizeDepsPlugin()],
    build: {
      outDir: "out/preload",
      rollupOptions: {
        input: { index: resolve(__dirname, "src/preload/index.ts") },
        output: {
          // Sandboxed renderers only accept CJS preloads; the package is
          // type:module, so pin the emitted format and extension here.
          format: "cjs",
          entryFileNames: "[name].cjs",
        },
      },
    },
  },
  renderer: {
    root: resolve(__dirname, "src/renderer"),
    plugins: [react(), tailwindcss()],
    build: {
      outDir: resolve(__dirname, "out/renderer"),
      rollupOptions: {
        input: { index: resolve(__dirname, "src/renderer/index.html") },
      },
    },
    server: {
      // Renderer dev server stays loopback-only; the engine socket lives in
      // the main process, so this port carries no privileged surface.
      host: "127.0.0.1",
      port: 5199,
      strictPort: true,
    },
  },
});
