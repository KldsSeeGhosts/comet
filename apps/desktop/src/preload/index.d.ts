import type { NochesBridge } from "./index";

declare global {
  interface Window {
    noches: NochesBridge;
  }
}

export {};
