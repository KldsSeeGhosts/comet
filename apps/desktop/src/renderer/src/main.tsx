import React from "react";
import { createRoot } from "react-dom/client";

import App from "./App";
import { useApp } from "./store";
import "./styles.css";

useApp.getState().boot();

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
