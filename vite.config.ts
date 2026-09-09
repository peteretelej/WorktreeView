import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const host = process.env.TAURI_DEV_HOST;
// scripts/dev.mjs pins a free port for both vite and tauri's devUrl; a bare
// `npm run dev:web` defaults to 1420 unless WORKTREEVIEW_DEV_PORT says else.
const port = Number(process.env.WORKTREEVIEW_DEV_PORT) || 1420;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react()],

  // The highlight worker uses dynamic imports (grammars, wasm), which needs
  // ES module workers rather than the IIFE default.
  worker: {
    format: "es" as const,
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port,
    strictPort: true,
    host: host || false,
    allowedHosts: process.env.WORKTREEVIEW_DEV_HOST
      ? process.env.WORKTREEVIEW_DEV_HOST.split(",").map((h) => h.trim())
      : [],
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: port + 1,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
